// Settings persisted as JSON in %APPDATA%\Monitor Lamp\settings.json
// (same file and format as the earlier Electron version, so nothing is lost).

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
    PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default()).join("Monitor Lamp")
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
