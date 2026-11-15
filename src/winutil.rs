// Small Windows helpers: log file, local time, start with Windows, single
// instance, window styling.

use std::ffi::c_void;
use std::io::Write;
use std::path::PathBuf;
use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetWindowRect, SetWindowPos, SIZE_RESTORED, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, WINDOWPOS, WM_DPICHANGED,
    WM_GETDPISCALEDSIZE, WM_SIZE, WM_WINDOWPOSCHANGING,
};
use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_CAPTION_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows_sys::Win32::System::Registry::{
    RegDeleteKeyValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ,
};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::System::Threading::{CreateEventW, CreateMutexW, SetEvent, WaitForSingleObject, INFINITE};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn local_hms() -> (u16, u16, u16) {
    let st = unsafe {
        let mut st = std::mem::zeroed();
        GetLocalTime(&mut st);
        st
    };
    (st.wHour, st.wMinute, st.wSecond)
}

fn timestamp() -> String {
    let st = unsafe {
        let mut st = std::mem::zeroed();
        GetLocalTime(&mut st);
        st
    };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond)
}

// --- log -----------------------------------------------------------------

pub fn log_file() -> PathBuf {
    crate::settings::dir().join("detent.log")
}

/// %APPDATA%\Detent\detent.log — written synchronously so lines
/// survive the process being killed at sign-out. If that file can't be
/// written, lines (and the reason) go to %TEMP%\detent.log instead.
pub fn log(msg: &str) {
    let line = format!("{} [{}] {}\n", timestamp(), std::process::id(), msg);
    let append = |path: &PathBuf, text: &str| -> std::io::Result<()> {
        std::fs::OpenOptions::new().create(true).append(true).open(path)?.write_all(text.as_bytes())
    };
    let main = log_file();
    if let Err(e) = std::fs::create_dir_all(crate::settings::dir()).and_then(|_| append(&main, &line)) {
        let fallback = std::env::temp_dir().join("detent.log");
        let _ = append(&fallback, &format!("{}  (main log failed: {e})\n", line.trim_end()));
    }
}

pub fn rotate_log() {
    let f = log_file();
    if std::fs::metadata(&f).map(|m| m.len() > 512 * 1024).unwrap_or(false) {
        let _ = std::fs::rename(&f, f.with_extension("log.old"));
    }
}

pub fn open_path(path: &std::path::Path) {
    let p = wide(&path.to_string_lossy());
    let op = wide("open");
    unsafe {
        ShellExecuteW(std::ptr::null_mut(), op.as_ptr(), p.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL);
    }
}

// --- start with Windows ----------------------------------------------------

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_NAME: &str = "Detent";

pub fn set_autostart(enabled: bool) {
    let key = wide(RUN_KEY);
    let name = wide(RUN_NAME);
    unsafe {
        if enabled {
            let exe = std::env::current_exe().unwrap_or_default();
            let cmd = wide(&format!("\"{}\" --hidden", exe.display()));
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                REG_SZ,
                cmd.as_ptr() as *const c_void,
                (cmd.len() * 2) as u32,
            );
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr());
        }
    }
}

// --- single instance -------------------------------------------------------

/// Returns false if another copy is running (after asking it to show its window).
/// `on_show` runs on a background thread whenever a second copy is launched.
pub fn single_instance(on_show: impl Fn() + Send + 'static) -> bool {
    let mutex = wide("Local\\MonitorLamp.SingleInstance");
    let event = wide("Local\\MonitorLamp.Show");
    unsafe {
        CreateMutexW(std::ptr::null(), 0, mutex.as_ptr()); // kept for the life of the process
        let already = GetLastError() == ERROR_ALREADY_EXISTS;
        let ev = CreateEventW(std::ptr::null(), 0, 0, event.as_ptr());
        if already {
            SetEvent(ev);
            return false;
        }
        let ev = ev as isize;
        std::thread::spawn(move || loop {
            WaitForSingleObject(ev as _, INFINITE);
            on_show();
        });
    }
    true
}

// --- the main window -------------------------------------------------------------

/// This process's visible top-level window with the given title. (Slint's
/// window handle isn't available with the software renderer.)
pub fn find_own_window(title: &str) -> Option<HWND> {
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible};
    struct Search {
        pid: u32,
        title: Vec<u16>,
        found: HWND,
    }
    unsafe extern "system" fn each(hwnd: HWND, lparam: LPARAM) -> i32 {
        let s = &mut *(lparam as *mut Search);
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid != s.pid || IsWindowVisible(hwnd) == 0 {
            return 1;
        }
        let mut buf = [0u16; 128];
        let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) as usize;
        if buf[..n] == s.title[..] {
            s.found = hwnd;
            return 0;
        }
        1
    }
    let mut s = Search { pid: unsafe { GetCurrentProcessId() }, title: title.encode_utf16().collect(), found: std::ptr::null_mut() };
    unsafe { EnumWindows(Some(each), &mut s as *mut Search as LPARAM) };
    (!s.found.is_null()).then_some(s.found)
}

// --- moving between screens with different scaling ----------------------------

/// Dragging the window between screens with different scaling made it grow or
/// shrink. At the edge between two screens the scale flips back and forth
/// several times a second, and each time both Windows' suggested size and the
/// UI library's own resize can be based on an already-wrong size, so errors
/// pile up.
///
/// Instead, the window's size is kept in scale-free units (measured whenever
/// it is resized outside a scale change, e.g. by the user), and every scale
/// change sets the size from that alone. For a moment after each change any
/// other resize is overridden with it.
pub fn fix_dpi_resizing(hwnd: HWND) {
    use std::sync::atomic::{AtomicIsize, Ordering};
    static DONE: AtomicIsize = AtomicIsize::new(0);
    if DONE.swap(hwnd as isize, Ordering::Relaxed) == hwnd as isize {
        return;
    }
    remember_size(hwnd);
    if unsafe { SetWindowSubclass(hwnd, Some(dpi_proc), 1, 0) } == 0 {
        log("couldn't install the fix for moving between screens");
    }
}

/// Window size in 96-dpi units
static UNSCALED: std::sync::Mutex<Option<(f64, f64)>> = std::sync::Mutex::new(None);
/// Size to hold the window at, and until when (set on each scale change)
static HOLD_SIZE: std::sync::Mutex<Option<(i32, i32, std::time::Instant)>> = std::sync::Mutex::new(None);

fn remember_size(hwnd: HWND) {
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    let dpi = unsafe {
        GetWindowRect(hwnd, &mut r);
        GetDpiForWindow(hwnd)
    };
    if dpi == 0 || r.right <= r.left {
        return;
    }
    let k = 96.0 / dpi as f64;
    *UNSCALED.lock().unwrap() = Some(((r.right - r.left) as f64 * k, (r.bottom - r.top) as f64 * k));
}

fn size_for(dpi: u32) -> Option<(i32, i32)> {
    let (w, h) = (*UNSCALED.lock().unwrap())?;
    let k = dpi as f64 / 96.0;
    Some(((w * k).round() as i32, (h * k).round() as i32))
}

fn holding() -> Option<(i32, i32)> {
    let mut held = HOLD_SIZE.lock().unwrap();
    match *held {
        Some((w, h, until)) if std::time::Instant::now() <= until => Some((w, h)),
        Some(_) => {
            *held = None;
            None
        }
        None => None,
    }
}

/// Logs scale-change handling when LAMP_DPI_DEBUG is set
fn dbg(s: String) {
    if std::env::var_os("LAMP_DPI_DEBUG").is_some() {
        log(&s);
    }
}

unsafe extern "system" fn dpi_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, _id: usize, _data: usize) -> LRESULT {
    match msg {
        // Tell Windows the size we'll use, so its suggested position fits it
        WM_GETDPISCALEDSIZE => match size_for(wparam as u32) {
            Some((w, h)) => {
                let size = &mut *(lparam as *mut SIZE);
                size.cx = w;
                size.cy = h;
                dbg(format!("dpi: GETDPISCALEDSIZE {} -> {w}x{h}", wparam));
                1
            }
            None => 0,
        },
        WM_DPICHANGED => {
            let dpi = (wparam & 0xffff) as u32;
            let r = *(lparam as *const RECT);
            let (w, h) = size_for(dpi).unwrap_or((r.right - r.left, r.bottom - r.top));
            dbg(format!("dpi: DPICHANGED {dpi} -> {w}x{h} at {},{}", r.left, r.top));
            *HOLD_SIZE.lock().unwrap() = Some((w, h, std::time::Instant::now() + std::time::Duration::from_millis(700)));
            let result = DefSubclassProc(hwnd, msg, wparam, lparam);
            SetWindowPos(hwnd, std::ptr::null_mut(), r.left, r.top, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
            result
        }
        WM_WINDOWPOSCHANGING => {
            let wp = &mut *(lparam as *mut WINDOWPOS);
            if wp.flags & SWP_NOSIZE == 0 {
                if let Some((w, h)) = holding() {
                    wp.cx = w;
                    wp.cy = h;
                }
            }
            DefSubclassProc(hwnd, msg, wparam, lparam)
        }
        // A resize outside a scale change (e.g. by the user) sets the size to keep
        WM_SIZE => {
            let result = DefSubclassProc(hwnd, msg, wparam, lparam);
            if wparam as u32 == SIZE_RESTORED && holding().is_none() {
                remember_size(hwnd);
            }
            result
        }
        _ => DefSubclassProc(hwnd, msg, wparam, lparam),
    }
}

// --- window styling ----------------------------------------------------------

/// Dark title bar in the same colour as the window background
pub fn style_window(hwnd: HWND, caption_rgb: u32) {
    let dark: i32 = 1;
    // COLORREF is 0x00BBGGRR
    let colorref: u32 = ((caption_rgb & 0xff) << 16) | (caption_rgb & 0xff00) | ((caption_rgb >> 16) & 0xff);
    unsafe {
        DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE as u32, &dark as *const _ as *const c_void, 4);
        DwmSetWindowAttribute(hwnd, DWMWA_CAPTION_COLOR as u32, &colorref as *const _ as *const c_void, 4);
    }
}
