// Time-of-day lighting: cool, full light during the day; warmer and dimmer
// in the hours before bed so the evening light doesn't fight your sleep.

use crate::settings::Circadian;

const RAMP_UP: f64 = 60.0; // minutes after wake-up to reach daytime light

#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub ct: u32,
    pub factor: f64,
    pub phase: &'static str,
}

pub fn to_minutes(hhmm: &str) -> Option<f64> {
    let (h, m) = hhmm.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.trim().parse().ok()?, m.trim().parse().ok()?);
    (h < 24 && m < 60).then_some((h * 60 + m) as f64)
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

fn smooth(t: f64) -> f64 {
    t * t * (3.0 - 2.0 * t)
}

/// Target for a time given as minutes since midnight
pub fn target_at(minute_of_day: f64, c: &Circadian) -> Target {
    let wake = to_minutes(&c.wake).unwrap_or(450.0);
    let bed = to_minutes(&c.bed).unwrap_or(1380.0);
    let mut awake = (bed - wake + 1440.0) % 1440.0; // length of the day in minutes
    if awake == 0.0 {
        awake = 1440.0;
    }
    let wind_down = (c.wind_down as f64).min(awake - RAMP_UP).max(0.0);
    let t = (minute_of_day - wake + 1440.0) % 1440.0; // minutes since wake-up

    let (k, phase) = if t < RAMP_UP {
        (t / RAMP_UP, "Waking up")
    } else if t < awake - wind_down {
        (1.0, "Daytime")
    } else if t < awake {
        (1.0 - (t - (awake - wind_down)) / wind_down, "Winding down")
    } else {
        (0.0, "Night")
    };
    let k = smooth(k);
    Target {
        ct: (lerp(c.night_temp as f64, c.day_temp as f64, k) / 50.0).round() as u32 * 50,
        factor: if c.dim { lerp(c.night_level as f64 / 100.0, 1.0, k) } else { 1.0 },
        phase,
    }
}

pub fn target_now(c: &Circadian) -> Target {
    let (h, m, s) = crate::winutil::local_hms();
    target_at(h as f64 * 60.0 + m as f64 + s as f64 / 60.0, c)
}

/// Rough colour of a colour temperature, for previews
pub fn kelvin_rgb(k: u32) -> (u8, u8, u8) {
    let t = k as f64 / 100.0;
    let c = |v: f64| v.round().clamp(0.0, 255.0) as u8;
    let r = if t <= 66.0 { 255 } else { c(329.7 * (t - 60.0).powf(-0.1332)) };
    let g = if t <= 66.0 { c(99.47 * t.ln() - 161.12) } else { c(288.12 * (t - 60.0).powf(-0.0755)) };
    let b = if t >= 66.0 { 255 } else if t <= 19.0 { 0 } else { c(138.52 * (t - 10.0).ln() - 305.04) };
    (r, g, b)
}
