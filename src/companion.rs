// Collects what the knob shows (now playing, cover, lyrics, PC stats, time) and keeps it flowing over the link.
// The snapshot always holds the latest state, so a (re)connected knob gets everything at once.

use crate::knob::{self, Out};
use crate::media::{self, Media, ART_KNOB};
use crate::stats::Sampler;
use crate::winutil::log;
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

struct Companion {
    snap: Arc<Mutex<Snapshot>>,
    tx: Sender<Out>,
    playing: Mutex<Playing>,
    connected: AtomicBool,
}

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

/// Starts the media watcher, the knob link and the timers.
/// `show_media` runs for every now-playing change (for the popup), `on_conn` when the knob comes or goes,
/// `on_knob` for each message from the knob (LAMP_STATE, LOG).
pub fn start(
    show_media: impl Fn(Option<Media>) + Send + Sync + 'static,
    on_conn: impl Fn(bool) + Send + 'static,
    on_knob: impl Fn(Msg) + Send + 'static,
) {
    let snap = Arc::new(Mutex::new(Snapshot { time: Some(time_msg()), ..Default::default() }));
    let tx = knob::start(
        snap.clone(),
        on_knob,
        move |up| {
            if let Some(co) = C.get() {
                co.connected.store(up, Ordering::Relaxed);
            }
            on_conn(up);
        },
    );
    let _ = C.set(Companion {
        snap,
        tx,
        playing: Mutex::new(Playing { media: None, lyrics: Tracker::default() }),
        connected: AtomicBool::new(false),
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
