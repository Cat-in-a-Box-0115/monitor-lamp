//! Synced lyrics from LRCLIB (https://lrclib.net), cached on disk per track.
//! Only the knob shows them; nothing here is stored in the repo.

use crate::lrc::{self, Lrc};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub enum Found {
    Synced(Lrc),
    /// LRCLIB answered, but has no synced lyrics for this track (cached too)
    None,
}

fn cache_file(dir: &Path, title: &str, artist: &str, dur_s: u32) -> PathBuf {
    // FNV-1a: stable across runs and Rust versions, unlike DefaultHasher
    let mut h: u64 = 0xcbf29ce484222325;
    for b in format!("{title}\u{1}{artist}\u{1}{dur_s}").bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    dir.join(format!("{h:016x}.lrc"))
}

/// Lyrics for a track: from the cache in `cache_dir`, else from LRCLIB. `Err` = couldn't ask (offline, timeout),
/// which isn't cached so it's tried again next time the track plays.
pub fn get(cache_dir: &Path, title: &str, artist: &str, dur_s: u32) -> Result<Found, String> {
    let file = cache_file(cache_dir, title, artist, dur_s);
    if let Ok(text) = std::fs::read_to_string(&file) {
        return Ok(lrc::parse(&text).map(Found::Synced).unwrap_or(Found::None));
    }
    let synced = fetch(title, artist, dur_s)?;
    let _ = std::fs::create_dir_all(cache_dir);
    let _ = std::fs::write(&file, synced.as_deref().unwrap_or(""));
    Ok(synced.and_then(|t| lrc::parse(&t)).map(Found::Synced).unwrap_or(Found::None))
}

/// The synced LRC text, or None if LRCLIB has none
fn fetch(title: &str, artist: &str, dur_s: u32) -> Result<Option<String>, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(5))
        .user_agent(concat!("Detent/", env!("CARGO_PKG_VERSION")))
        .build();
    let synced = |v: &serde_json::Value| v["syncedLyrics"].as_str().filter(|s| !s.trim().is_empty()).map(String::from);

    // Exact match first (title, artist, duration within 2 s)
    let mut req = agent.get("https://lrclib.net/api/get").query("track_name", title).query("artist_name", artist);
    if dur_s > 0 {
        req = req.query("duration", &dur_s.to_string());
    }
    match req.call() {
        Ok(r) => {
            let v: serde_json::Value = r.into_json().map_err(|e| e.to_string())?;
            if let Some(s) = synced(&v) {
                return Ok(Some(s));
            }
        }
        Err(ureq::Error::Status(404, _)) => {}
        Err(e) => return Err(e.to_string()),
    }

    // Then search, preferring a result whose duration is close
    let r = agent
        .get("https://lrclib.net/api/search")
        .query("track_name", title)
        .query("artist_name", artist)
        .call()
        .map_err(|e| e.to_string())?;
    let list: Vec<serde_json::Value> = r.into_json().map_err(|e| e.to_string())?;
    let close = |v: &serde_json::Value| dur_s == 0 || (v["duration"].as_f64().unwrap_or(0.0) - dur_s as f64).abs() <= 3.0;
    Ok(list.iter().filter(|v| close(v)).find_map(synced))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_hit_skips_network_and_caches_none() {
        let dir = std::env::temp_dir().join(format!("detent-lyrics-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(cache_file(&dir, "Glass Garden", "The Placeholders", 200), "[00:01.00]paper moons\n").unwrap();
        std::fs::write(cache_file(&dir, "Night Ferry", "Mono Lake", 238), "").unwrap();
        assert!(matches!(get(&dir, "Glass Garden", "The Placeholders", 200), Ok(Found::Synced(_))));
        assert!(matches!(get(&dir, "Night Ferry", "Mono Lake", 238), Ok(Found::None)));
        assert_ne!(cache_file(&dir, "a", "b", 1), cache_file(&dir, "a", "b", 2));
    }
}

/// Which track lyrics belong to. Same track = same title and artist, and durations within 3 s
/// (live and studio versions differ in length); an unknown duration (0) matches any.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackKey {
    pub title: String,
    pub artist: String,
    pub dur_s: u32,
}

impl TrackKey {
    fn same(&self, o: &TrackKey) -> bool {
        self.title == o.title && self.artist == o.artist && (self.dur_s == 0 || o.dur_s == 0 || self.dur_s.abs_diff(o.dur_s) <= 3)
    }
}

/// What `Tracker::track` asks the caller to do
#[derive(Debug, PartialEq)]
pub struct TrackChange {
    /// A different track: clear the knob's lyrics now
    pub new_track: bool,
    /// Start a fetch with this generation (pass it back to `install`)
    pub fetch: Option<u64>,
}

/// The lyrics for whatever is playing, safe against fetches finishing after the track changed
#[derive(Default)]
pub struct Tracker {
    generation: u64,
    key: Option<TrackKey>,
    lrc: Option<Lrc>,
    failed_at: Option<std::time::Instant>,
    fetching: bool,
    last_line: Option<u32>,
}

/// How long to wait before asking LRCLIB again after a failed (not "no lyrics") lookup
pub const RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

impl Tracker {
    /// Call on every now-playing change (None = nothing playing)
    pub fn track(&mut self, key: Option<TrackKey>, now: std::time::Instant) -> TrackChange {
        let same = match (&self.key, &key) {
            (None, None) => return TrackChange { new_track: false, fetch: None },
            (Some(a), Some(b)) => a.same(b),
            _ => false,
        };
        if !same {
            self.generation += 1;
            self.lrc = None;
            self.failed_at = None;
            self.last_line = None;
            self.fetching = key.is_some();
            let fetch = key.is_some().then_some(self.generation);
            self.key = key;
            return TrackChange { new_track: true, fetch };
        }
        let key = key.unwrap();
        let old_dur = self.key.as_ref().map_or(0, |k| k.dur_s);
        if old_dur == 0 && key.dur_s > 0 {
            self.key = Some(key);
            // The first lookup had no duration to match on; try again with it
            if self.lrc.is_none() {
                self.generation += 1;
                self.fetching = true;
                self.failed_at = None;
                return TrackChange { new_track: false, fetch: Some(self.generation) };
            }
        }
        if self.lrc.is_none() && !self.fetching && self.failed_at.is_some_and(|t| now.duration_since(t) >= RETRY_AFTER) {
            self.generation += 1;
            self.fetching = true;
            self.failed_at = None;
            return TrackChange { new_track: false, fetch: Some(self.generation) };
        }
        TrackChange { new_track: false, fetch: None }
    }

    /// A fetch finished. Ignored (returns false) if it belongs to an older generation.
    /// `Err` = couldn't ask; retried after RETRY_AFTER.
    pub fn install(&mut self, generation: u64, found: Result<Option<Lrc>, ()>, now: std::time::Instant) -> bool {
        if generation != self.generation {
            return false;
        }
        self.fetching = false;
        match found {
            Ok(l) => {
                self.lrc = l;
                self.failed_at = None;
                self.last_line = None;
            }
            Err(()) => self.failed_at = Some(now),
        }
        true
    }

    /// The lyric message for `pos_ms`, only when the line changed since the last call
    pub fn line(&mut self, pos_ms: u32) -> Option<crate::protocol::Msg> {
        let (id, prev, cur, next) = self.lrc.as_ref()?.at(pos_ms)?;
        if self.last_line == Some(id) {
            return None;
        }
        self.last_line = Some(id);
        Some(crate::protocol::Msg::Lyrics { line_id: id, prev: prev.into(), cur: cur.into(), next: next.into() })
    }
}

#[cfg(test)]
mod tracker_tests {
    use super::*;
    use crate::protocol::Msg;
    use std::time::{Duration, Instant};

    fn key(t: &str, d: u32) -> Option<TrackKey> {
        Some(TrackKey { title: t.into(), artist: "Mono Lake".into(), dur_s: d })
    }
    fn lrc() -> Lrc {
        lrc::parse("[00:01.00]one\n[00:02.00]two\n").unwrap()
    }

    #[test]
    fn stale_fetch_cannot_install() {
        let mut t = Tracker::default();
        let now = Instant::now();
        let a = t.track(key("Night Ferry", 238), now).fetch.unwrap();
        let b = t.track(key("Slow Orbit", 200), now);
        assert!(b.new_track);
        assert!(!t.install(a, Ok(Some(lrc())), now));
        assert_eq!(t.line(1500), None);
        assert!(t.install(b.fetch.unwrap(), Ok(Some(lrc())), now));
        assert!(matches!(t.line(1500), Some(Msg::Lyrics { line_id: 1, .. })));
    }

    #[test]
    fn same_title_different_duration_is_new_track() {
        let mut t = Tracker::default();
        let now = Instant::now();
        t.track(key("Night Ferry", 238), now);
        let c = t.track(key("Night Ferry", 301), now);
        assert!(c.new_track && c.fetch.is_some());
        // within 3 s is the same track, nothing to do
        assert_eq!(t.track(key("Night Ferry", 302), now), TrackChange { new_track: false, fetch: None });
    }

    #[test]
    fn duration_known_later_refetches_without_clearing() {
        let mut t = Tracker::default();
        let now = Instant::now();
        let g = t.track(key("Night Ferry", 0), now).fetch.unwrap();
        t.install(g, Ok(None), now);
        let c = t.track(key("Night Ferry", 238), now);
        assert!(!c.new_track);
        assert!(c.fetch.is_some());
        // and only once
        assert_eq!(t.track(key("Night Ferry", 238), now).fetch, None);
    }

    #[test]
    fn failed_fetch_retried_later() {
        let mut t = Tracker::default();
        let now = Instant::now();
        let g = t.track(key("Night Ferry", 238), now).fetch.unwrap();
        assert!(t.install(g, Err(()), now));
        assert_eq!(t.track(key("Night Ferry", 238), now + Duration::from_secs(5)).fetch, None);
        let r = t.track(key("Night Ferry", 238), now + RETRY_AFTER + Duration::from_secs(1));
        assert!(!r.new_track && r.fetch.is_some());
        // "no lyrics" is final for this track
        t.install(r.fetch.unwrap(), Ok(None), now);
        assert_eq!(t.track(key("Night Ferry", 238), now + RETRY_AFTER * 3).fetch, None);
    }

    #[test]
    fn line_reported_once_per_change() {
        let mut t = Tracker::default();
        let now = Instant::now();
        let g = t.track(key("Glass Garden", 200), now).fetch.unwrap();
        t.install(g, Ok(Some(lrc())), now);
        assert!(matches!(t.line(1200), Some(Msg::Lyrics { line_id: 1, .. })));
        assert_eq!(t.line(1300), None);
        assert!(matches!(t.line(2100), Some(Msg::Lyrics { line_id: 2, .. })));
        // seek back
        assert!(matches!(t.line(1100), Some(Msg::Lyrics { line_id: 1, .. })));
        // nothing playing clears it
        assert!(t.track(None, now).new_track);
        assert_eq!(t.line(1100), None);
    }
}
