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
