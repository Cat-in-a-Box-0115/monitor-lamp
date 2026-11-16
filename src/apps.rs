//! Per-app volume and per-app knob modes: the platform-neutral parts (naming, grouping, rules).

use crate::protocol::{AudioApp, MAX_APPS, MODE_SCROLL, MODE_SIDEWAYS, MODE_VOLUME, MODE_ZOOM};
use serde::{Deserialize, Serialize};

/// "C:\\...\\Spotify.exe" or "Spotify.exe" -> "Spotify"
pub fn display_name(exe: &str) -> String {
    let file = exe.rsplit(['\\', '/']).next().unwrap_or(exe);
    let stem = file.strip_suffix(".exe").or_else(|| file.strip_suffix(".EXE")).unwrap_or(file);
    let known = [
        ("msedge", "Edge"),
        ("chrome", "Chrome"),
        ("firefox", "Firefox"),
        ("Fusion360", "Fusion 360"),
        ("Discord", "Discord"),
        ("Spotify", "Spotify"),
        ("vlc", "VLC"),
        ("steam", "Steam"),
        ("steamwebhelper", "Steam"),
    ];
    for (k, v) in known {
        if stem.eq_ignore_ascii_case(k) {
            return v.into();
        }
    }
    stem.to_string()
}

/// A stable id for an app: FNV-1a of its lowercased display name, so it survives restarts and process changes
pub fn app_id(name: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in name.to_lowercase().bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// One Windows audio session, as read from the mixer
#[derive(Clone, Debug)]
pub struct Session {
    pub exe: String,
    pub vol: f32,
    pub muted: bool,
}

/// Turns sessions into the knob's app list: one entry per app (Chrome's many sessions become one), in first-seen
/// order, at most MAX_APPS. An app shows the loudest of its sessions' volumes and is muted only if all are.
pub fn group(sessions: &[Session]) -> Vec<AudioApp> {
    let mut out: Vec<AudioApp> = Vec::new();
    for s in sessions {
        let name = display_name(&s.exe);
        if name.is_empty() {
            continue;
        }
        let vol = (s.vol.clamp(0.0, 1.0) * 100.0).round() as u8;
        match out.iter_mut().find(|a| a.name == name) {
            Some(a) => {
                a.vol = a.vol.max(vol);
                a.muted &= s.muted;
            }
            None => out.push(AudioApp { id: app_id(&name), vol, muted: s.muted, name }),
        }
    }
    out.truncate(MAX_APPS);
    out
}

/// "This app gets this knob mode", kept in settings.json as `appModes`
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AppRule {
    /// The program's file name, e.g. "Fusion360.exe" (case doesn't matter)
    pub exe: String,
    /// "scroll", "zoom" (ctrl + wheel) or "sideways"
    pub mode: String,
    /// What the knob shows; the app's name when empty
    #[serde(default)]
    pub label: String,
}

pub fn default_rules() -> Vec<AppRule> {
    let r = |exe: &str, mode: &str, label: &str| AppRule { exe: exe.into(), mode: mode.into(), label: label.into() };
    vec![
        // Fusion and KiCad zoom with the plain wheel
        r("Fusion360.exe", "scroll", "Fusion · zoom"),
        r("kicad.exe", "scroll", "KiCad · zoom"),
        r("pcbnew.exe", "scroll", "PCB · zoom"),
        r("eeschema.exe", "scroll", "Schematic · zoom"),
        r("Photoshop.exe", "zoom", "Photoshop · zoom"),
        r("Figma.exe", "zoom", "Figma · zoom"),
    ]
}

fn mode_code(mode: &str) -> u8 {
    match mode {
        "scroll" => MODE_SCROLL,
        "zoom" => MODE_ZOOM,
        "sideways" => MODE_SIDEWAYS,
        _ => MODE_VOLUME,
    }
}

/// The FOCUS message for the program in front: (mode, label). Programs without a rule keep the knob on volume.
pub fn focus_for(exe: &str, rules: &[AppRule]) -> (u8, String) {
    let file = exe.rsplit(['\\', '/']).next().unwrap_or(exe);
    match rules.iter().find(|r| r.exe.eq_ignore_ascii_case(file)) {
        Some(r) => {
            let label = if r.label.is_empty() { display_name(file) } else { r.label.clone() };
            (mode_code(&r.mode), label)
        }
        None => (MODE_VOLUME, display_name(file)),
    }
}

/// What to inject for a knob TURN in a mode
#[derive(Debug, PartialEq)]
pub struct Wheel {
    /// WHEEL_DELTA units; positive = up / right
    pub delta: i32,
    pub horizontal: bool,
    pub ctrl: bool,
}

/// A clockwise turn scrolls down (like a wheel rolled towards you), zooms in and moves right
pub fn wheel_for(mode: u8, steps: i8) -> Option<Wheel> {
    let d = steps as i32 * 120;
    match mode {
        MODE_SCROLL => Some(Wheel { delta: -d, horizontal: false, ctrl: false }),
        MODE_ZOOM => Some(Wheel { delta: d, horizontal: false, ctrl: true }),
        MODE_SIDEWAYS => Some(Wheel { delta: d, horizontal: true, ctrl: false }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_friendly() {
        assert_eq!(display_name(r"C:\Program Files\Spotify\Spotify.exe"), "Spotify");
        assert_eq!(display_name("msedge.exe"), "Edge");
        assert_eq!(display_name("SomeGame.EXE"), "SomeGame");
    }

    #[test]
    fn sessions_group_by_app() {
        let s = |exe: &str, vol: f32, muted: bool| Session { exe: exe.into(), vol, muted };
        let apps = group(&[s("chrome.exe", 0.4, true), s("Spotify.exe", 0.8, false), s("chrome.exe", 0.6, false), s("", 1.0, false)]);
        assert_eq!(apps.len(), 2);
        assert_eq!((apps[0].name.as_str(), apps[0].vol, apps[0].muted), ("Chrome", 60, false));
        assert_eq!((apps[1].name.as_str(), apps[1].vol), ("Spotify", 80));
        assert_eq!(apps[0].id, app_id("chrome"), "id ignores case and survives restarts");
        let many: Vec<Session> = (0..20).map(|i| s(&format!("a{i}.exe"), 0.5, false)).collect();
        assert_eq!(group(&many).len(), MAX_APPS);
    }

    #[test]
    fn focus_follows_rules() {
        let rules = default_rules();
        assert_eq!(focus_for(r"C:\x\Fusion360.exe", &rules), (MODE_SCROLL, "Fusion · zoom".into()));
        assert_eq!(focus_for("PHOTOSHOP.EXE", &rules).0, MODE_ZOOM);
        assert_eq!(focus_for("chrome.exe", &rules), (MODE_VOLUME, "Chrome".into()));
        let custom = vec![AppRule { exe: "excel.exe".into(), mode: "sideways".into(), label: String::new() }];
        assert_eq!(focus_for("EXCEL.EXE", &custom), (MODE_SIDEWAYS, "EXCEL".into()));
    }

    #[test]
    fn turns_become_wheel_input() {
        assert_eq!(wheel_for(MODE_VOLUME, 3), None);
        assert_eq!(wheel_for(MODE_SCROLL, 2), Some(Wheel { delta: -240, horizontal: false, ctrl: false }));
        assert_eq!(wheel_for(MODE_ZOOM, -1), Some(Wheel { delta: -120, horizontal: false, ctrl: true }));
        assert_eq!(wheel_for(MODE_SIDEWAYS, 1), Some(Wheel { delta: 120, horizontal: true, ctrl: false }));
    }
}
