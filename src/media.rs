// Now playing from Windows' media controls (SMTC): Melodia, browsers (YouTube), Spotify and anything else that
// publishes a media session. Event-driven, with a 1 s resync while playing.

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows::Foundation::TypedEventHandler;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session, GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
};
use windows::Storage::Streams::DataReader;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

/// Cover sizes: square, for the popup (42 px at 2x) and for the knob
pub const ART_POPUP: u32 = 84;
pub const ART_KNOB: u32 = 180;

#[derive(Clone, Debug)]
pub struct Media {
    pub playing: bool,
    pub title: String,
    pub artist: String,
    pub pos_ms: u32,
    pub dur_ms: u32,
    /// When pos_ms was true; while playing, the position now is pos_ms + at.elapsed()
    pub at: Instant,
    /// Changes whenever the track or its cover changes (0 = no cover)
    pub art_id: u32,
    pub art_rgba: Option<Vec<u8>>,
    pub art_rgb565: Option<Vec<u8>>,
}

impl Media {
    pub fn pos_now(&self) -> u32 {
        if self.playing {
            let p = self.pos_ms as u64 + self.at.elapsed().as_millis() as u64;
            p.min(if self.dur_ms > 0 { self.dur_ms as u64 } else { u64::MAX }).min(u32::MAX as u64) as u32
        } else {
            self.pos_ms
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum MediaKey {
    PlayPause,
    Next,
    Prev,
}

fn fnv(h: u32, bytes: &[u8]) -> u32 {
    bytes.iter().fold(h, |h, b| (h ^ *b as u32).wrapping_mul(16_777_619))
}

/// The cover, center-cropped square: (RGBA at ART_POPUP, RGB565 LE at ART_KNOB)
fn decode_art(bytes: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let img = image::load_from_memory(bytes).ok()?;
    let side = img.width().min(img.height());
    let sq = img.crop_imm((img.width() - side) / 2, (img.height() - side) / 2, side, side);
    let small = sq.resize_exact(ART_POPUP, ART_POPUP, image::imageops::FilterType::Triangle).to_rgba8().into_raw();
    let big = sq.resize_exact(ART_KNOB, ART_KNOB, image::imageops::FilterType::Triangle).to_rgb8();
    let mut rgb565 = Vec::with_capacity((ART_KNOB * ART_KNOB * 2) as usize);
    for p in big.pixels() {
        let v = ((p[0] as u16 >> 3) << 11) | ((p[1] as u16 >> 2) << 5) | (p[2] as u16 >> 3);
        rgb565.extend_from_slice(&v.to_le_bytes());
    }
    Some((small, rgb565))
}

fn read_thumbnail(props: &windows::Media::Control::GlobalSystemMediaTransportControlsSessionMediaProperties) -> Option<Vec<u8>> {
    let stream = props.Thumbnail().ok()?.OpenReadAsync().ok()?.get().ok()?;
    let size = stream.Size().ok()? as u32;
    if size == 0 || size > 8 << 20 {
        return None;
    }
    let reader = DataReader::CreateDataReader(&stream).ok()?;
    reader.LoadAsync(size).ok()?.get().ok()?;
    let mut buf = vec![0u8; size as usize];
    reader.ReadBytes(&mut buf).ok()?;
    Some(buf)
}

/// 100 ns ticks since 1601, now
fn now_ticks() -> i64 {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    (d.as_nanos() / 100) as i64 + 116_444_736_000_000_000
}

/// Everything except the cover, which is only read when the track changes
fn read_basic(s: &Session) -> Option<(bool, String, String, u32, u32)> {
    let props = s.TryGetMediaPropertiesAsync().ok()?.get().ok()?;
    let title = props.Title().map(|t| t.to_string()).unwrap_or_default();
    let artist = props.Artist().map(|t| t.to_string()).unwrap_or_default();
    if title.is_empty() {
        return None;
    }
    let playing = s.GetPlaybackInfo().ok().and_then(|p| p.PlaybackStatus().ok()) == Some(Status::Playing);
    let (mut pos_ms, mut dur_ms) = (0u32, 0u32);
    if let Ok(t) = s.GetTimelineProperties() {
        let pos = t.Position().map(|d| d.Duration).unwrap_or(0);
        let end = t.EndTime().map(|d| d.Duration).unwrap_or(0);
        let start = t.StartTime().map(|d| d.Duration).unwrap_or(0);
        let mut pos = pos - start;
        // The session reports the position as of LastUpdatedTime; move it on to now while playing
        if playing {
            if let Ok(u) = t.LastUpdatedTime() {
                let since = now_ticks() - u.UniversalTime;
                if since > 0 && since < 6 * 3600 * 10_000_000 {
                    pos += since;
                }
            }
        }
        dur_ms = ((end - start).max(0) / 10_000) as u32;
        pos_ms = (pos.max(0) / 10_000) as u32;
        if dur_ms > 0 {
            pos_ms = pos_ms.min(dur_ms);
        }
    }
    Some((playing, title, artist, pos_ms, dur_ms))
}

/// An event handler that only wakes the watcher
fn wake<A>(tx: mpsc::Sender<()>) -> impl FnMut(&Option<Session>, &Option<A>) -> windows::core::Result<()> + Send + 'static {
    move |_, _| {
        let _ = tx.send(());
        Ok(())
    }
}

/// Watches the current media session on its own thread; `on_change` gets every change (None = nothing playing).
pub fn start(on_change: impl Fn(Option<Media>) + Send + 'static) {
    thread::spawn(move || {
        unsafe {
            let _ = RoInitialize(RO_INIT_MULTITHREADED);
        }
        let mgr = loop {
            match Manager::RequestAsync().and_then(|op| op.get()) {
                Ok(m) => break m,
                Err(e) => {
                    crate::winutil::log(&format!("media: no session manager ({e}), retrying"));
                    thread::sleep(Duration::from_secs(30));
                }
            }
        };
        let (tx, rx) = mpsc::channel::<()>();
        let t = tx.clone();
        let _ = mgr.CurrentSessionChanged(&TypedEventHandler::new(move |_, _| {
            let _ = t.send(());
            Ok(())
        }));

        let mut session: Option<Session> = None;
        let mut tokens = None;
        let mut last: Option<Media> = None;
        let mut delivered: Option<Media> = None;
        let mut sent_none = false;
        loop {
            let cur = mgr.GetCurrentSession().ok();
            if cur != session {
                if let (Some(s), Some((a, b, c))) = (&session, tokens.take()) {
                    let _ = s.RemoveMediaPropertiesChanged(a);
                    let _ = s.RemovePlaybackInfoChanged(b);
                    let _ = s.RemoveTimelinePropertiesChanged(c);
                }
                if let Some(s) = &cur {
                    let a = s.MediaPropertiesChanged(&TypedEventHandler::new(wake(tx.clone())));
                    let b = s.PlaybackInfoChanged(&TypedEventHandler::new(wake(tx.clone())));
                    let c = s.TimelinePropertiesChanged(&TypedEventHandler::new(wake(tx.clone())));
                    if let (Ok(a), Ok(b), Ok(c)) = (a, b, c) {
                        tokens = Some((a, b, c));
                    }
                }
                session = cur;
            }

            let basic = session.as_ref().and_then(read_basic);
            match basic {
                None => {
                    if !sent_none {
                        last = None;
                        delivered = None;
                        sent_none = true;
                        on_change(None);
                    }
                }
                Some((playing, title, artist, pos_ms, dur_ms)) => {
                    let same_track = last.as_ref().is_some_and(|l| l.title == title && l.artist == artist);
                    // Reuse the cover for the same track, unless it had none yet (browsers often publish
                    // the cover a moment after the title)
                    let (art_id, art_rgba, art_rgb565) = if same_track && last.as_ref().is_some_and(|l| l.art_id != 0) {
                        let l = last.as_ref().unwrap();
                        (l.art_id, l.art_rgba.clone(), l.art_rgb565.clone())
                    } else {
                        let props = session.as_ref().and_then(|s| s.TryGetMediaPropertiesAsync().ok()?.get().ok());
                        let thumb = props.as_ref().and_then(read_thumbnail);
                        let art = thumb.as_deref().and_then(decode_art);
                        let id = fnv(fnv(2_166_136_261, title.as_bytes()), artist.as_bytes());
                        match art {
                            Some((a, b)) => (fnv(id, &b).max(1), Some(a), Some(b)),
                            None => (0, None, None),
                        }
                    };
                    let m = Media { playing, title, artist, pos_ms, dur_ms, at: Instant::now(), art_id, art_rgba, art_rgb565 };
                    // Report real changes only: state, track, cover, or the position jumping (a seek).
                    // Compared with what was last *reported*, so small seeks add up and get reported too.
                    let changed = match &delivered {
                        None => true,
                        Some(l) => {
                            l.playing != m.playing
                                || !same_track
                                || l.art_id != m.art_id
                                || l.dur_ms != m.dur_ms
                                || (l.pos_now() as i64 - m.pos_ms as i64).abs() > 1500
                        }
                    };
                    sent_none = false;
                    if changed {
                        delivered = Some(m.clone());
                        on_change(Some(m.clone()));
                    }
                    last = Some(m);
                }
            }

            // Wait for an event; while playing, re-check every second (covers that arrive late, seeks
            // some players don't announce)
            let playing = last.as_ref().is_some_and(|l| l.playing);
            let wait = if playing || (last.is_some() && last.as_ref().unwrap().art_id == 0) { 1 } else { 5 };
            let _ = rx.recv_timeout(Duration::from_secs(wait));
            while rx.try_recv().is_ok() {}
        }
    });
}

/// Sends a media key to the current session (play/pause, next, previous), off the calling thread
pub fn key(k: MediaKey) {
    thread::spawn(move || {
        unsafe {
            let _ = RoInitialize(RO_INIT_MULTITHREADED);
        }
        let Ok(s) = Manager::RequestAsync().and_then(|op| op.get()).and_then(|m| m.GetCurrentSession()) else { return };
        let r = match k {
            MediaKey::PlayPause => s.TryTogglePlayPauseAsync(),
            MediaKey::Next => s.TrySkipNextAsync(),
            MediaKey::Prev => s.TrySkipPreviousAsync(),
        };
        let _ = r.and_then(|op| op.get());
    });
}
