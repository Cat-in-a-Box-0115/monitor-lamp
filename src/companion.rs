// Collects what the knob shows (now playing, cover, lyrics, PC stats, time) and keeps it flowing over the link.
// The snapshot always holds the latest state, so a (re)connected knob gets everything at once.

use crate::knob::{self, Out};
use crate::media::{self, Media, ART_KNOB};
use crate::stats::Sampler;
use crate::winutil::log;
use detent::link::Snapshot;
use detent::lrc::Lrc;
use detent::lyrics::{self, Found};
use detent::protocol::Msg;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Companion {
    snap: Arc<Mutex<Snapshot>>,
    tx: Sender<Out>,
    media: Mutex<Option<Media>>,
    /// Lyrics for the current track; the generation tells a stale fetch from a current one
    lrc: Mutex<Option<Lrc>>,
    generation: AtomicU64,
    last_line: Mutex<Option<u32>>,
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

fn on_media(m: Option<Media>, show: &(dyn Fn(Option<Media>) + Send + Sync)) {
    let co = c();
    let prev = co.media.lock().unwrap().clone();
    let new_track = match (&prev, &m) {
        (Some(p), Some(n)) => p.title != n.title || p.artist != n.artist,
        (None, None) => false,
        _ => true,
    };
    *co.media.lock().unwrap() = m.clone();
    {
        let mut s = co.snap.lock().unwrap();
        s.now = m.as_ref().map(now_playing);
        s.cover = m.as_ref().and_then(|m| Some((m.art_id, ART_KNOB as u16, ART_KNOB as u16, m.art_rgb565.clone()?)));
    }
    match &m {
        Some(m) => {
            send(now_playing(m));
            if let Some(px) = &m.art_rgb565 {
                let _ = co.tx.send(Out::Cover(m.art_id, ART_KNOB as u16, ART_KNOB as u16, px.clone()));
            }
        }
        None => send(Msg::NowPlaying { playing: false, pos_ms: 0, dur_ms: 0, title: String::new(), artist: String::new() }),
    }

    if new_track {
        let generation = co.generation.fetch_add(1, Ordering::SeqCst) + 1;
        *co.lrc.lock().unwrap() = None;
        *co.last_line.lock().unwrap() = None;
        set_lyrics(Msg::LyricsNone);
        if let Some(m) = m.clone() {
            thread::spawn(move || {
                let dir = detent::settings::dir().join("lyrics");
                let found = lyrics::get(&dir, &m.title, &m.artist, (m.dur_ms + 500) / 1000);
                if c().generation.load(Ordering::SeqCst) != generation {
                    return; // the track changed meanwhile
                }
                match found {
                    Ok(Found::Synced(l)) => *c().lrc.lock().unwrap() = Some(l),
                    Ok(Found::None) => {}
                    Err(e) => log(&format!("lyrics: {e}")),
                }
            });
        }
    }
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
            let media = co.media.lock().unwrap().clone();

            if let Some(m) = &media {
                let line = co.lrc.lock().unwrap().as_ref().and_then(|l| {
                    let (id, p, cur, n) = l.at(m.pos_now())?;
                    Some(Msg::Lyrics { line_id: id, prev: p.into(), cur: cur.into(), next: n.into() })
                });
                if let Some(Msg::Lyrics { line_id, .. }) = &line {
                    let mut last = co.last_line.lock().unwrap();
                    if *last != Some(*line_id) {
                        *last = Some(*line_id);
                        drop(last);
                        set_lyrics(line.unwrap());
                    }
                }
                if m.playing && last_np.elapsed() >= Duration::from_secs(5) {
                    last_np = Instant::now();
                    let np = now_playing(m);
                    co.snap.lock().unwrap().now = Some(np.clone());
                    send(np);
                }
            }
            if tick % 5 == 0 {
                co.snap.lock().unwrap().time = Some(time_msg());
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
        media: Mutex::new(None),
        lrc: Mutex::new(None),
        generation: AtomicU64::new(0),
        last_line: Mutex::new(None),
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
