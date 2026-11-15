//! The knob link, protocol v1. Spec: docs/knob-protocol.md (shared with the knob firmware).
//!
//! A frame is `type u8, len u16 LE, payload`, COBS-encoded and ended by 0x00. The same frames go over
//! USB serial (CDC) and, later, BLE.

pub const VID: u16 = 0x303A;
pub const PID: u16 = 0x8D01;
pub const PROTO_VER: u8 = 1;

/// LAMP_STATE `changed` bits: which values the knob just changed
pub const CHANGED_POWER: u8 = 1;
pub const CHANGED_BRIGHT: u8 = 2;
pub const CHANGED_CT: u8 = 4;
pub const CHANGED_BG_POWER: u8 = 8;
pub const CHANGED_BG_RGB: u8 = 16;

/// Bytes per COVER chunk
pub const COVER_CHUNK: usize = 1024;

// String limits in bytes, matching the knob's buffers (one less than the C array, for the NUL)
pub const MAX_TITLE: usize = 39;
pub const MAX_ARTIST: usize = 31;
pub const MAX_LYRIC: usize = 63;
pub const MAX_LYRIC_CUR: usize = 95;
pub const MAX_SSID: usize = 32;
pub const MAX_PASS: usize = 64;
pub const MAX_IP: usize = 15;
pub const MAX_TOKEN: usize = 32;
pub const MAX_HOST: usize = 23;
pub const MAX_VERSION: usize = 15;
pub const MAX_LOG: usize = 200;

/// Largest payload a decoder accepts (a cover chunk plus its header)
const MAX_PAYLOAD: usize = COVER_CHUNK + 32;

#[derive(Clone, Debug, PartialEq)]
pub enum Msg {
    /// 0x01, both ways
    Hello { proto: u8, version: String, caps: u32 },
    /// 0x10
    NowPlaying { playing: bool, pos_ms: u32, dur_ms: u32, title: String, artist: String },
    /// 0x11, RGB565 little-endian, row-major
    Cover { art_id: u32, w: u16, h: u16, chunk: u16, chunks: u16, data: Vec<u8> },
    /// 0x12
    Lyrics { line_id: u32, prev: String, cur: String, next: String },
    /// 0x13
    LyricsNone,
    /// 0x14, 0xFF on the wire = missing
    PcStats { cpu_pct: Option<u8>, cpu_c: Option<u8>, gpu_pct: Option<u8>, gpu_c: Option<u8>, ram_pct: Option<u8>, host: String },
    /// 0x20
    Provision { ssid: String, pass: String, lamp_ip: String, lamp_token: String },
    /// 0x21
    Time { unix: u32, tz_min: i16 },
    /// 0x30, knob -> app. `changed`: CHANGED_* bits for what the knob just set; the rest is the full state
    LampState { changed: u8, power: bool, bright: u8, ct: u16, bg_power: bool, bg_rgb: u32 },
    /// 0x31, knob -> app
    Log(String),
}

pub fn encode(m: &Msg) -> Vec<u8> {
    let mut w = W(Vec::new());
    let ty = match m {
        Msg::Hello { proto, version, caps } => {
            w.u8(*proto);
            w.u32(*caps);
            w.str(version, MAX_VERSION);
            0x01
        }
        Msg::NowPlaying { playing, pos_ms, dur_ms, title, artist } => {
            w.u8(*playing as u8);
            w.u32(*pos_ms);
            w.u32(*dur_ms);
            w.str(title, MAX_TITLE);
            w.str(artist, MAX_ARTIST);
            0x10
        }
        Msg::Cover { art_id, w: cw, h, chunk, chunks, data } => {
            w.u32(*art_id);
            w.u16(*cw);
            w.u16(*h);
            w.u16(*chunk);
            w.u16(*chunks);
            w.0.extend_from_slice(&data[..data.len().min(COVER_CHUNK)]);
            0x11
        }
        Msg::Lyrics { line_id, prev, cur, next } => {
            w.u32(*line_id);
            w.str(prev, MAX_LYRIC);
            w.str(cur, MAX_LYRIC_CUR);
            w.str(next, MAX_LYRIC);
            0x12
        }
        Msg::LyricsNone => 0x13,
        Msg::PcStats { cpu_pct, cpu_c, gpu_pct, gpu_c, ram_pct, host } => {
            for v in [cpu_pct, cpu_c, gpu_pct, gpu_c, ram_pct] {
                w.u8(v.map(|v| v.min(254)).unwrap_or(0xFF));
            }
            w.str(host, MAX_HOST);
            0x14
        }
        Msg::Provision { ssid, pass, lamp_ip, lamp_token } => {
            w.str(ssid, MAX_SSID);
            w.str(pass, MAX_PASS);
            w.str(lamp_ip, MAX_IP);
            w.str(lamp_token, MAX_TOKEN);
            0x20
        }
        Msg::Time { unix, tz_min } => {
            w.u32(*unix);
            w.u16(*tz_min as u16);
            0x21
        }
        Msg::LampState { changed, power, bright, ct, bg_power, bg_rgb } => {
            w.u8(*changed);
            w.u8(*power as u8);
            w.u8(*bright);
            w.u16(*ct);
            w.u8(*bg_power as u8);
            w.u32(*bg_rgb);
            0x30
        }
        Msg::Log(text) => {
            w.str(text, MAX_LOG);
            0x31
        }
    };
    let len = w.0.len() as u16;
    let mut raw = Vec::with_capacity(w.0.len() + 3);
    raw.push(ty);
    raw.extend_from_slice(&len.to_le_bytes());
    raw.extend_from_slice(&w.0);
    let mut out = cobs_encode(&raw);
    out.push(0);
    out
}

/// Decodes one raw (already COBS-decoded) frame
fn decode(raw: &[u8]) -> Option<Msg> {
    if raw.len() < 3 {
        return None;
    }
    let len = u16::from_le_bytes([raw[1], raw[2]]) as usize;
    if raw.len() != 3 + len {
        return None;
    }
    let mut r = R(&raw[3..]);
    let m = match raw[0] {
        0x01 => Msg::Hello { proto: r.u8()?, caps: r.u32()?, version: r.str()? },
        0x10 => Msg::NowPlaying { playing: r.u8()? != 0, pos_ms: r.u32()?, dur_ms: r.u32()?, title: r.str()?, artist: r.str()? },
        0x11 => Msg::Cover { art_id: r.u32()?, w: r.u16()?, h: r.u16()?, chunk: r.u16()?, chunks: r.u16()?, data: r.rest() },
        0x12 => Msg::Lyrics { line_id: r.u32()?, prev: r.str()?, cur: r.str()?, next: r.str()? },
        0x13 => Msg::LyricsNone,
        0x14 => {
            let mut v = [None; 5];
            for slot in v.iter_mut() {
                let b = r.u8()?;
                *slot = (b != 0xFF).then_some(b);
            }
            Msg::PcStats { cpu_pct: v[0], cpu_c: v[1], gpu_pct: v[2], gpu_c: v[3], ram_pct: v[4], host: r.str()? }
        }
        0x20 => Msg::Provision { ssid: r.str()?, pass: r.str()?, lamp_ip: r.str()?, lamp_token: r.str()? },
        0x21 => Msg::Time { unix: r.u32()?, tz_min: r.u16()? as i16 },
        0x30 => Msg::LampState { changed: r.u8()?, power: r.u8()? != 0, bright: r.u8()?, ct: r.u16()?, bg_power: r.u8()? != 0, bg_rgb: r.u32()? },
        0x31 => Msg::Log(r.str()?),
        _ => return None,
    };
    r.0.is_empty().then_some(m)
}

struct W(Vec<u8>);

impl W {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    /// u8 length + UTF-8, cut to at most `max` bytes on a char boundary
    fn str(&mut self, s: &str, max: usize) {
        let mut end = s.len().min(max).min(255);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        self.0.push(end as u8);
        self.0.extend_from_slice(&s.as_bytes()[..end]);
    }
}

struct R<'a>(&'a [u8]);

impl R<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        let b = self.take(2)?;
        Some(u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Option<u32> {
        let b = self.take(4)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn str(&mut self) -> Option<String> {
        let n = self.u8()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }
    fn rest(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.0).to_vec()
    }
}

fn cobs_encode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 254 + 2);
    let mut code_at = 0;
    out.push(0);
    let mut code = 1u8;
    for &b in data {
        if b == 0 {
            out[code_at] = code;
            code_at = out.len();
            out.push(0);
            code = 1;
        } else {
            out.push(b);
            code += 1;
            if code == 0xFF {
                out[code_at] = code;
                code_at = out.len();
                out.push(0);
                code = 1;
            }
        }
    }
    out[code_at] = code;
    out
}

fn cobs_decode(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        let code = data[i] as usize;
        if code == 0 || i + code > data.len() {
            return None;
        }
        out.extend_from_slice(&data[i + 1..i + code]);
        i += code;
        if code < 0xFF && i < data.len() {
            out.push(0);
        }
    }
    Some(out)
}

pub struct Decoder {
    buf: Vec<u8>,
    /// Lost sync (junk too long to be a frame): drop bytes until the next delimiter
    skipping: bool,
}

impl Decoder {
    pub fn new() -> Self {
        Decoder { buf: Vec::new(), skipping: false }
    }

    /// Feeds received bytes; returns every complete, valid message in them. Invalid frames are dropped.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Msg> {
        let limit = MAX_PAYLOAD + 3 + (MAX_PAYLOAD + 3) / 254 + 2;
        let mut out = Vec::new();
        for &b in bytes {
            if b == 0 {
                if !self.skipping && !self.buf.is_empty() {
                    if let Some(m) = cobs_decode(&self.buf).and_then(|raw| decode(&raw)) {
                        out.push(m);
                    }
                }
                self.buf.clear();
                self.skipping = false;
            } else if !self.skipping {
                if self.buf.len() >= limit {
                    self.buf.clear();
                    self.skipping = true;
                } else {
                    self.buf.push(b);
                }
            }
        }
        out
    }
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Splits an RGB565 cover into COVER messages of COVER_CHUNK bytes
pub fn cover_chunks(art_id: u32, w: u16, h: u16, rgb565: &[u8]) -> Vec<Msg> {
    let n = rgb565.chunks(COVER_CHUNK).count() as u16;
    rgb565
        .chunks(COVER_CHUNK)
        .enumerate()
        .map(|(i, c)| Msg::Cover { art_id, w, h, chunk: i as u16, chunks: n, data: c.to_vec() })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Vec<Msg> {
        vec![
            Msg::Hello { proto: 1, version: "3.0.0".into(), caps: 0b101 },
            Msg::NowPlaying { playing: true, pos_ms: 102_000, dur_ms: 238_000, title: "Night Ferry".into(), artist: "Mono Lake".into() },
            Msg::Cover { art_id: 0xDEADBEEF, w: 180, h: 180, chunk: 3, chunks: 64, data: vec![0, 1, 2, 0, 255, 0] },
            Msg::Lyrics { line_id: 7, prev: "the harbour lights".into(), cur: "fold into the water".into(), next: "".into() },
            Msg::LyricsNone,
            Msg::PcStats { cpu_pct: Some(12), cpu_c: None, gpu_pct: Some(3), gpu_c: Some(61), ram_pct: Some(48), host: "DESK".into() },
            Msg::Provision { ssid: "home".into(), pass: "pa ss".into(), lamp_ip: "10.0.0.16".into(), lamp_token: "00112233445566778899aabbccddeeff".into() },
            Msg::Time { unix: 1_794_758_659, tz_min: -480 },
            Msg::LampState { changed: CHANGED_BRIGHT | CHANGED_CT, power: true, bright: 70, ct: 4000, bg_power: false, bg_rgb: 0xFFB340 },
            Msg::Log("hello from the knob".into()),
        ]
    }

    #[test]
    fn roundtrip_every_msg() {
        let mut d = Decoder::new();
        for m in all() {
            let f = encode(&m);
            assert_eq!(*f.last().unwrap(), 0, "frame ends with 0x00");
            assert_eq!(f.iter().filter(|b| **b == 0).count(), 1, "only the delimiter is zero");
            assert_eq!(d.push(&f), vec![m]);
        }
    }

    #[test]
    fn decoder_handles_split_and_joined_frames() {
        let msgs = all();
        let mut d = Decoder::new();
        let mut got = vec![];
        for b in encode(&msgs[1]).iter().chain(encode(&msgs[3]).iter()) {
            got.extend(d.push(&[*b]));
        }
        assert_eq!(got, vec![msgs[1].clone(), msgs[3].clone()]);

        let joined: Vec<u8> = [&msgs[0], &msgs[4], &msgs[8]].iter().flat_map(|m| encode(m)).collect();
        assert_eq!(d.push(&joined), vec![msgs[0].clone(), msgs[4].clone(), msgs[8].clone()]);
    }

    #[test]
    fn decoder_skips_garbage() {
        let mut d = Decoder::new();
        let mut bytes = vec![0x55, 0x13, 0x99, 0x00, 0x07, 0x01, 0x00];
        bytes.extend(encode(&Msg::LyricsNone));
        assert_eq!(d.push(&bytes), vec![Msg::LyricsNone]);

        // A frame whose length field disagrees with its payload is dropped
        let mut bad = encode(&Msg::Time { unix: 5, tz_min: 0 });
        bad.pop();
        let mut raw = cobs_decode(&bad).unwrap();
        raw[1] = 200;
        let mut frame = cobs_encode(&raw);
        frame.push(0);
        assert!(d.push(&frame).is_empty());
        // and the decoder keeps working after it
        assert_eq!(d.push(&encode(&Msg::LyricsNone)), vec![Msg::LyricsNone]);

        // Endless junk without a delimiter doesn't grow the buffer forever
        d.push(&vec![0x42; 100_000]);
        assert!(d.buf.len() <= 2 * (MAX_PAYLOAD + 8));
        assert_eq!(d.push(&[0]), vec![]);
        assert_eq!(d.push(&encode(&Msg::LyricsNone)), vec![Msg::LyricsNone]);
    }

    #[test]
    fn strings_truncate_on_char_boundary() {
        let title: String = "夜".repeat(50);
        let f = encode(&Msg::NowPlaying { playing: false, pos_ms: 0, dur_ms: 0, title, artist: "a".into() });
        let got = Decoder::new().push(&f);
        let Msg::NowPlaying { title, .. } = &got[0] else { panic!() };
        assert!(title.len() <= MAX_TITLE);
        assert_eq!(title.len(), 39); // 13 three-byte chars
        assert!(title.chars().all(|c| c == '夜'));
    }

    #[test]
    fn cover_chunks_reassemble() {
        let px: Vec<u8> = (0..180 * 180 * 2).map(|i| (i * 7 % 251) as u8).collect();
        let chunks = cover_chunks(42, 180, 180, &px);
        assert_eq!(chunks.len(), 64);
        let mut d = Decoder::new();
        let mut joined = vec![];
        for (i, c) in chunks.iter().enumerate() {
            let back = d.push(&encode(c));
            let Msg::Cover { art_id, w, h, chunk, chunks: n, data } = &back[0] else { panic!() };
            assert_eq!((*art_id, *w, *h, *chunk as usize, *n), (42, 180, 180, i, 64));
            joined.extend_from_slice(data);
        }
        assert_eq!(joined, px);
    }
}
