// Shutdown, sign-out, sleep and lock notifications.
//
// A hidden top-level window on its own thread receives them, independent of the
// UI. Windows may end the process as soon as WM_ENDSESSION returns, so the
// handler must do its work (sending "off") before returning.

use crate::winutil::wide;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use std::sync::Mutex;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::RemoteDesktop::{WTSRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW, TranslateMessage, MSG,
    PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, PBT_APMSUSPEND, WM_ENDSESSION, WM_POWERBROADCAST,
    WM_QUERYENDSESSION, WM_WTSSESSION_CHANGE, WNDCLASSW, WS_OVERLAPPED,
};

const WTS_SESSION_LOCK: usize = 7;
const WTS_SESSION_UNLOCK: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event {
    SessionEnd,
    Suspend,
    Resume,
    Lock,
    Unlock,
}

type Handler = Box<dyn Fn(Event) + Send + Sync>;
static HANDLER: OnceLock<Handler> = OnceLock::new();
static LAST_RESUME: Mutex<Option<Instant>> = Mutex::new(None);

pub fn start(handler: impl Fn(Event) + Send + Sync + 'static) {
    let _ = HANDLER.set(Box::new(handler));
    std::thread::spawn(|| unsafe {
        let class = wide("MonitorLampHooks");
        let hinst = GetModuleHandleW(std::ptr::null());
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = hinst;
        wc.lpszClassName = class.as_ptr();
        RegisterClassW(&wc);
        // A real (hidden) top-level window: message-only windows don't get
        // WM_ENDSESSION or power broadcasts.
        let title = wide("Detent hooks");
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        );
        WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    });
}

fn fire(e: Event) {
    if let Some(h) = HANDLER.get() {
        h(e);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_QUERYENDSESSION => return 1,
        WM_ENDSESSION => {
            if wparam != 0 {
                fire(Event::SessionEnd);
            }
            return 0;
        }
        WM_POWERBROADCAST => {
            match wparam as u32 {
                PBT_APMSUSPEND => fire(Event::Suspend),
                // Both can arrive for one wake-up; act on the first
                PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => {
                    let mut last = LAST_RESUME.lock().unwrap();
                    if last.map_or(true, |t| t.elapsed() > Duration::from_secs(10)) {
                        *last = Some(Instant::now());
                        drop(last);
                        fire(Event::Resume);
                    }
                }
                _ => {}
            }
            return 1;
        }
        WM_WTSSESSION_CHANGE => {
            match wparam {
                WTS_SESSION_LOCK => fire(Event::Lock),
                WTS_SESSION_UNLOCK => fire(Event::Unlock),
                _ => {}
            }
            return 0;
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}
