// Xiaomi miIO protocol client (UDP 54321). Unlike Yeelight's LAN protocol it has no
// per-minute command quota, which is what makes real-time screen sync possible.

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use md5::{Digest, Md5};
use serde_json::{json, Value};
use std::fmt;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

type Enc = cbc::Encryptor<aes::Aes128>;
type Dec = cbc::Decryptor<aes::Aes128>;

const PORT: u16 = 54321;

#[derive(Debug, Clone)]
pub enum Error {
    /// No (matching) reply in time; worth retrying
    Timeout(String),
    /// The lamp answered with an error; retrying won't help
    Lamp(String),
    NotSetUp,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Timeout(m) | Error::Lamp(m) => f.write_str(m),
            Error::NotSetUp => f.write_str("Lamp not set up — add its IP and token in Settings"),
        }
    }
}

fn md5(parts: &[&[u8]]) -> [u8; 16] {
    let mut h = Md5::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

#[derive(Clone, Copy)]
struct Handshake {
    dev_id: [u8; 4],
    stamp: u32,
    at: Instant,
}

pub struct Miio {
    pub ip: String,
    token: [u8; 16],
    key: [u8; 16],
    iv: [u8; 16],
    sock: Mutex<UdpSocket>, // request/response; one call at a time
    // A connected socket, used by send_now() while Windows is ending the
    // session: the packet is on the wire before the call returns.
    fast: Option<UdpSocket>,
    hs: Mutex<Option<Handshake>>,
    next_id: AtomicU32,
}

impl Miio {
    pub fn new(ip: &str, token_hex: &str) -> Option<Miio> {
        let token: [u8; 16] = hex::decode(token_hex).ok()?.try_into().ok()?;
        let key = md5(&[&token]);
        let iv = md5(&[&key, &token]);
        let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
        let fast = UdpSocket::bind("0.0.0.0:0").ok().filter(|s| s.connect((ip, PORT)).is_ok());
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        Some(Miio {
            ip: ip.to_string(),
            token,
            key,
            iv,
            sock: Mutex::new(sock),
            fast,
            hs: Mutex::new(None),
            next_id: AtomicU32::new(1 + seed % 10_000),
        })
    }

    /// Fire-and-forget; uses the last handshake, so it only works if the lamp was
    /// reached recently. Returns false if it couldn't send.
    pub fn send_now(&self, method: &str, params: Value) -> bool {
        let Some(fast) = &self.fast else { return false };
        let Some(hs) = *self.hs.lock().unwrap() else { return false };
        if hs.at.elapsed() > Duration::from_secs(600) {
            return false;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        fast.send(&self.packet(&hs, id, method, &params)).is_ok()
    }

    pub fn call(&self, method: &str, params: Value, timeout: Duration, retries: u32) -> Result<Value, Error> {
        let sock = self.sock.lock().unwrap();
        let mut err = Error::Timeout(self.no_reply());
        for _ in 0..=retries {
            match self.try_call(&sock, method, &params, timeout) {
                Ok(v) => return Ok(v),
                Err(e @ Error::Lamp(_)) => return Err(e),
                Err(e) => err = e,
            }
        }
        Err(err)
    }

    fn no_reply(&self) -> String {
        format!("No reply from lamp at {}", self.ip)
    }

    fn try_call(&self, sock: &UdpSocket, method: &str, params: &Value, timeout: Duration) -> Result<Value, Error> {
        let hs = self.handshake(sock, timeout)?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        sock.send_to(&self.packet(&hs, id, method, params), (self.ip.as_str(), PORT))
            .map_err(|e| Error::Timeout(e.to_string()))?;
        let deadline = Instant::now() + timeout;
        loop {
            let Some(msg) = self.recv(sock, deadline) else {
                *self.hs.lock().unwrap() = None; // force a fresh handshake next time
                return Err(Error::Timeout(self.no_reply()));
            };
            if msg.len() == 32 {
                self.store_handshake(&msg);
                continue;
            }
            let Some(resp) = self.decode(&msg) else { continue };
            if resp["id"].as_u64() != Some(id as u64) {
                continue; // a late answer to an earlier, timed-out request
            }
            if let Some(e) = resp.get("error") {
                let m = e["message"].as_str().map(str::to_string).unwrap_or_else(|| e.to_string());
                return Err(Error::Lamp(format!("Lamp error: {m}")));
            }
            return Ok(resp["result"].clone());
        }
    }

    fn handshake(&self, sock: &UdpSocket, timeout: Duration) -> Result<Handshake, Error> {
        if let Some(hs) = *self.hs.lock().unwrap() {
            if hs.at.elapsed() < Duration::from_secs(60) {
                return Ok(hs);
            }
        }
        let mut hello = [0xffu8; 32];
        hello[..4].copy_from_slice(&[0x21, 0x31, 0x00, 0x20]);
        sock.send_to(&hello, (self.ip.as_str(), PORT)).map_err(|e| Error::Timeout(e.to_string()))?;
        let deadline = Instant::now() + timeout;
        while let Some(msg) = self.recv(sock, deadline) {
            if msg.len() == 32 {
                return Ok(self.store_handshake(&msg));
            }
        }
        Err(Error::Timeout(self.no_reply()))
    }

    fn store_handshake(&self, msg: &[u8]) -> Handshake {
        let hs = Handshake {
            dev_id: msg[8..12].try_into().unwrap(),
            stamp: u32::from_be_bytes(msg[12..16].try_into().unwrap()),
            at: Instant::now(),
        };
        *self.hs.lock().unwrap() = Some(hs);
        hs
    }

    /// Next miIO packet before the deadline, or None
    fn recv(&self, sock: &UdpSocket, deadline: Instant) -> Option<Vec<u8>> {
        let mut buf = [0u8; 4096];
        loop {
            let left = deadline.checked_duration_since(Instant::now())?;
            if left.is_zero() {
                return None;
            }
            sock.set_read_timeout(Some(left)).ok()?;
            match sock.recv_from(&mut buf) {
                Ok((n, _)) if n >= 32 && buf[0] == 0x21 && buf[1] == 0x31 => return Some(buf[..n].to_vec()),
                Ok(_) => continue,
                // WouldBlock/TimedOut when the timeout runs out; on Windows an ICMP
                // "port unreachable" also lands here as ConnectionReset
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
                Err(_) => return None,
            }
        }
    }

    fn packet(&self, hs: &Handshake, id: u32, method: &str, params: &Value) -> Vec<u8> {
        let text = json!({ "id": id, "method": method, "params": params }).to_string();
        let body = Enc::new(&self.key.into(), &self.iv.into()).encrypt_padded_vec_mut::<Pkcs7>(text.as_bytes());
        let mut head = [0u8; 32];
        head[0..2].copy_from_slice(&0x2131u16.to_be_bytes());
        head[2..4].copy_from_slice(&((32 + body.len()) as u16).to_be_bytes());
        head[8..12].copy_from_slice(&hs.dev_id);
        let stamp = hs.stamp.wrapping_add(1 + hs.at.elapsed().as_secs() as u32);
        head[12..16].copy_from_slice(&stamp.to_be_bytes());
        // Checksum = md5(header with the token in the checksum field + body)
        head[16..32].copy_from_slice(&self.token);
        let sum = md5(&[&head, &body]);
        head[16..32].copy_from_slice(&sum);
        [head.as_slice(), &body].concat()
    }

    fn decode(&self, msg: &[u8]) -> Option<Value> {
        let plain = Dec::new(&self.key.into(), &self.iv.into()).decrypt_padded_vec_mut::<Pkcs7>(&msg[32..]).ok()?;
        let text = String::from_utf8_lossy(&plain);
        serde_json::from_str(text.trim_end_matches('\0')).ok()
    }
}
