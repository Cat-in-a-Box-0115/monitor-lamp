// High-level wrapper around the miIO client for the Yeelight Screen Light Bar (lamp15).

use crate::miio::{Error, Miio};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

// 'power' is on while any part of the lamp is lit; 'main_power' is the main light alone.
const PROPS: [&str; 8] = ["power", "bright", "ct", "bg_power", "bg_bright", "bg_rgb", "bg_flowing", "main_power"];

#[derive(Clone, Copy, Debug, Default)]
pub struct State {
    pub power: bool,
    pub bright: u32,
    pub ct: u32,
    pub bg_power: bool,
    pub bg_bright: u32,
    pub bg_rgb: u32,
    pub bg_flowing: bool,
}

type StatusFn = Box<dyn Fn(Option<bool>, Option<String>) + Send + Sync>;
type LogFn = Box<dyn Fn(String) + Send + Sync>;

pub struct Lamp {
    miio: RwLock<Option<Arc<Miio>>>,
    creds: Mutex<(String, String)>,
    online: Mutex<Option<bool>>,
    failures: AtomicU32,
    on_status: StatusFn,
    log: LogFn,
    stream: Arc<Latest<(u32, u32)>>,
}

impl Lamp {
    pub fn new(on_status: StatusFn, log: LogFn) -> Arc<Lamp> {
        Arc::new_cyclic(|weak: &std::sync::Weak<Lamp>| {
            let w = weak.clone();
            // Real-time colour stream (screen sync, flows): one command in flight, newest colour wins.
            let stream = Latest::new(move |(rgb, ms): (u32, u32)| {
                let Some(lamp) = w.upgrade() else { return };
                let params = if ms >= 30 { json!([rgb, "smooth", ms]) } else { json!([rgb, "sudden", 0]) };
                if lamp.call_with("bg_set_rgb", params, 400, 0).is_err() {
                    thread::sleep(Duration::from_millis(200));
                }
            });
            Lamp {
                miio: RwLock::new(None),
                creds: Mutex::new(Default::default()),
                online: Mutex::new(None),
                failures: AtomicU32::new(0),
                on_status,
                log,
                stream,
            }
        })
    }

    pub fn configure(&self, ip: &str, token: &str) {
        *self.creds.lock().unwrap() = (ip.to_string(), token.to_string());
        let valid = ip.parse::<std::net::Ipv4Addr>().is_ok();
        *self.miio.write().unwrap() = if valid { Miio::new(ip, token).map(Arc::new) } else { None };
        *self.online.lock().unwrap() = None;
        (self.on_status)(None, None);
    }

    pub fn configured(&self) -> bool {
        self.miio.read().unwrap().is_some()
    }

    pub fn online(&self) -> Option<bool> {
        *self.online.lock().unwrap()
    }

    pub fn streaming(&self) -> bool {
        self.stream.busy()
    }

    fn set_online(&self, online: bool, error: Option<String>) {
        let mut cur = self.online.lock().unwrap();
        if *cur == Some(online) && error.is_none() {
            return;
        }
        // Only report repeated errors once
        if *cur == Some(false) && !online {
            return;
        }
        *cur = Some(online);
        drop(cur);
        (self.on_status)(Some(online), error);
    }

    pub fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        self.call_with(method, params, 800, 2)
    }

    pub fn call_with(&self, method: &str, params: Value, timeout_ms: u64, retries: u32) -> Result<Value, Error> {
        let miio = self.miio.read().unwrap().clone().ok_or(Error::NotSetUp)?;
        match miio.call(method, params, Duration::from_millis(timeout_ms), retries) {
            Ok(v) => {
                self.failures.store(0, Ordering::Relaxed);
                self.set_online(true, None);
                Ok(v)
            }
            Err(e @ Error::Lamp(_)) => Err(e),
            Err(e) => {
                self.set_online(false, Some(e.to_string()));
                // A socket created before the network came up (e.g. at sign-in) can stay
                // broken; start over with a fresh one after a few misses.
                let n = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
                if n % 3 == 0 {
                    (self.log)(format!("{n} failed attempts, recreating socket"));
                    let (ip, token) = self.creds.lock().unwrap().clone();
                    *self.miio.write().unwrap() = Miio::new(&ip, &token).map(Arc::new);
                }
                Err(e)
            }
        }
    }

    pub fn state(&self) -> Result<State, Error> {
        let v = self.call("get_prop", json!(PROPS))?;
        let s = |i: usize| match &v[i] {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let n = |i: usize| s(i).parse::<u32>().unwrap_or(0);
        let main = s(7);
        Ok(State {
            power: if main.is_empty() { s(0) == "on" } else { main == "on" },
            bright: n(1),
            ct: n(2),
            bg_power: s(3) == "on",
            bg_bright: n(4),
            bg_rgb: n(5),
            bg_flowing: s(6) == "1",
        })
    }

    /// Keeps trying for a while — used right after sign-in/wake when Wi-Fi may still be connecting.
    pub fn persist(&self, method: &str, params: Value, max: Duration) -> Result<Value, Error> {
        let until = Instant::now() + max;
        loop {
            match self.call_with(method, params.clone(), 1000, 0) {
                Ok(v) => return Ok(v),
                Err(e @ (Error::Lamp(_) | Error::NotSetUp)) => return Err(e),
                Err(e) if Instant::now() > until => return Err(e),
                Err(_) => thread::sleep(Duration::from_secs(2)),
            }
        }
    }

    /// Synchronous "off" for when Windows is ending the session: the packets are
    /// sent before this returns, and the lamp then fades out on its own.
    /// Sent twice since there's no time to wait for replies.
    pub fn off_now(&self, include_backlight: bool, fade_ms: u32) -> bool {
        let Some(miio) = self.miio.read().unwrap().clone() else { return false };
        let mode = |cmd: &str| if fade_ms >= 30 { json!([cmd, "smooth", fade_ms]) } else { json!([cmd, "sudden", 0]) };
        let mut sent = false;
        for _ in 0..2 {
            sent |= miio.send_now("set_power", mode("off"));
            if include_backlight {
                miio.send_now("bg_set_power", mode("off"));
            }
        }
        sent
    }

    pub fn stream(&self, rgb: u32, transition_ms: u32) {
        self.stream.push((rgb, transition_ms));
    }
}

/// Runs `f` on a worker thread with one call in flight; values pushed meanwhile
/// replace each other, so only the newest one is sent next.
pub struct Latest<T> {
    slot: Mutex<Option<T>>,
    busy: AtomicBool,
    f: Box<dyn Fn(T) + Send + Sync>,
}

impl<T: Send + 'static> Latest<T> {
    pub fn new(f: impl Fn(T) + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Latest { slot: Mutex::new(None), busy: AtomicBool::new(false), f: Box::new(f) })
    }

    pub fn busy(&self) -> bool {
        self.busy.load(Ordering::Relaxed)
    }

    pub fn push(self: &Arc<Self>, v: T) {
        *self.slot.lock().unwrap() = Some(v);
        if self.busy.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = self.clone();
        thread::spawn(move || loop {
            let next = me.slot.lock().unwrap().take();
            match next {
                Some(v) => (me.f)(v),
                None => {
                    me.busy.store(false, Ordering::Release);
                    // Something may have arrived between take() and the store above
                    if me.slot.lock().unwrap().is_some() && !me.busy.swap(true, Ordering::AcqRel) {
                        continue;
                    }
                    break;
                }
            }
        });
    }
}
