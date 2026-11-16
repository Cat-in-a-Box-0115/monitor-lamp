// Monitor brightness over DDC/CI (VCP code 0x10), for every monitor that supports it. DDC is slow (tens of ms
// per call), so one thread does it: a read at start and every minute, and the latest requested level when the
// knob or the popup moves it.

use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;
use windows_sys::Win32::Devices::Display::{
    DestroyPhysicalMonitors, GetNumberOfPhysicalMonitorsFromHMONITOR, GetPhysicalMonitorsFromHMONITOR,
    GetVCPFeatureAndVCPFeatureReply, SetVCPFeature, PHYSICAL_MONITOR,
};
use windows_sys::Win32::Foundation::{BOOL, LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR};

const VCP_BRIGHTNESS: u8 = 0x10;

unsafe extern "system" fn each(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
    (*(data as *mut Vec<HMONITOR>)).push(h);
    1
}

/// Runs `f` on every physical monitor, then releases them
fn with_monitors(mut f: impl FnMut(PHYSICAL_MONITOR)) {
    unsafe {
        let mut handles: Vec<HMONITOR> = Vec::new();
        EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(each), &mut handles as *mut _ as LPARAM);
        for h in handles {
            let mut n = 0u32;
            if GetNumberOfPhysicalMonitorsFromHMONITOR(h, &mut n) == 0 || n == 0 {
                continue;
            }
            let mut mons: Vec<PHYSICAL_MONITOR> = vec![std::mem::zeroed(); n as usize];
            if GetPhysicalMonitorsFromHMONITOR(h, n, mons.as_mut_ptr()) == 0 {
                continue;
            }
            for m in &mons {
                f(*m);
            }
            DestroyPhysicalMonitors(n, mons.as_ptr());
        }
    }
}

/// (current, maximum) brightness of one monitor, if it answers DDC/CI
fn get(m: PHYSICAL_MONITOR) -> Option<(u32, u32)> {
    let (mut cur, mut max) = (0u32, 0u32);
    let ok = unsafe { GetVCPFeatureAndVCPFeatureReply(m.hPhysicalMonitor, VCP_BRIGHTNESS, std::ptr::null_mut(), &mut cur, &mut max) };
    (ok != 0 && max > 0).then_some((cur, max))
}

/// The first DDC monitor's brightness in %, or None when no monitor takes DDC/CI
fn read() -> Option<u8> {
    let mut out = None;
    with_monitors(|m| {
        if out.is_none() {
            out = get(m).map(|(c, mx)| ((c * 100 + mx / 2) / mx).min(100) as u8);
        }
    });
    out
}

/// Sets every DDC monitor; true if at least one took it
fn set(pct: u8) -> bool {
    let mut any = false;
    with_monitors(|m| {
        if let Some((_, max)) = get(m) {
            any |= unsafe { SetVCPFeature(m.hPhysicalMonitor, VCP_BRIGHTNESS, (pct.min(100) as u32 * max + 50) / 100) } != 0;
        }
    });
    any
}

/// Starts the monitor thread; send levels (0-100) to the returned sender. `on_change` gets the level at start,
/// after every change, and when someone changes it on the monitor itself.
pub fn start(on_change: impl Fn(Option<u8>) + Send + 'static) -> Sender<u8> {
    let (tx, rx) = mpsc::channel::<u8>();
    thread::spawn(move || {
        let mut last = read();
        crate::winutil::log(&format!("monitor: {}", last.map_or("no DDC/CI brightness".into(), |b| format!("brightness {b}%"))));
        on_change(last);
        loop {
            let now = match rx.recv_timeout(Duration::from_secs(60)) {
                Ok(mut pct) => {
                    // Only the latest level matters
                    while let Ok(p) = rx.try_recv() {
                        pct = p;
                    }
                    // A monitor that refused (some picture modes lock brightness) reports what it really has
                    if set(pct) { Some(pct) } else { read() }
                }
                Err(RecvTimeoutError::Timeout) => read(),
                Err(RecvTimeoutError::Disconnected) => return,
            };
            if now != last {
                last = now;
                on_change(now);
            }
        }
    });
    tx
}
