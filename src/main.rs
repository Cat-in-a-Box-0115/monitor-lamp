#![windows_subsystem = "windows"]

mod capture;
mod companion;
mod circadian;
mod flow;
mod hooks;
mod knob;
mod lamp;
mod media;
mod miio;
mod stats;
mod winutil;

slint::include_modules!();

use capture::ScreenSync;
use flow::FlowPlayer;
use lamp::{Lamp, Latest, State};
use serde_json::json;
use detent::settings::{self, Settings};
use slint::{Color, ComponentHandle, ModelRc, VecModel};
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use winutil::log;

// Fade lengths for the automatic on/off; the lamp fades by itself, so the PC
// doesn't have to stay awake for it.
const FADE_OFF_MS: u32 = 2500;
const FADE_ON_MS: u32 = 1500;
const WINDOW_BG: u32 = 0x1f1f1f;

const SWATCHES: [u32; 10] = [0xff3b30, 0xff9500, 0xffcc00, 0x34c759, 0x00c7be, 0x0a84ff, 0x5e5ce6, 0xbf5af2, 0xff2d55, 0xffffff];
const FLOWS: &[(&str, &[u32])] = &[
    ("Sunset", &[0xff5e3a, 0xff9500, 0xff2d55, 0xaf52de]),
    ("Ocean", &[0x0040ff, 0x00b4d8, 0x48cae4, 0x0077b6]),
    ("Aurora", &[0x00ffa3, 0x00c2ff, 0x7a5cff, 0x00ffa3]),
    ("Rainbow", &[0xff0000, 0xff9900, 0xffee00, 0x33dd00, 0x00aaff, 0x8800ff]),
    ("Candle", &[0xff7a00, 0xff9a2e, 0xff6a00, 0xffb347]),
    ("Neon", &[0xff00aa, 0x7a00ff, 0x00e5ff]),
];
const SMOOTH_LABELS: [&str; 11] =
    ["Instant", "Very fast", "Fast", "Fast", "Balanced", "Balanced", "Smooth", "Smooth", "Very smooth", "Very smooth", "Ambient"];
const SYNC_IDLE: &str = "Matches the backlight to your main screen in real time.";

struct App {
    settings: Mutex<Settings>,
    lamp: Arc<Lamp>,
    flow: Arc<FlowPlayer>,
    sync: Arc<ScreenSync>,
    ui: slint::Weak<AppWindow>,
    main_power: Mutex<Option<bool>>,           // so time-of-day updates don't wake an off lamp
    circ_last: Mutex<(Option<u32>, Option<u32>)>, // last ct / brightness sent by time-of-day
    status_error: Mutex<Option<String>>,
    session_ending: AtomicBool,
    visible: AtomicBool,
    input_seq: AtomicU64,
    last_input: Mutex<Instant>,
    bg_rgb: AtomicU32,
    send_bright: Arc<Latest<u32>>,
    send_temp: Arc<Latest<u32>>,
    send_bg_bright: Arc<Latest<u32>>,
    send_bg_rgb: Arc<Latest<u32>>,
    dry_run: bool,
    /// The knob's last LAMP_STATE: (power, bright, ct, bg_power, bg_rgb)
    knob_last: Mutex<Option<(bool, u8, u16, bool, u32)>>,
}

static APP: OnceLock<App> = OnceLock::new();

fn app() -> &'static App {
    APP.get().expect("app not initialised")
}

thread_local! {
    static TRAY_CIRCADIAN: RefCell<Option<tray_icon::menu::CheckMenuItem>> = const { RefCell::new(None) };
}

// --- helpers -------------------------------------------------------------

fn color(rgb: u32) -> Color {
    Color::from_rgb_u8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

fn rgb_of(c: Color) -> u32 {
    ((c.red() as u32) << 16) | ((c.green() as u32) << 8) | c.blue() as u32
}

fn hex6(rgb: u32) -> String {
    format!("#{:06X}", rgb & 0xffffff)
}

fn kelvin_color(k: u32) -> Color {
    let (r, g, b) = circadian::kelvin_rgb(k);
    Color::from_rgb_u8(r, g, b)
}

fn hue_to_rgb(h: f64) -> u32 {
    let x = (255.0 * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs())).round() as u32;
    let (r, g, b) = match (h / 60.0).floor() as u32 % 6 {
        0 => (255, x, 0),
        1 => (x, 255, 0),
        2 => (0, 255, x),
        3 => (0, x, 255),
        4 => (x, 0, 255),
        _ => (255, 0, x),
    };
    (r << 16) | (g << 8) | b
}

fn rgb_to_hue(rgb: u32) -> Option<f64> {
    let (h, s, _) = flow::to_hsv(rgb);
    (s > 0.0).then_some(h)
}

/// Run on the UI thread, from any thread
fn ui_do(f: impl FnOnce(&AppWindow) + Send + 'static) {
    if let Some(a) = APP.get() {
        let _ = a.ui.upgrade_in_event_loop(move |ui| f(&ui));
    }
}

fn settings() -> Settings {
    app().settings.lock().unwrap().clone()
}

fn update_settings(save: bool, f: impl FnOnce(&mut Settings)) -> Settings {
    let mut s = app().settings.lock().unwrap();
    f(&mut s);
    if save {
        if let Err(e) = settings::save(&s) {
            log(&format!("couldn't save settings: {e}"));
        }
    }
    s.clone()
}

fn touched() -> u64 {
    let a = app();
    *a.last_input.lock().unwrap() = Instant::now();
    a.input_seq.fetch_add(1, Ordering::SeqCst) + 1
}

fn is_installed() -> bool {
    !std::env::current_exe().map(|p| p.to_string_lossy().contains("\\target\\")).unwrap_or(true)
}

// --- lamp actions (worker threads) ------------------------------------------

fn refresh_state() -> Option<State> {
    let a = app();
    let s = a.lamp.state().ok()?;
    *a.main_power.lock().unwrap() = Some(s.power);
    if a.settings.lock().unwrap().user_brightness.is_none() {
        update_settings(true, |st| st.user_brightness = Some(s.bright));
    }
    Some(s)
}

/// Reads the lamp and shows it, unless the user changed something meanwhile
fn poll_state() {
    thread::spawn(|| {
        let seq = app().input_seq.load(Ordering::SeqCst);
        if let Some(s) = refresh_state() {
            ui_do(move |ui| {
                if app().input_seq.load(Ordering::SeqCst) == seq {
                    apply_state(ui, &s);
                }
            });
        }
    });
}

/// `backlight`: None = follow the setting
fn set_power(on: bool, backlight: Option<bool>, persist: bool, fade_ms: u32) -> Option<State> {
    let a = app();
    let st = settings();
    let with_bg = backlight.unwrap_or(st.backlight_with_main);
    let word = if on { "on" } else { "off" };
    if !on && with_bg {
        a.flow.stop();
    }
    let call = |method: &str| {
        let params = json!([word, "smooth", fade_ms]);
        if persist {
            a.lamp.persist(method, params, Duration::from_secs(180))
        } else {
            a.lamp.call(method, params)
        }
    };
    let mut results = vec![call("set_power").is_ok()];
    if with_bg {
        results.push(call("bg_set_power").is_ok());
    }
    let r: Vec<_> = results.iter().map(|ok| if *ok { "ok" } else { "failed" }).collect();
    log(&format!("power {word}{}: {}", if with_bg { " (+backlight)" } else { "" }, r.join(", ")));
    *a.main_power.lock().unwrap() = Some(on);
    if on {
        *a.circ_last.lock().unwrap() = (None, None);
        apply_circadian();
        if with_bg {
            if let Some(f) = &st.flow {
                a.flow.start(&f.colors, st.flow_speed);
            }
        }
    }
    refresh_state()
}

fn circadian_target(st: &Settings) -> (circadian::Target, u32) {
    let t = circadian::target_now(&st.circadian);
    let bright = ((st.user_brightness.unwrap_or(100) as f64 * t.factor).round() as u32).max(1);
    (t, bright)
}

fn apply_circadian() {
    let a = app();
    let st = settings();
    ui_do(show_circadian);
    if !st.circadian.enabled || *a.main_power.lock().unwrap() == Some(false) || !a.lamp.configured() {
        return;
    }
    let (t, bright) = circadian_target(&st);
    let (last_ct, last_bright) = *a.circ_last.lock().unwrap();
    if last_ct.map_or(true, |ct| ct.abs_diff(t.ct) >= 50) {
        if a.lamp.call("set_ct_abx", json!([t.ct, "smooth", 4000])).is_err() {
            return; // lamp offline; retried next minute
        }
        a.circ_last.lock().unwrap().0 = Some(t.ct);
    }
    if st.circadian.dim && last_bright != Some(bright) && a.lamp.call("set_bright", json!([bright, "smooth", 4000])).is_ok() {
        a.circ_last.lock().unwrap().1 = Some(bright);
    }
}

/// Windows may end the process right after this; the "off" packets are sent
/// synchronously before it returns.
fn off_now(reason: &str) {
    let a = app();
    if a.session_ending.swap(true, Ordering::SeqCst) {
        return;
    }
    if !a.settings.lock().unwrap().off_at_shutdown {
        log(&format!("{reason}: lamp left on (setting off)"));
        return;
    }
    a.flow.stop();
    a.sync.stop();
    let sent = a.lamp.off_now(true, FADE_OFF_MS); // whole lamp, backlight included
    log(&format!("{reason}: off {}", if sent { "sent" } else { "NOT sent (no recent contact with lamp)" }));
}

fn on_hook(e: hooks::Event) {
    let a = app();
    let st = settings();
    match e {
        hooks::Event::SessionEnd => off_now("session end"),
        hooks::Event::Suspend => {
            if !st.sleep_wake {
                log("suspend");
                return;
            }
            a.flow.stop();
            ui_do(stop_sync);
            let sent = a.lamp.off_now(true, FADE_OFF_MS);
            log(&format!("suspend: off {}", if sent { "sent" } else { "NOT sent" }));
        }
        hooks::Event::Resume => {
            log("resume");
            a.session_ending.store(false, Ordering::SeqCst);
            if st.sleep_wake {
                thread::spawn(|| show_lamp_state(set_power(true, None, true, FADE_ON_MS)));
            }
        }
        hooks::Event::Lock if st.lock_unlock => {
            thread::spawn(|| show_lamp_state(set_power(false, None, false, 300)));
        }
        hooks::Event::Unlock if st.lock_unlock => {
            thread::spawn(|| show_lamp_state(set_power(true, None, false, 300)));
        }
        _ => {}
    }
}

/// Shows a state read in the background, unless the user is mid-change
fn show_lamp_state(s: Option<State>) {
    if let Some(s) = s {
        ui_do(move |ui| {
            if app().last_input.lock().unwrap().elapsed() > Duration::from_secs(2) {
                apply_state(ui, &s);
            }
        });
    }
}

// --- the knob ----------------------------------------------------------------

fn on_knob(m: detent::protocol::Msg) {
    use detent::protocol::Msg;
    match m {
        Msg::LampState { power, bright, ct, bg_power, bg_rgb } => {
            thread::spawn(move || on_knob_lamp((power, bright, ct, bg_power, bg_rgb)));
        }
        Msg::Hello { version, proto, .. } => log(&format!("knob: firmware {version}, protocol {proto}")),
        Msg::Log(text) => log(&format!("knob says: {text}")),
        _ => {}
    }
}

/// The knob changed the lamp itself (over Wi-Fi). Treated like a change made in the app:
/// brightness becomes the user's brightness, a brightness or warmth change hands control back from
/// time of day, and a backlight change stops screen sync and colour flows.
fn on_knob_lamp(new: (bool, u8, u16, bool, u32)) {
    let a = app();
    touched();
    let prev = a.knob_last.lock().unwrap().replace(new).or_else(|| {
        let s = a.lamp.state().ok()?;
        Some((s.power, s.bright as u8, s.ct as u16, s.bg_power, s.bg_rgb))
    });
    let (power, bright, ct, bg_power, bg_rgb) = new;
    let changed = |f: fn(&(bool, u8, u16, bool, u32)) -> u32| prev.as_ref().map_or(true, |p| f(p) != f(&new));
    let main_changed = changed(|s| s.1 as u32) || changed(|s| s.2 as u32);
    let bg_changed = changed(|s| s.3 as u32) || changed(|s| s.4);
    log(&format!("knob set the lamp: power {power} bright {bright} ct {ct} bg {bg_power} {bg_rgb:06x}"));

    *a.main_power.lock().unwrap() = Some(power);
    if changed(|s| s.1 as u32) {
        update_settings(true, |s| s.user_brightness = Some(bright as u32));
    }
    if bg_changed {
        a.bg_rgb.store(bg_rgb, Ordering::Relaxed);
    }
    ui_do(move |ui| {
        if main_changed && settings().circadian.enabled {
            set_circadian_enabled(ui, false);
        }
        if bg_changed {
            stop_sync(ui);
            if app().flow.running() {
                stop_flow(ui);
            }
        }
    });
    poll_state();
}

// --- UI updates (UI thread) -----------------------------------------------------

fn show_media(_ui: &AppWindow, m: Option<media::Media>) {
    // Filled in with the popup (Task 8)
    let _ = m;
}

fn show_knob(_ui: &AppWindow, up: bool) {
    let _ = up;
}

fn apply_state(ui: &AppWindow, s: &State) {
    let a = app();
    let st = settings();
    ui.set_main_on(s.power);
    ui.set_main_bright(st.user_brightness.unwrap_or(s.bright) as f32);
    if !st.circadian.enabled && s.ct > 0 {
        ui.set_main_temp(s.ct as f32);
        ui.set_main_tint(kelvin_color(s.ct));
    }
    show_bright_label(ui);
    ui.set_bg_on(s.bg_power);
    ui.set_bg_bright(s.bg_bright as f32);
    if !a.sync.running() && !a.flow.running() {
        show_bg_color(ui, s.bg_rgb);
        if let Some(h) = rgb_to_hue(s.bg_rgb) {
            ui.set_hue(h as f32);
        }
        if s.bg_flowing {
            ui.set_bg_sub("Colour flow running".into());
        }
    }
}

fn show_status(ui: &AppWindow) {
    let a = app();
    let online = a.lamp.online();
    let text = if !a.lamp.configured() {
        "Add the lamp's IP address and token in Settings".to_string()
    } else {
        match online {
            Some(true) => format!("Connected to {}", a.settings.lock().unwrap().ip),
            Some(false) => a.status_error.lock().unwrap().clone().unwrap_or_else(|| "Lamp not reachable".into()),
            None => "Connecting…".into(),
        }
    };
    ui.set_status_text(text.into());
    ui.set_status_kind(match online {
        Some(true) => 1,
        Some(false) => 2,
        None => 0,
    });
}

fn show_bright_label(ui: &AppWindow) {
    let st = settings();
    let v = ui.get_main_bright().round() as u32;
    let label = if st.circadian.enabled && st.circadian.dim {
        let (_, b) = circadian_target(&st);
        if b != v { format!("{v}% → {b}%") } else { format!("{v}%") }
    } else {
        format!("{v}%")
    };
    ui.set_bright_label(label.into());
}

fn show_circadian(ui: &AppWindow) {
    let st = settings();
    ui.set_auto_active(st.circadian.enabled);
    if st.circadian.enabled {
        let (t, _) = circadian_target(&st);
        ui.set_main_temp(t.ct as f32);
        ui.set_main_tint(kelvin_color(t.ct));
        ui.set_main_sub(t.phase.into());
    } else {
        ui.set_main_sub("Shines on the desk".into());
    }
    show_bright_label(ui);
    show_curve(ui, &st.circadian);
}

fn show_curve(ui: &AppWindow, c: &settings::Circadian) {
    let colors: Vec<Color> = (0..96).map(|i| kelvin_color(circadian::target_at(i as f64 * 15.0 + 7.5, c).ct)).collect();
    let mut path = String::new();
    for i in 0..=96 {
        let m = i as f64 * 15.0;
        let y = 92.0 - circadian::target_at(m, c).factor * 80.0;
        path += &format!("{} {m} {y:.1} ", if i == 0 { "M" } else { "L" });
    }
    let (h, m, _) = winutil::local_hms();
    ui.set_curve_colors(ModelRc::new(VecModel::from(colors)));
    ui.set_curve_path(path.into());
    ui.set_now_frac((h as f32 * 60.0 + m as f32) / 1440.0);
}

fn show_settings(ui: &AppWindow) {
    let st = settings();
    let c = &st.circadian;
    ui.set_prefs(Prefs {
        open_at_login: st.open_at_login,
        start_hidden: st.start_hidden,
        close_to_tray: st.close_to_tray,
        on_at_login: st.on_at_login,
        off_at_shutdown: st.off_at_shutdown,
        sleep_wake: st.sleep_wake,
        lock_unlock: st.lock_unlock,
        backlight_with_main: st.backlight_with_main,
        circ_enabled: c.enabled,
        circ_dim: c.dim,
        wake: c.wake.clone().into(),
        bed: c.bed.clone().into(),
        ip: st.ip.clone().into(),
        token: st.token.clone().into(),
    });
    ui.set_wind_down(c.wind_down as f32);
    ui.set_day_temp(c.day_temp as f32);
    ui.set_night_temp(c.night_temp as f32);
    ui.set_night_level(c.night_level as f32);
    ui.set_flow_speed(st.flow_speed as f32);
    ui.set_sync_smooth(st.sync_smoothing as f32);
    ui.set_sync_smooth_label(SMOOTH_LABELS[st.sync_smoothing.min(10) as usize].into());
    show_circadian(ui);
    TRAY_CIRCADIAN.with(|t| {
        if let Some(item) = t.borrow().as_ref() {
            item.set_checked(c.enabled);
        }
    });
}

fn show_bg_color(ui: &AppWindow, rgb: u32) {
    app().bg_rgb.store(rgb, Ordering::Relaxed);
    ui.set_bg_color(color(rgb));
    ui.set_bg_fill(color(rgb).into());
    ui.set_bg_hex(hex6(rgb).into());
}

fn ensure_bg_on(ui: &AppWindow) {
    if ui.get_bg_on() {
        return;
    }
    ui.set_bg_on(true);
    thread::spawn(|| {
        let _ = app().lamp.call("bg_set_power", json!(["on", "smooth", 300]));
    });
}

fn set_bg_color(ui: &AppWindow, rgb: u32, move_hue: bool) {
    stop_sync(ui);
    if app().flow.running() {
        stop_flow(ui);
    }
    show_bg_color(ui, rgb);
    if move_hue {
        if let Some(h) = rgb_to_hue(rgb) {
            ui.set_hue(h as f32);
        }
    }
    ensure_bg_on(ui);
    app().send_bg_rgb.push(rgb);
}

/// Five colours along a flow, for its preview tile
fn flow_item(name: &str, colors: &[u32]) -> FlowItem {
    let n = colors.len();
    let at = |p: f64| {
        let x = p * n as f64;
        let k = (x.floor() as usize).min(n - 1);
        color(flow::mix(colors[k], colors[(k + 1) % n], x - k as f64))
    };
    FlowItem { name: name.into(), c1: at(0.0), c2: at(0.25), c3: at(0.5), c4: at(0.75), c5: at(0.999) }
}

fn start_flow(ui: &AppWindow, name: &str) {
    let Some((name, colors)) = FLOWS.iter().find(|(n, _)| *n == name) else { return };
    stop_sync(ui);
    ui.set_active_flow((*name).into());
    ui.set_bg_sub(format!("{name} flow").into());
    let mid = flow::mix(colors[0], colors[colors.len() / 2], 0.5);
    ui.set_bg_color(color(mid));
    ui.set_bg_fill(color(mid).into());
    ensure_bg_on(ui);
    let st = update_settings(true, |s| s.flow = Some(settings::Flow { name: name.to_string(), colors: colors.to_vec() }));
    app().flow.start(colors, st.flow_speed);
}

fn stop_flow(ui: &AppWindow) {
    ui.set_active_flow("".into());
    ui.set_bg_sub("Behind the monitor".into());
    app().flow.stop();
    if settings().flow.is_some() {
        update_settings(true, |s| s.flow = None);
    }
}

fn start_sync(ui: &AppWindow) {
    let a = app();
    if a.sync.running() {
        return;
    }
    if a.flow.running() {
        stop_flow(ui);
    }
    ui.set_mode(2);
    ensure_bg_on(ui);
    a.sync.start(a.lamp.clone(), a.dry_run, |f| {
        ui_do(move |ui| {
            if !app().sync.running() {
                return;
            }
            ui.set_sync_color(color(f.rgb));
            ui.set_bg_color(color(f.rgb));
            ui.set_bg_fill(color(f.rgb).into());
            ui.set_sync_hex(hex6(f.rgb).into());
            ui.set_sync_rate(format!("{} updates/s", f.rate).into());
        });
    });
    ui.set_sync_running(true);
    ui.set_bg_sub("Matching your screen".into());
}

fn stop_sync(ui: &AppWindow) {
    let a = app();
    if !a.sync.running() {
        return;
    }
    a.sync.stop();
    ui.set_sync_running(false);
    ui.set_sync_hex("Not running".into());
    ui.set_sync_rate(SYNC_IDLE.into());
    ui.set_bg_sub("Behind the monitor".into());
    show_bg_color(ui, a.bg_rgb.load(Ordering::Relaxed));
}

fn set_circadian_enabled(ui: &AppWindow, enabled: bool) {
    let before = settings();
    let st = update_settings(true, |s| s.circadian.enabled = enabled);
    *app().circ_last.lock().unwrap() = (None, None);
    show_settings(ui);
    thread::spawn(move || {
        apply_circadian();
        // Turning time-of-day off: put the brightness back to what the user picked
        if before.circadian.enabled && !enabled {
            if let Some(b) = st.user_brightness {
                let _ = app().lamp.call("set_bright", json!([b, "smooth", 1500]));
            }
        }
    });
}

fn show_window(ui: &AppWindow) {
    let _ = ui.show();
    app().visible.store(true, Ordering::Relaxed);
    prepare_native_window(0);
    show_circadian(ui);
    if !app().sync.running() {
        poll_state();
    }
}

/// Styles the native window, fixes its moves between screens, and brings it to
/// the front. The window only exists once the event loop has created it, so
/// this retries for a moment.
fn prepare_native_window(attempt: u32) {
    match winutil::find_own_window("Detent") {
        Some(hwnd) => {
            winutil::style_window(hwnd, WINDOW_BG);
            winutil::fix_dpi_resizing(hwnd);
            unsafe {
                use windows_sys::Win32::UI::WindowsAndMessaging::{IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE};
                if IsIconic(hwnd) != 0 {
                    ShowWindow(hwnd, SW_RESTORE);
                }
                SetForegroundWindow(hwnd);
            }
        }
        None if attempt < 20 => {
            slint::Timer::single_shot(Duration::from_millis(50), move || prepare_native_window(attempt + 1));
        }
        None => log("couldn't find the main window to style it"),
    }
}

fn quit() {
    log("quit by user");
    app().sync.stop();
    app().flow.stop();
    let _ = slint::quit_event_loop();
}

// --- setup -------------------------------------------------------------------

fn wire_callbacks(ui: &AppWindow) {
    let w = ui.as_weak();

    ui.on_retry(poll_state);

    let u = w.clone();
    ui.on_main_toggled(move |on| {
        let ui = u.unwrap();
        let seq = touched();
        ui.set_main_on(on);
        thread::spawn(move || {
            let s = set_power(on, Some(false), false, 300);
            ui_do(move |ui| {
                if let Some(s) = s {
                    if app().input_seq.load(Ordering::SeqCst) == seq {
                        apply_state(ui, &s);
                    }
                }
            });
        });
    });

    let u = w.clone();
    ui.on_main_bright_moved(move |v| {
        touched();
        show_bright_label(&u.unwrap());
        app().send_bright.push(v.round() as u32);
    });

    let u = w.clone();
    ui.on_main_temp_moved(move |v| {
        touched();
        let ui = u.unwrap();
        let k = v.round() as u32;
        ui.set_main_tint(kelvin_color(k));
        // Picking a temperature by hand takes over from time-of-day lighting
        if settings().circadian.enabled {
            update_settings(true, |s| s.circadian.enabled = false);
            show_settings(&ui);
        }
        app().send_temp.push(k);
    });

    let u = w.clone();
    ui.on_auto_clicked(move || {
        let ui = u.unwrap();
        set_circadian_enabled(&ui, !settings().circadian.enabled);
    });

    let u = w.clone();
    ui.on_bg_toggled(move |on| {
        touched();
        let ui = u.unwrap();
        ui.set_bg_on(on);
        if !on {
            stop_sync(&ui);
            if app().flow.running() {
                stop_flow(&ui);
            }
        }
        thread::spawn(move || {
            let _ = app().lamp.call("bg_set_power", json!([if on { "on" } else { "off" }, "smooth", 300]));
        });
    });

    ui.on_bg_bright_moved(|v| {
        touched();
        app().send_bg_bright.push(v.round() as u32);
    });

    let u = w.clone();
    ui.on_hue_moved(move |h| {
        touched();
        set_bg_color(&u.unwrap(), hue_to_rgb(h as f64), false);
    });

    let u = w.clone();
    ui.on_pick_color(move |c| {
        touched();
        set_bg_color(&u.unwrap(), rgb_of(c), true);
    });

    let u = w.clone();
    ui.on_custom_hex(move |t| {
        let hex = t.trim().trim_start_matches('#');
        if let (6, Ok(rgb)) = (hex.len(), u32::from_str_radix(hex, 16)) {
            touched();
            set_bg_color(&u.unwrap(), rgb, true);
        }
    });

    let u = w.clone();
    ui.on_mode_selected(move |i| {
        let ui = u.unwrap();
        if i != 2 {
            stop_sync(&ui);
        }
        ui.set_mode(i);
    });

    let u = w.clone();
    ui.on_flow_picked(move |name| start_flow(&u.unwrap(), &name));

    let u = w.clone();
    ui.on_sync_clicked(move || {
        let ui = u.unwrap();
        if app().sync.running() { stop_sync(&ui) } else { start_sync(&ui) }
    });

    let u = w.clone();
    ui.on_set_bool(move |key, v| {
        let ui = u.unwrap();
        if key == "circadian.enabled" {
            set_circadian_enabled(&ui, v);
            return;
        }
        update_settings(true, |s| match key.as_str() {
            "openAtLogin" => s.open_at_login = v,
            "startHidden" => s.start_hidden = v,
            "closeToTray" => s.close_to_tray = v,
            "onAtLogin" => s.on_at_login = v,
            "offAtShutdown" => s.off_at_shutdown = v,
            "sleepWake" => s.sleep_wake = v,
            "lockUnlock" => s.lock_unlock = v,
            "backlightWithMain" => s.backlight_with_main = v,
            "circadian.dim" => s.circadian.dim = v,
            _ => {}
        });
        if key == "openAtLogin" && is_installed() {
            winutil::set_autostart(v);
        }
        if key == "circadian.dim" {
            *app().circ_last.lock().unwrap() = (None, None);
            thread::spawn(apply_circadian);
        }
        show_settings(&ui);
    });

    let u = w.clone();
    ui.on_set_number(move |key, v, done| {
        let ui = u.unwrap();
        let n = v.round() as u32;
        let st = update_settings(done, |s| match key.as_str() {
            "flowSpeed" => s.flow_speed = n,
            "syncSmoothing" => s.sync_smoothing = n,
            "circadian.windDown" => s.circadian.wind_down = n,
            "circadian.dayTemp" => s.circadian.day_temp = n,
            "circadian.nightTemp" => s.circadian.night_temp = n,
            "circadian.nightLevel" => s.circadian.night_level = n,
            _ => {}
        });
        match key.as_str() {
            "flowSpeed" => app().flow.set_speed(n),
            "syncSmoothing" => {
                app().sync.smoothing.store(n, Ordering::Relaxed);
                ui.set_sync_smooth_label(SMOOTH_LABELS[n.min(10) as usize].into());
            }
            _ => {
                show_curve(&ui, &st.circadian);
                if done {
                    show_circadian(&ui);
                    thread::spawn(apply_circadian);
                }
            }
        }
    });

    let u = w.clone();
    ui.on_set_text(move |key, t| {
        let t = t.trim().to_string();
        if circadian::to_minutes(&t).is_none() {
            return;
        }
        update_settings(true, |s| match key.as_str() {
            "circadian.wake" => s.circadian.wake = t,
            "circadian.bed" => s.circadian.bed = t,
            _ => {}
        });
        show_circadian(&u.unwrap());
        thread::spawn(apply_circadian);
    });

    let u = w.clone();
    ui.on_save_lamp(move |ip, token| {
        let ui = u.unwrap();
        let (ip, token) = (ip.trim().to_string(), token.trim().to_lowercase());
        update_settings(true, |s| {
            s.ip = ip.clone();
            s.token = token.clone();
        });
        app().lamp.configure(&ip, &token);
        ui.set_lamp_test("Connecting…".into());
        thread::spawn(|| {
            let s = refresh_state();
            ui_do(move |ui| {
                ui.set_lamp_test(if s.is_some() { "Connected." } else { "The lamp didn't answer. Check the IP address and token." }.into());
                if let Some(s) = s {
                    apply_state(ui, &s);
                }
            });
        });
    });

    ui.on_open_log(|| winutil::open_path(&winutil::log_file()));

    let u = w.clone();
    ui.on_settings_opened(move || show_settings(&u.unwrap()));

    ui.window().on_close_requested(|| {
        app().visible.store(false, Ordering::Relaxed);
        if !settings().close_to_tray {
            quit();
        }
        slint::CloseRequestResponse::HideWindow
    });
}

fn tray_icon_image() -> Option<tray_icon::Icon> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(include_bytes!("../assets/tray@2x.png")));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::GrayscaleAlpha => buf.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        _ => return None,
    };
    tray_icon::Icon::from_rgba(rgba, info.width, info.height).ok()
}

fn create_tray() -> Option<tray_icon::TrayIcon> {
    use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    let open = MenuItem::new("Open Detent", true, None);
    let on = MenuItem::new("Lamp on", true, None);
    let off = MenuItem::new("Lamp off", true, None);
    let sync = MenuItem::new("Screen sync", true, None);
    let circ = CheckMenuItem::new("Time-of-day lighting", true, settings().circadian.enabled, None);
    let open_log = MenuItem::new("Open log", true, None);
    let quit_item = MenuItem::new("Quit", true, None);
    let menu = Menu::new();
    menu.append_items(&[
        &open,
        &PredefinedMenuItem::separator(),
        &on,
        &off,
        &sync,
        &PredefinedMenuItem::separator(),
        &circ,
        &PredefinedMenuItem::separator(),
        &open_log,
        &quit_item,
    ])
    .ok()?;

    let ids = [open.id(), on.id(), off.id(), sync.id(), circ.id(), open_log.id(), quit_item.id()].map(|i| i.clone());
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        let Some(which) = ids.iter().position(|id| *id == e.id) else { return };
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = app().ui.upgrade() else { return };
            match which {
                0 => show_window(&ui),
                1 => {
                    thread::spawn(|| show_lamp_state(set_power(true, None, false, 300)));
                }
                2 => {
                    thread::spawn(|| show_lamp_state(set_power(false, None, false, 300)));
                }
                3 => {
                    if app().sync.running() { stop_sync(&ui) } else { start_sync(&ui) }
                }
                4 => set_circadian_enabled(&ui, !settings().circadian.enabled),
                5 => winutil::open_path(&winutil::log_file()),
                _ => quit(),
            }
        });
    }));
    TrayIconEvent::set_event_handler(Some(|e: TrayIconEvent| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
            let _ = slint::invoke_from_event_loop(|| {
                if let Some(ui) = app().ui.upgrade() {
                    show_window(&ui);
                }
            });
        }
    }));

    let mut b = TrayIconBuilder::new().with_menu(Box::new(menu)).with_tooltip("Detent").with_menu_on_left_click(false);
    if let Some(icon) = tray_icon_image() {
        b = b.with_icon(icon);
    }
    let tray = b.build().map_err(|e| log(&format!("tray: {e}"))).ok()?;
    TRAY_CIRCADIAN.with(|t| *t.borrow_mut() = Some(circ));
    Some(tray)
}

/// Background checks: the lamp every 30s (keeps the status honest, reconnects
/// after network hiccups, and keeps the handshake fresh so the instant "off"
/// at sign-out can be sent), time-of-day every minute, and the shown state
/// every 10s while the window is open.
fn scheduler() {
    thread::spawn(|| {
        let mut tick: u64 = 0;
        loop {
            thread::sleep(Duration::from_secs(1));
            tick += 1;
            let a = app();
            if a.session_ending.load(Ordering::SeqCst) {
                continue;
            }
            if tick % 30 == 0 {
                // Started without usable settings (e.g. the file couldn't be read at boot): try again
                if !a.lamp.configured() {
                    let (s, err) = settings::load();
                    if err.is_none() && !s.ip.is_empty() {
                        log(&format!("settings reloaded, lamp {}", s.ip));
                        a.lamp.configure(&s.ip, &s.token);
                        *a.settings.lock().unwrap() = s;
                        ui_do(show_settings);
                    }
                }
                if !a.lamp.streaming() && !a.visible.load(Ordering::Relaxed) {
                    let _ = refresh_state();
                }
            }
            if tick % 60 == 0 {
                apply_circadian();
            }
            if tick % 10 == 0 && a.visible.load(Ordering::Relaxed) && !a.sync.running() && !a.lamp.streaming() {
                poll_state();
            }
        }
    });
}

fn main() {
    winutil::rotate_log();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let at_login = args.iter().any(|a| a == "--hidden");
    log(&format!(
        "--- start v{}{} args={}",
        env!("CARGO_PKG_VERSION"),
        if at_login { " (at sign-in)" } else { "" },
        args.join(" ")
    ));

    if !winutil::single_instance(|| {
        log("second launch, showing window");
        let _ = slint::invoke_from_event_loop(|| {
            if let Some(ui) = APP.get().and_then(|a| a.ui.upgrade()) {
                show_window(&ui);
            }
        });
    }) {
        log("another instance is already running, exiting");
        return;
    }

    match settings::migrate_from(&settings::old_file(), &settings::file()) {
        Ok(true) => log("settings: copied from Monitor Lamp"),
        Ok(false) => {}
        Err(e) => log(&format!("settings: couldn't copy from Monitor Lamp: {e}")),
    }
    let (st, err) = settings::load();
    log(&match &err {
        Some(e) => format!("settings: defaults ({e})"),
        None => format!("settings: loaded, lamp {}", if st.ip.is_empty() { "(no ip)" } else { &st.ip }),
    });

    let ui = AppWindow::new().expect("couldn't create the window");
    ui.set_version(env!("CARGO_PKG_VERSION").into());
    ui.set_swatches(ModelRc::new(VecModel::from(SWATCHES.iter().map(|c| color(*c)).collect::<Vec<_>>())));
    ui.set_flows(ModelRc::new(VecModel::from(FLOWS.iter().map(|(n, c)| flow_item(n, c)).collect::<Vec<_>>())));

    let lamp = Lamp::new(
        Box::new(|online, error| {
            match online {
                Some(true) => log("lamp online"),
                Some(false) => log(&format!("lamp offline: {}", error.as_deref().unwrap_or("?"))),
                None => {}
            }
            if let Some(a) = APP.get() {
                if error.is_some() || online != Some(false) {
                    *a.status_error.lock().unwrap() = error;
                }
            }
            ui_do(show_status);
        }),
        Box::new(|m| log(&m)),
    );
    lamp.configure(&st.ip, &st.token);

    let l = lamp.clone();
    let send_bright = Latest::new(move |v: u32| {
        update_settings(true, |s| s.user_brightness = Some(v));
        let st = settings();
        let bright = if st.circadian.enabled && st.circadian.dim { circadian_target(&st).1 } else { v };
        app().circ_last.lock().unwrap().1 = Some(bright);
        let _ = l.call("set_bright", json!([bright, "smooth", 200]));
        ui_do(show_bright_label);
    });
    let l = lamp.clone();
    let send_temp = Latest::new(move |k: u32| {
        let _ = l.call("set_ct_abx", json!([k, "smooth", 200]));
    });
    let l = lamp.clone();
    let send_bg_bright = Latest::new(move |v: u32| {
        let _ = l.call("bg_set_bright", json!([v, "smooth", 150]));
    });
    let l = lamp.clone();
    let send_bg_rgb = Latest::new(move |rgb: u32| {
        let _ = l.call("bg_set_rgb", json!([rgb, "smooth", 150]));
    });

    let sync = ScreenSync::new(st.sync_smoothing);
    let flow = FlowPlayer::new(lamp.clone());
    let resume_flow = st.flow.clone();
    let (on_at_login, start_hidden, open_at_login) = (st.on_at_login, st.start_hidden, st.open_at_login);
    let _ = APP.set(App {
        settings: Mutex::new(st),
        lamp,
        flow,
        sync,
        ui: ui.as_weak(),
        main_power: Mutex::new(None),
        circ_last: Mutex::new((None, None)),
        status_error: Mutex::new(None),
        session_ending: AtomicBool::new(false),
        visible: AtomicBool::new(false),
        input_seq: AtomicU64::new(0),
        last_input: Mutex::new(Instant::now() - Duration::from_secs(60)),
        bg_rgb: AtomicU32::new(0xffb340),
        send_bright,
        send_temp,
        send_bg_bright,
        send_bg_rgb,
        dry_run: std::env::var_os("LAMP_DRY_RUN").is_some(),
        knob_last: Mutex::new(None),
    });

    wire_callbacks(&ui);
    let tray = create_tray();
    show_settings(&ui);
    show_status(&ui);
    if let Some(f) = &resume_flow {
        ui.set_active_flow(f.name.clone().into());
        ui.set_bg_sub(format!("{} flow", f.name).into());
        ui.set_mode(1);
    }
    if is_installed() {
        winutil::set_autostart(open_at_login);
    }
    hooks::start(on_hook);
    scheduler();
    companion::start(|m| ui_do(move |ui| show_media(ui, m)), |up| ui_do(move |ui| show_knob(ui, up)), on_knob);
    log(&format!(
        "window and tray created{}; lamp {}",
        if tray.is_some() { "" } else { " (NO tray)" },
        if app().lamp.configured() { format!("configured ({})", settings().ip) } else { "NOT configured".into() }
    ));

    thread::spawn(move || {
        let a = app();
        let s = if at_login && on_at_login {
            set_power(true, None, true, FADE_ON_MS)
        } else {
            let s = refresh_state();
            if let (Some(s), Some(f)) = (&s, &resume_flow) {
                if s.bg_power {
                    a.flow.start(&f.colors, settings().flow_speed);
                }
            }
            s
        };
        if let Some(s) = s {
            ui_do(move |ui| apply_state(ui, &s));
        }
        apply_circadian();
    });

    if !(at_login && start_hidden) {
        show_window(&ui);
    }
    if let Err(e) = slint::run_event_loop_until_quit() {
        log(&format!("event loop error: {e}"));
    }
    drop(tray);
}
