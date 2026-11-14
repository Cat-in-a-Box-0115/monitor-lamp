// Colour flows driven by the app rather than the lamp's built-in colour flow
// (which steps visibly between colours on this model). Colours are blended
// along the hue wheel with easing and streamed ~20 times a second.

use crate::lamp::Lamp;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_millis(50);

pub fn to_hsv(rgb: u32) -> (f64, f64, f64) {
    let r = (rgb >> 16 & 255) as f64 / 255.0;
    let g = (rgb >> 8 & 255) as f64 / 255.0;
    let b = (rgb & 255) as f64 / 255.0;
    let max = r.max(g).max(b);
    let d = max - r.min(g).min(b);
    let mut h = 0.0;
    if d > 0.0 {
        h = if max == r { ((g - b) / d) % 6.0 } else if max == g { (b - r) / d + 2.0 } else { (r - g) / d + 4.0 };
    }
    ((h * 60.0 + 360.0) % 360.0, if max > 0.0 { d / max } else { 0.0 }, max)
}

pub fn from_hsv(h: f64, s: f64, v: f64) -> u32 {
    let f = |n: f64| {
        let k = (n + h / 60.0) % 6.0;
        (255.0 * (v - v * s * k.min(4.0 - k).clamp(0.0, 1.0))).round() as u32
    };
    (f(5.0) << 16) | (f(3.0) << 8) | f(1.0)
}

/// Blend a→b by t (0..1), going the short way round the hue wheel
pub fn mix(a: u32, b: u32, t: f64) -> u32 {
    let (h1, s1, v1) = to_hsv(a);
    let (h2, s2, v2) = to_hsv(b);
    let mut dh = h2 - h1;
    if dh > 180.0 {
        dh -= 360.0;
    }
    if dh < -180.0 {
        dh += 360.0;
    }
    from_hsv((h1 + dh * t + 360.0) % 360.0, s1 + (s2 - s1) * t, v1 + (v2 - v1) * t)
}

struct Inner {
    colors: Vec<u32>,
    step_ms: f64,
    phase: f64, // position along the colour list, in steps
}

pub struct FlowPlayer {
    lamp: Arc<Lamp>,
    inner: Mutex<Inner>,
    run: AtomicU64, // id of the running loop; 0 = stopped
}

impl FlowPlayer {
    pub fn new(lamp: Arc<Lamp>) -> Arc<Self> {
        Arc::new(FlowPlayer {
            lamp,
            inner: Mutex::new(Inner { colors: vec![], step_ms: 2000.0, phase: 0.0 }),
            run: AtomicU64::new(0),
        })
    }

    pub fn running(&self) -> bool {
        self.run.load(Ordering::Relaxed) != 0
    }

    pub fn start(self: &Arc<Self>, colors: &[u32], speed: u32) {
        if colors.is_empty() {
            return;
        }
        {
            let mut i = self.inner.lock().unwrap();
            i.colors = colors.to_vec();
            i.phase %= colors.len() as f64;
        }
        self.set_speed(speed);
        if self.running() {
            return;
        }
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        self.run.store(id, Ordering::Relaxed);
        let me = self.clone();
        thread::spawn(move || {
            let mut last = Instant::now();
            while me.run.load(Ordering::Relaxed) == id {
                let now = Instant::now();
                me.tick(now.duration_since(last).as_secs_f64() * 1000.0);
                last = now;
                thread::sleep(TICK);
            }
        });
    }

    /// Changing speed keeps the current position, so the colour doesn't jump
    pub fn set_speed(&self, speed: u32) {
        self.inner.lock().unwrap().step_ms = 9000.0 / speed.max(1) as f64;
    }

    pub fn stop(&self) {
        self.run.store(0, Ordering::Relaxed);
    }

    fn tick(&self, elapsed_ms: f64) {
        let rgb = {
            let mut i = self.inner.lock().unwrap();
            let n = i.colors.len() as f64;
            i.phase = (i.phase + elapsed_ms / i.step_ms) % n;
            let k = i.phase.floor() as usize;
            let t = i.phase - k as f64;
            let eased = (1.0 - (std::f64::consts::PI * t).cos()) / 2.0;
            mix(i.colors[k], i.colors[(k + 1) % i.colors.len()], eased)
        };
        // Transition a little longer than the tick so consecutive steps overlap into one glide
        self.lamp.stream(rgb, TICK.as_millis() as u32 * 2);
    }
}
