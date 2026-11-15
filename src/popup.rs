// The tray popup: opens above the tray on a left click, mirrors the window's lamp state while open,
// and hides when it loses focus (like Windows' own volume flyout).

use crate::{app, AppWindow, Popup};
use slint::{ComponentHandle, PhysicalPosition, Timer, TimerMode};
use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetSystemMetrics, SetForegroundWindow, SystemParametersInfoW, SM_CXSCREEN, SPI_GETWORKAREA,
};

const MARGIN: f32 = 12.0;

thread_local! {
    static TIMER: Timer = Timer::default();
    static HWND_CACHE: Cell<Option<HWND>> = const { Cell::new(None) };
    static HIDDEN_AT: Cell<Option<Instant>> = const { Cell::new(None) };
    static AWAY_TICKS: Cell<u32> = const { Cell::new(0) };
    static LAST_SIZE: RefCell<(u32, u32)> = const { RefCell::new((0, 0)) };
}

fn hwnd() -> Option<HWND> {
    HWND_CACHE.with(|c| {
        if c.get().is_none() {
            c.set(crate::winutil::find_own_window("Detent popup"));
        }
        c.get()
    })
}

/// Bottom-right corner of the work area (or top-right when the taskbar is at the top)
fn place(p: &Popup) {
    let sf = p.window().scale_factor();
    let size = p.window().size();
    let (w, h) = if size.width > 0 { (size.width, size.height) } else { LAST_SIZE.with(|s| *s.borrow()) };
    LAST_SIZE.with(|s| *s.borrow_mut() = (w, h));
    let mut wa = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    unsafe {
        if SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut wa as *mut RECT as *mut _, 0) == 0 {
            wa.right = GetSystemMetrics(SM_CXSCREEN);
        }
    }
    let m = (MARGIN * sf) as i32;
    let x = wa.right - w as i32 - m;
    let y = if wa.top > 0 { wa.top + m } else { wa.bottom - h as i32 - m };
    p.window().set_position(PhysicalPosition::new(x, y));
}

/// Copies the lamp state from the window (which the rest of the app keeps current)
pub fn mirror(p: &Popup, ui: &AppWindow) {
    p.set_main_on(ui.get_main_on());
    p.set_bg_on(ui.get_bg_on());
    p.set_auto_on(ui.get_auto_active());
    p.set_desk_dot(ui.get_main_tint());
    p.set_bg_dot(ui.get_bg_color());
    let state = if ui.get_mode() == 2 && ui.get_sync_running() {
        "sync"
    } else if ui.get_mode() == 1 && !ui.get_active_flow().is_empty() {
        "flow"
    } else {
        "on"
    };
    p.set_bg_state(state.into());
    // Don't move a slider under the user's finger
    if app().last_input.lock().unwrap().elapsed() > Duration::from_secs(1) {
        p.set_bright(ui.get_main_bright());
        p.set_temp(ui.get_main_temp());
    }
}

pub fn hide(p: &Popup) {
    let _ = p.hide();
    HIDDEN_AT.with(|h| h.set(Some(Instant::now())));
    TIMER.with(|t| t.stop());
}

/// Tray left click: open, or close if open
pub fn toggle(p: &Popup, ui: &AppWindow) {
    if p.window().is_visible() {
        hide(p);
        return;
    }
    // The click that took focus away from the popup also lands here: don't reopen it
    if HIDDEN_AT.with(|h| h.get()).is_some_and(|t| t.elapsed() < Duration::from_millis(300)) {
        return;
    }
    mirror(p, ui);
    // The window may have been hidden for hours: read the lamp now, and every few seconds while open
    crate::refresh_shown_state();
    place(p);
    let _ = p.show();
    place(p);
    if let Some(h) = hwnd() {
        unsafe { SetForegroundWindow(h) };
    }
    AWAY_TICKS.with(|a| a.set(0));
    let (pw, uw) = (p.as_weak(), ui.as_weak());
    let ticks = Cell::new(0u32);
    TIMER.with(|t| {
        t.start(TimerMode::Repeated, Duration::from_millis(150), move || {
            let (Some(p), Some(ui)) = (pw.upgrade(), uw.upgrade()) else { return };
            ticks.set(ticks.get() + 1);
            if ticks.get() % 20 == 0 {
                crate::refresh_shown_state();
            }
            mirror(&p, &ui);
            // Hide once focus has been elsewhere for two ticks
            let fg = unsafe { GetForegroundWindow() };
            let away = hwnd().map_or(false, |h| fg != h);
            let n = if away { AWAY_TICKS.with(|a| a.get()) + 1 } else { 0 };
            AWAY_TICKS.with(|a| a.set(n));
            if n >= 2 {
                hide(&p);
            }
        });
    });
}
