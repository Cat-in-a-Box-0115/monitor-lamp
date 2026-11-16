// Collects what the knob shows (now playing, cover, lyrics, PC stats, time, app volumes, mic, monitor, the app in
// front) and keeps it flowing over the link; carries out what the knob asks the PC to do.
// The snapshot always holds the latest state, so a (re)connected knob gets everything at once.

use crate::audio::{self, Cmd};
use crate::knob::{self, FwEvent, Out};
use crate::media::{self, Media, ART_KNOB};
use crate::stats::Sampler;
use crate::winutil::log;
use detent::apps::{self, AppRule};
use detent::link::{nothing_playing, Snapshot};
use detent::lyrics::{self, Found, TrackKey, Tracker};
use detent::protocol::Msg;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// What's playing and its lyrics, under one lock so a lyric line always belongs to the media it's read with
struct Playing {
    media: Option<Media>,
    lyrics: Tracker,
}

/// The program in front and the FOCUS it gave
struct Focus {
    pid: u32,
    mode: u8,
    label: String,
}

struct Companion {
    snap: Arc<Mutex<Snapshot>>,
    tx: Sender<Out>,
    playing: Mutex<Playing>,
    connected: AtomicBool,
    audio: Sender<Cmd>,
    display: Sender<u8>,
    focus: Mutex<Focus>,
}

/// Per-app knob modes, from settings
static RULES: Mutex<Vec<AppRule>> = Mutex::new(Vec::new());

static C: OnceLock<Companion> = OnceLock::new();

fn c() -> &'static Companion {
    C.get().expect("companion not started")
}

fn send(m: Msg) {
    let _ = c().tx.send(Out::Msg(m));
}

fn now_playing(m: &Media) -> Msg {
    Msg::NowPlaying { playing: m.playing, pos_ms: m.pos_now(), dur_ms: m.dur_ms, title: m.title.clone(), artist: m.artist.clone() }
}

fn time_msg() -> Msg {
    let unix = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as u32).unwrap_or(0);
    Msg::Time { unix, tz_min: crate::winutil::utc_offset_minutes() }
}

fn set_lyrics(m: Msg) {
    c().snap.lock().unwrap().lyrics = m.clone();
    send(m);
}

fn key_of(m: &Option<Media>) -> Option<TrackKey> {
    m.as_ref().map(|m| TrackKey { title: m.title.clone(), artist: m.artist.clone(), dur_s: (m.dur_ms + 500) / 1000 })
}

/// Starts a lyrics lookup for generation `generation` on its own thread; never blocks the caller
fn fetch_lyrics(generation: u64, key: TrackKey) {
    thread::spawn(move || {
        let dir = detent::settings::dir().join("lyrics");
        let found = match lyrics::get(&dir, &key.title, &key.artist, key.dur_s) {
            Ok(Found::Synced(l)) => Ok(Some(l)),
            Ok(Found::None) => Ok(None),
            Err(e) => {
                log(&format!("lyrics: {e}"));
                Err(())
            }
        };
        // Installed only if the track hasn't changed meanwhile (checked under the same lock)
        c().playing.lock().unwrap().lyrics.install(generation, found, Instant::now());
    });
}

/// Asks the tracker whether lyrics need clearing or fetching for what's playing now
fn check_lyrics() {
    let co = c();
    let (change, key) = {
        let mut p = co.playing.lock().unwrap();
        let key = key_of(&p.media);
        (p.lyrics.track(key.clone(), Instant::now()), key)
    };
    if change.new_track {
        set_lyrics(Msg::LyricsNone);
    }
    if let (Some(g), Some(k)) = (change.fetch, key) {
        fetch_lyrics(g, k);
    }
}

fn on_media(m: Option<Media>, show: &(dyn Fn(Option<Media>) + Send + Sync)) {
    let co = c();
    co.playing.lock().unwrap().media = m.clone();
    {
        let mut s = co.snap.lock().unwrap();
        s.now = Some(m.as_ref().map(now_playing).unwrap_or_else(nothing_playing));
        s.cover = m.as_ref().and_then(|m| Some((m.art_id, ART_KNOB as u16, ART_KNOB as u16, m.art_rgb565.clone()?)));
    }
    match &m {
        Some(m) => {
            send(now_playing(m));
            if let Some(px) = &m.art_rgb565 {
                let _ = co.tx.send(Out::Cover(m.art_id, ART_KNOB as u16, ART_KNOB as u16, px.clone()));
            }
        }
        None => send(nothing_playing()),
    }
    check_lyrics();
    show(m);
}

/// Every 200 ms: the lyric line; every 1 s: the time in the snapshot; every 2 s: PC stats;
/// every 5 s while playing: NOW_PLAYING again, so the knob's position doesn't drift.
fn ticker() {
    thread::spawn(|| {
        let mut sampler = Sampler::new();
        let mut tick: u64 = 0;
        let mut last_np = Instant::now();
        loop {
            thread::sleep(Duration::from_millis(200));
            tick += 1;
            let co = c();
            let (media, line) = {
                let mut p = co.playing.lock().unwrap();
                let line = p.media.as_ref().map(|m| m.pos_now()).and_then(|pos| p.lyrics.line(pos));
                (p.media.clone(), line)
            };
            if let Some(l) = line {
                set_lyrics(l);
            }
            check_focus();
            if let Some(m) = &media {
                if m.playing && last_np.elapsed() >= Duration::from_secs(5) {
                    last_np = Instant::now();
                    let np = now_playing(m);
                    co.snap.lock().unwrap().now = Some(np.clone());
                    send(np);
                }
            }
            if tick % 5 == 0 {
                co.snap.lock().unwrap().time = Some(time_msg());
                // retries a failed lyrics lookup once it's due
                check_lyrics();
            }
            if tick % 10 == 0 {
                let st = sampler.sample();
                co.snap.lock().unwrap().stats = Some(st.clone());
                if co.connected.load(Ordering::Relaxed) {
                    send(st);
                }
            }
        }
    });
}

/// Tells the knob when the program in front changes, and what a turn does there
fn check_focus() {
    let co = c();
    let pid = crate::focus::foreground_pid();
    if pid == 0 || pid == co.focus.lock().unwrap().pid {
        return;
    }
    let exe = crate::winutil::exe_of(pid).unwrap_or_default();
    let (mode, label) = apps::focus_for(&exe, &RULES.lock().unwrap());
    let mut f = co.focus.lock().unwrap();
    f.pid = pid;
    if (mode, &label) != (f.mode, &f.label) {
        f.mode = mode;
        f.label = label.clone();
        let m = Msg::Focus { mode, label };
        co.snap.lock().unwrap().focus = Some(m.clone());
        send(m);
    }
}

/// A knob turn in an app mode becomes wheel input in the program in front
fn turn(steps: i8) {
    let mode = c().focus.lock().unwrap().mode;
    if let Some(w) = apps::wheel_for(mode, steps) {
        crate::focus::inject(&w);
    }
}

/// Handles what the knob asks the PC to do; everything else goes to the app's handler
fn from_knob(m: Msg, rest: &(dyn Fn(Msg) + Send)) {
    let Some(co) = C.get() else { return };
    match m {
        Msg::SetAppVolume { id, vol, muted } => {
            let _ = co.audio.send(Cmd::App { id, vol, muted });
        }
        Msg::SetMic { muted } => {
            log(&format!("knob: mic {}", if muted { "muted" } else { "on" }));
            let _ = co.audio.send(Cmd::Mic(muted));
        }
        Msg::SetMonitor { bright } => {
            let _ = co.display.send(bright.min(100));
        }
        Msg::Turn { steps } => turn(steps),
        m => rest(m),
    }
}

/// Starts the media watcher, the knob link and the timers.
/// `show_media` runs for every now-playing change (for the popup), `on_conn` when the knob comes or goes,
/// `on_knob` for each message from the knob (LAMP_STATE, LOG).
/// `show_monitor` runs when the monitor's brightness is first read and whenever it changes.
pub fn start(
    show_media: impl Fn(Option<Media>) + Send + Sync + 'static,
    on_conn: impl Fn(bool) + Send + 'static,
    on_knob: impl Fn(Msg) + Send + 'static,
    show_monitor: impl Fn(Option<u8>) + Send + 'static,
) {
    let snap = Arc::new(Mutex::new(Snapshot { time: Some(time_msg()), ..Default::default() }));
    let tx = knob::start(
        snap.clone(),
        move |m| from_knob(m, &on_knob),
        move |up| {
            if let Some(co) = C.get() {
                co.connected.store(up, Ordering::Relaxed);
            }
            on_conn(up);
        },
    );
    // These report from their own threads, possibly before C is set, so they use their own handles
    let audio = audio::start({
        let (snap, tx) = (snap.clone(), tx.clone());
        move |a| {
            let (apps, mic) = (Msg::AudioApps(a.apps), Msg::Mic { muted: a.mic });
            {
                let mut s = snap.lock().unwrap();
                s.apps = Some(apps.clone());
                s.mic = Some(mic.clone());
            }
            let _ = tx.send(Out::Msg(apps));
            let _ = tx.send(Out::Msg(mic));
        }
    });
    let display = crate::display::start({
        let (snap, tx) = (snap.clone(), tx.clone());
        move |b| {
            let m = Msg::Monitor { bright: b };
            snap.lock().unwrap().monitor = Some(m.clone());
            let _ = tx.send(Out::Msg(m));
            show_monitor(b);
        }
    });
    let _ = C.set(Companion {
        snap,
        tx,
        playing: Mutex::new(Playing { media: None, lyrics: Tracker::default() }),
        connected: AtomicBool::new(false),
        audio,
        display,
        focus: Mutex::new(Focus { pid: 0, mode: 0, label: String::new() }),
    });
    media::start(move |m| on_media(m, &show_media));
    ticker();
}

pub fn connected() -> bool {
    C.get().is_some_and(|co| co.connected.load(Ordering::Relaxed))
}

/// Sends Wi-Fi and lamp details to the knob ("Set up knob"). The password isn't kept anywhere.
pub fn provision(ssid: String, pass: String, lamp_ip: String, lamp_token: String) -> bool {
    if !connected() {
        return false;
    }
    log(&format!("knob: sending set-up (Wi-Fi \"{ssid}\", lamp {lamp_ip})"));
    send(Msg::Provision { ssid, pass, lamp_ip, lamp_token });
    true
}

/// The per-app knob modes (from settings); the program in front is checked again
pub fn set_rules(rules: Vec<AppRule>) {
    *RULES.lock().unwrap() = rules;
    if let Some(co) = C.get() {
        co.focus.lock().unwrap().pid = 0;
    }
}

/// Monitor brightness from the popup
pub fn set_monitor(pct: u8) {
    if let Some(co) = C.get() {
        let _ = co.display.send(pct.min(100));
    }
}

/// Sends a firmware image to the knob; false when no knob is connected. `report` follows the update.
pub fn update_firmware(image: Vec<u8>, version: String, report: impl Fn(FwEvent) + Send + 'static) -> bool {
    if !connected() {
        return false;
    }
    let _ = c().tx.send(Out::Firmware { image, version, report: Box::new(report) });
    true
}
