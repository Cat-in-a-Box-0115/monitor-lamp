// Settings persisted as JSON in %APPDATA%\Detent\settings.json. Same format as Monitor Lamp (and its
// Electron predecessor); the old file is copied across once by `migrate_from`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub ip: String,
    pub token: String,
    pub open_at_login: bool,
    pub start_hidden: bool,
    pub close_to_tray: bool,
    pub on_at_login: bool,
    pub backlight_with_main: bool,
    pub off_at_shutdown: bool,
    pub sleep_wake: bool,
    pub lock_unlock: bool,
    /// Brightness the user picked; time-of-day dimming scales from this
    pub user_brightness: Option<u32>,
    pub flow_speed: u32,
    /// The running colour flow, resumed on restart
    pub flow: Option<Flow>,
    pub sync_smoothing: u32,
    pub circadian: Circadian,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Flow {
    pub name: String,
    pub colors: Vec<u32>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase", default)]
pub struct Circadian {
    pub enabled: bool,
    pub wake: String,
    pub bed: String,
    pub wind_down: u32,
    pub day_temp: u32,
    pub night_temp: u32,
    pub dim: bool,
    pub night_level: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            ip: String::new(),
            token: String::new(),
            open_at_login: true,
            start_hidden: true,
            close_to_tray: true,
            on_at_login: true,
            backlight_with_main: true,
            off_at_shutdown: true,
            sleep_wake: true,
            lock_unlock: false,
            user_brightness: None,
            flow_speed: 4,
            flow: None,
            sync_smoothing: 3,
            circadian: Circadian::default(),
        }
    }
}

impl Default for Circadian {
    fn default() -> Self {
        Circadian {
            enabled: false,
            wake: "07:30".into(),
            bed: "23:00".into(),
            wind_down: 180,
            day_temp: 5000,
            night_temp: 2700,
            dim: true,
            night_level: 40,
        }
    }
}

pub fn dir() -> PathBuf {
    PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default()).join("Detent")
}

/// Where Monitor Lamp (Detent's earlier name) kept its settings
pub fn old_file() -> PathBuf {
    PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default()).join("Monitor Lamp").join("settings.json")
}

/// Copies `old` to `new` once: only when `new` doesn't exist yet and `old` does. Returns whether it copied.
pub fn migrate_from(old: &std::path::Path, new: &std::path::Path) -> std::io::Result<bool> {
    if new.exists() || !old.exists() {
        return Ok(false);
    }
    if let Some(dir) = new.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::copy(old, new)?;
    Ok(true)
}

pub fn file() -> PathBuf {
    dir().join("settings.json")
}

/// The settings, plus why they fell back to defaults if they did
pub fn load() -> (Settings, Option<String>) {
    let path = file();
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(s) => (s, None),
            Err(e) => (Settings::default(), Some(format!("{}: {e}", path.display()))),
        },
        Err(e) => (Settings::default(), Some(format!("{}: {e}", path.display()))),
    }
}

pub fn save(s: &Settings) -> std::io::Result<()> {
    std::fs::create_dir_all(dir())?;
    std::fs::write(file(), serde_json::to_string_pretty(s).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("detent-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn migrate_copies_old_settings_once() {
        let d = tmp("migrate");
        let old = d.join("Monitor Lamp").join("settings.json");
        let new = d.join("Detent").join("settings.json");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, "{\"ip\":\"10.0.0.16\"}").unwrap();
        assert!(migrate_from(&old, &new).unwrap());
        assert_eq!(std::fs::read_to_string(&new).unwrap(), "{\"ip\":\"10.0.0.16\"}");
        std::fs::write(&old, "{\"ip\":\"changed\"}").unwrap();
        assert!(!migrate_from(&old, &new).unwrap());
        assert_eq!(std::fs::read_to_string(&new).unwrap(), "{\"ip\":\"10.0.0.16\"}");

        let none_old = d.join("nope.json");
        let none_new = d.join("x").join("settings.json");
        assert!(!migrate_from(&none_old, &none_new).unwrap());
        assert!(!none_new.exists());
    }
}
