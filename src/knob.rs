// The knob over USB serial: finds it by VID/PID, keeps one session open, reconnects when it's replugged.
// Message rules are in docs/knob-protocol.md; the session logic is in detent::link (tested on any OS).

use detent::link::{FwTimeouts, Session, Snapshot};
use detent::protocol::{Msg, PID, VID};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub enum Out {
    Msg(Msg),
    /// (art_id, w, h, RGB565): sent only if the knob doesn't have this cover yet
    Cover(u32, u16, u16, Vec<u8>),
    /// A new knob firmware image and its version; `report` follows the update
    Firmware { image: Vec<u8>, version: String, report: Box<dyn Fn(FwEvent) + Send> },
}

pub enum FwEvent {
    Progress(usize, usize),
    Done,
    /// `unconfirmed`: the knob got the whole image but its last answer never came
    Failed { text: String, unconfirmed: bool },
}

/// The serial port as the session sees it. Reads return at once when nothing is waiting (the link loop paces
/// itself); writes may take up to the port's 2 s timeout, enough for a whole cover and short enough to notice a
/// knob that stopped reading. No FlushFileBuffers: Windows doesn't time it out.
struct Port(Box<dyn serialport::SerialPort>);

impl std::io::Read for Port {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let waiting = self.0.bytes_to_read().map_err(std::io::Error::from)?;
        if waiting == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "nothing waiting"));
        }
        let n = buf.len().min(waiting as usize);
        self.0.read(&mut buf[..n])
    }
}

impl std::io::Write for Port {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Throws away queued updates (the snapshot already has the latest state); a queued firmware update is
/// answered as failed rather than dropped silently
fn drain(rx: &Receiver<Out>) {
    while let Ok(o) = rx.try_recv() {
        if let Out::Firmware { report, .. } = o {
            report(FwEvent::Failed { text: "the knob disconnected".into(), unconfirmed: false });
        }
    }
}

fn find_port() -> Option<String> {
    // For testing with a virtual COM port (tools/fake_knob.py), which has no VID/PID
    if let Some(p) = std::env::var_os("DETENT_KNOB_PORT") {
        return Some(p.to_string_lossy().into_owned());
    }
    serialport::available_ports().ok()?.into_iter().find_map(|p| match p.port_type {
        serialport::SerialPortType::UsbPort(u) if u.vid == VID && u.pid == PID => Some(p.port_name),
        _ => None,
    })
}

fn serve(port: Box<dyn serialport::SerialPort>, snap: &Arc<Mutex<Snapshot>>, rx: &Receiver<Out>, on_msg: &(dyn Fn(Msg) + Send)) -> std::io::Result<()> {
    drain(rx);
    let current = snap.lock().unwrap().clone();
    let mut s = Session::new(Port(port), &current)?;
    loop {
        match rx.recv_timeout(Duration::from_millis(40)) {
            Ok(Out::Msg(m)) => s.send(&m)?,
            Ok(Out::Cover(id, w, h, px)) => s.send_cover(id, w, h, &px)?,
            Ok(Out::Firmware { image, version, report }) => {
                crate::winutil::log(&format!("knob: updating firmware to {version} ({} bytes)", image.len()));
                let r = s.update_firmware(&image, &version, FwTimeouts::default(), &mut |a, b| report(FwEvent::Progress(a, b)), &mut |m| on_msg(m));
                crate::winutil::log(&format!("knob: firmware update {}", r.as_ref().map_or_else(|e| format!("failed: {}", e.text), |_| "done".into())));
                // Either way, start a fresh session: the knob reboots after an update, and after a failure
                // the link may be mid-frame
                match r {
                    Ok(()) => report(FwEvent::Done),
                    Err(e) => report(FwEvent::Failed { text: e.text, unconfirmed: e.unconfirmed }),
                }
                return Ok(());
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
        for m in s.poll()? {
            on_msg(m);
        }
    }
}

/// Starts the link thread. Send live updates through the returned sender; keep `snap` current so a
/// reconnect can send everything again. `on_conn(true/false)` reports the knob coming and going.
pub fn start(
    snap: Arc<Mutex<Snapshot>>,
    on_msg: impl Fn(Msg) + Send + 'static,
    on_conn: impl Fn(bool) + Send + 'static,
) -> Sender<Out> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || loop {
        if let Some(name) = find_port() {
            match serialport::new(&name, 115_200).timeout(Duration::from_secs(2)).open() {
                Ok(mut port) => {
                    // TinyUSB's CDC only sends once the host raises DTR
                    let _ = port.write_data_terminal_ready(true);
                    crate::winutil::log(&format!("knob: connected on {name}"));
                    on_conn(true);
                    let r = serve(port, &snap, &rx, &on_msg);
                    crate::winutil::log(&format!("knob: disconnected ({})", r.err().map(|e| e.to_string()).unwrap_or_default()));
                    on_conn(false);
                }
                Err(e) => crate::winutil::log(&format!("knob: can't open {name}: {e}")),
            }
        }
        // Not connected: forget queued updates and look again in 2 s
        thread::sleep(Duration::from_secs(2));
        drain(&rx);
    });
    tx
}
