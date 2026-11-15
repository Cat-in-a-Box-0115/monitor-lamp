// The knob over USB serial: finds it by VID/PID, keeps one session open, reconnects when it's replugged.
// Message rules are in docs/knob-protocol.md; the session logic is in detent::link (tested on any OS).

use detent::link::{Session, Snapshot};
use detent::protocol::{Msg, PID, VID};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub enum Out {
    Msg(Msg),
    /// (art_id, w, h, RGB565): sent only if the knob doesn't have this cover yet
    Cover(u32, u16, u16, Vec<u8>),
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
    // Drop anything queued while disconnected: the snapshot already has the latest state
    while rx.try_recv().is_ok() {}
    let current = snap.lock().unwrap().clone();
    let mut s = Session::new(port, &current)?;
    loop {
        match rx.recv_timeout(Duration::from_millis(40)) {
            Ok(Out::Msg(m)) => s.send(&m)?,
            Ok(Out::Cover(id, w, h, px)) => s.send_cover(id, w, h, &px)?,
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
            match serialport::new(&name, 115_200).timeout(Duration::from_millis(20)).open() {
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
        while rx.try_recv().is_ok() {}
    });
    tx
}
