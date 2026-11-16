//! One connection to the knob over any byte stream (USB serial now, BLE later): HELLO plus a full snapshot on
//! connect, then live messages. The port scanning and reconnect loop live in the app (`knob.rs`).

use crate::protocol::{cover_chunks, encode, Decoder, Msg, FW_CHUNK, FW_DONE, FW_OK, PROTO_VER};
use std::io::{self, ErrorKind, Read, Write};
use std::time::{Duration, Instant};

/// Everything the knob should know right now; sent in full on every (re)connect
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub time: Option<Msg>,
    pub now: Option<Msg>,
    /// (art_id, w, h, RGB565)
    pub cover: Option<(u32, u16, u16, Vec<u8>)>,
    pub lyrics: Msg,
    pub stats: Option<Msg>,
    pub apps: Option<Msg>,
    pub mic: Option<Msg>,
    pub monitor: Option<Msg>,
    pub focus: Option<Msg>,
}

/// "Nothing playing", sent explicitly so the knob clears an old song
pub fn nothing_playing() -> Msg {
    Msg::NowPlaying { playing: false, pos_ms: 0, dur_ms: 0, title: String::new(), artist: String::new() }
}

impl Default for Snapshot {
    fn default() -> Self {
        Snapshot {
            time: None,
            now: Some(nothing_playing()),
            cover: None,
            lyrics: Msg::LyricsNone,
            stats: None,
            apps: None,
            mic: None,
            monitor: None,
            focus: None,
        }
    }
}

pub struct Session<T: Read + Write> {
    io: T,
    dec: Decoder,
    sent_art: Option<u32>,
}

impl<T: Read + Write> Session<T> {
    /// Opens a session: sends HELLO, then the whole snapshot
    pub fn new(io: T, snap: &Snapshot) -> io::Result<Self> {
        let mut s = Session { io, dec: Decoder::new(), sent_art: None };
        s.send(&Msg::Hello { proto: PROTO_VER, version: env!("CARGO_PKG_VERSION").into(), caps: 0 })?;
        for m in snap.time.iter().chain(snap.now.iter()) {
            s.send(m)?;
        }
        if let Some((id, w, h, px)) = &snap.cover {
            s.send_cover(*id, *w, *h, px)?;
        }
        s.send(&snap.lyrics)?;
        for m in [&snap.stats, &snap.apps, &snap.mic, &snap.monitor, &snap.focus].into_iter().flatten() {
            s.send(m)?;
        }
        Ok(s)
    }

    /// Sends one message
    pub fn send(&mut self, m: &Msg) -> io::Result<()> {
        self.io.write_all(&encode(m))?;
        self.io.flush()
    }

    /// Sends a cover unless this session already has this art_id
    pub fn send_cover(&mut self, art_id: u32, w: u16, h: u16, rgb565: &[u8]) -> io::Result<()> {
        if self.sent_art == Some(art_id) {
            return Ok(());
        }
        // Mark it first: if a chunk fails the session is dropped anyway, and a new one starts over
        self.sent_art = Some(art_id);
        let mut buf = Vec::with_capacity(rgb565.len() + rgb565.len() / 8);
        for m in cover_chunks(art_id, w, h, rgb565) {
            buf.extend_from_slice(&encode(&m));
        }
        self.io.write_all(&buf)?;
        self.io.flush()
    }

    /// Reads whatever has arrived. A timeout means nothing arrived; other errors mean the knob is gone.
    pub fn poll(&mut self) -> io::Result<Vec<Msg>> {
        let mut out = Vec::new();
        let mut buf = [0u8; 512];
        loop {
            match self.io.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    out.extend(self.dec.push(&buf[..n]));
                    if n < buf.len() {
                        break;
                    }
                }
                Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock | ErrorKind::Interrupted) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }
}

/// How long the knob may take to answer during an update
#[derive(Clone, Copy, Debug)]
pub struct FwTimeouts {
    /// FW_BEGIN: the knob erases its spare app slot first
    pub begin: Duration,
    pub chunk: Duration,
    /// FW_END: the knob checks the CRC and switches slots
    pub end: Duration,
}

impl Default for FwTimeouts {
    fn default() -> Self {
        FwTimeouts { begin: Duration::from_secs(20), chunk: Duration::from_secs(3), end: Duration::from_secs(20) }
    }
}

impl<T: Read + Write> Session<T> {
    /// Waits for the knob's next FW_STATUS; any other message goes to `other`
    fn fw_status(&mut self, wait: Duration, other: &mut dyn FnMut(Msg)) -> Result<(u8, u32, String), String> {
        let deadline = Instant::now() + wait;
        loop {
            for m in self.poll().map_err(|e| format!("knob disconnected: {e}"))? {
                match m {
                    Msg::FwStatus { state, offset, text } => return Ok((state, offset, text)),
                    m => other(m),
                }
            }
            if Instant::now() >= deadline {
                return Err("the knob stopped answering".into());
            }
        }
    }

    /// Sends a firmware image: FW_BEGIN, then FW_DATA chunks, each acknowledged by FW_STATUS with the next offset
    /// the knob wants (it may ask for a chunk again), then FW_END. Succeeds once the knob reports FW_DONE; it
    /// reboots into the new firmware after that, so the session ends. `progress(sent, total)` follows along.
    pub fn update_firmware(
        &mut self,
        image: &[u8],
        version: &str,
        t: FwTimeouts,
        progress: &mut dyn FnMut(usize, usize),
        other: &mut dyn FnMut(Msg),
    ) -> Result<(), String> {
        let total = image.len();
        let io = |e: io::Error| format!("knob disconnected: {e}");
        self.send(&Msg::FwBegin { size: total as u32, crc32: crate::fw::crc32(image), version: version.into() }).map_err(io)?;
        let mut want = self.expect_ok(t.begin, other)?;
        let mut stuck = 0;
        while (want as usize) < total {
            let at = want as usize;
            let end = (at + FW_CHUNK).min(total);
            self.send(&Msg::FwData { offset: want, data: image[at..end].to_vec() }).map_err(io)?;
            let next = self.expect_ok(t.chunk, other)?;
            if next as usize > total {
                return Err(format!("the knob asked for offset {next}, past the end"));
            }
            // A knob that keeps asking for the same or an earlier spot is not making progress
            stuck = if next <= want { stuck + 1 } else { 0 };
            if stuck >= 3 {
                return Err(format!("the knob keeps asking for offset {next}"));
            }
            want = next;
            progress(want as usize, total);
        }
        self.send(&Msg::FwEnd).map_err(io)?;
        match self.fw_status(t.end, other)? {
            (FW_DONE, _, _) => Ok(()),
            (_, _, text) => Err(if text.is_empty() { "the knob rejected the update".into() } else { format!("the knob says: {text}") }),
        }
    }

    /// The next FW_STATUS must be FW_OK; returns the offset it asks for
    fn expect_ok(&mut self, wait: Duration, other: &mut dyn FnMut(Msg)) -> Result<u32, String> {
        match self.fw_status(wait, other)? {
            (FW_OK, offset, _) => Ok(offset),
            (_, _, text) => Err(if text.is_empty() { "the knob stopped the update".into() } else { format!("the knob says: {text}") }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::CHANGED_BRIGHT;
    use std::collections::VecDeque;

    /// A fake knob: bytes the "knob" sends wait in `inp`; what the app writes lands in `out`
    #[derive(Default)]
    struct Pipe {
        inp: VecDeque<u8>,
        out: Vec<u8>,
        broken: bool,
    }
    impl Read for Pipe {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.broken {
                return Err(io::Error::new(ErrorKind::BrokenPipe, "unplugged"));
            }
            if self.inp.is_empty() {
                return Err(io::Error::new(ErrorKind::TimedOut, "nothing yet"));
            }
            let n = buf.len().min(self.inp.len());
            for b in buf.iter_mut().take(n) {
                *b = self.inp.pop_front().unwrap();
            }
            Ok(n)
        }
    }
    impl Write for Pipe {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.broken {
                return Err(io::Error::new(ErrorKind::BrokenPipe, "unplugged"));
            }
            self.out.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn sent(p: &Pipe) -> Vec<Msg> {
        Decoder::new().push(&p.out)
    }

    fn snap() -> Snapshot {
        Snapshot {
            time: Some(Msg::Time { unix: 1_794_758_659, tz_min: -480 }),
            now: Some(Msg::NowPlaying { playing: true, pos_ms: 1000, dur_ms: 200_000, title: "Glass Garden".into(), artist: "The Placeholders".into() }),
            cover: Some((77, 4, 4, vec![9; 32])),
            lyrics: Msg::Lyrics { line_id: 1, prev: "".into(), cur: "paper moons".into(), next: "over a glass garden".into() },
            stats: Some(Msg::PcStats { cpu_pct: Some(5), cpu_c: None, gpu_pct: Some(1), gpu_c: Some(40), ram_pct: Some(50), host: "DESK".into() }),
            apps: Some(Msg::AudioApps(vec![])),
            mic: Some(Msg::Mic { muted: Some(false) }),
            monitor: Some(Msg::Monitor { bright: Some(60) }),
            focus: Some(Msg::Focus { mode: 0, label: "Chrome".into() }),
        }
    }

    #[test]
    fn session_sends_hello_then_snapshot() {
        let s = Session::new(Pipe::default(), &snap()).unwrap();
        let got = sent(&s.io);
        assert!(matches!(got[0], Msg::Hello { proto: PROTO_VER, .. }));
        assert!(matches!(got[1], Msg::Time { .. }));
        assert!(matches!(got[2], Msg::NowPlaying { .. }));
        assert!(matches!(got[3], Msg::Cover { art_id: 77, chunk: 0, chunks: 1, .. }));
        assert!(matches!(got[4], Msg::Lyrics { line_id: 1, .. }));
        assert!(matches!(got[5], Msg::PcStats { .. }));
        assert!(matches!(got[6], Msg::AudioApps(_)));
        assert!(matches!(got[7], Msg::Mic { .. }));
        assert!(matches!(got[8], Msg::Monitor { .. }));
        assert!(matches!(got[9], Msg::Focus { .. }));
        assert_eq!(got.len(), 10);

        // An empty snapshot still tells the knob nothing is playing and there are no lyrics, so a knob
        // that kept an old song on screen across a reconnect clears it
        let s = Session::new(Pipe::default(), &Snapshot::default()).unwrap();
        let got = sent(&s.io);
        assert_eq!(got.len(), 3);
        assert_eq!(got[1], Msg::NowPlaying { playing: false, pos_ms: 0, dur_ms: 0, title: String::new(), artist: String::new() });
        assert_eq!(got[2], Msg::LyricsNone);
    }

    #[test]
    fn link_resends_snapshot_on_reconnect() {
        let snap = snap();
        let mut first = Session::new(Pipe::default(), &snap).unwrap();
        first.io.broken = true;
        assert!(first.poll().is_err());
        // reconnect: a new session sends everything again, cover included
        let second = Session::new(Pipe::default(), &snap).unwrap();
        assert!(sent(&second.io).iter().any(|m| matches!(m, Msg::Cover { art_id: 77, .. })));
    }

    #[test]
    fn same_art_id_not_resent() {
        let mut s = Session::new(Pipe::default(), &snap()).unwrap();
        s.io.out.clear();
        s.send_cover(77, 4, 4, &[9; 32]).unwrap();
        assert!(s.io.out.is_empty());
        s.send_cover(78, 4, 4, &[1; 32]).unwrap();
        assert!(matches!(sent(&s.io)[0], Msg::Cover { art_id: 78, .. }));
    }

    #[test]
    fn fake_knob_frames_match() {
        // tools/fake_knob.py's LAMP_STATE frame for (changed bright, power 1, bright 50, ct 4000, bg off, rgb 0)
        let st = Msg::LampState { changed: CHANGED_BRIGHT, power: true, bright: 50, ct: 4000, bg_power: false, bg_rgb: 0 };
        assert_eq!(hex(&encode(&st)), "03300a06020132a00f010101010100");
        // and its key commands and firmware answers
        assert_eq!(hex(&encode(&Msg::SetMic { muted: true })), "033301020100");
        assert_eq!(hex(&encode(&Msg::SetMonitor { bright: 70 })), "033401024600");
        assert_eq!(hex(&encode(&Msg::SetAppVolume { id: 0x0A0B_0C0D, vol: 40, muted: false })), "033206060d0c0b0a280100");
        assert_eq!(hex(&encode(&Msg::Turn { steps: -1 })), "03350102ff00");
        assert_eq!(hex(&encode(&Msg::FwStatus { state: FW_OK, offset: 2048, text: String::new() })), "0343060101020801010100");
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn poll_returns_lamp_state() {
        let mut s = Session::new(Pipe::default(), &Snapshot::default()).unwrap();
        assert_eq!(s.poll().unwrap(), vec![]);
        let st = Msg::LampState { changed: CHANGED_BRIGHT, power: true, bright: 40, ct: 3500, bg_power: false, bg_rgb: 0 };
        let f = encode(&st);
        s.io.inp.extend(&f[..3]);
        assert_eq!(s.poll().unwrap(), vec![]);
        s.io.inp.extend(&f[3..]);
        assert_eq!(s.poll().unwrap(), vec![st]);
    }

    /// A knob that runs the firmware-update side of the protocol on what the app writes
    struct FwKnob {
        dec: Decoder,
        inp: VecDeque<u8>,
        image: Vec<u8>,
        size: u32,
        crc: u32,
        /// Answer the chunk at this offset by asking for it again, once
        resend_at: Option<u32>,
        /// Ask for this offset forever (a stuck knob)
        stuck_at: Option<u32>,
        silent: bool,
        lamp_during: bool,
        done: bool,
    }
    impl FwKnob {
        fn new() -> Self {
            FwKnob { dec: Decoder::new(), inp: VecDeque::new(), image: vec![], size: 0, crc: 0, resend_at: None, stuck_at: None, silent: false, lamp_during: false, done: false }
        }
        fn reply(&mut self, m: Msg) {
            self.inp.extend(encode(&m));
        }
        fn ok(&mut self, offset: u32) {
            self.reply(Msg::FwStatus { state: FW_OK, offset, text: String::new() });
        }
    }
    impl Read for FwKnob {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.inp.is_empty() {
                return Err(io::Error::new(ErrorKind::TimedOut, "nothing yet"));
            }
            let n = buf.len().min(self.inp.len());
            for b in buf.iter_mut().take(n) {
                *b = self.inp.pop_front().unwrap();
            }
            Ok(n)
        }
    }
    impl Write for FwKnob {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            for m in self.dec.push(buf) {
                if self.silent {
                    continue;
                }
                match m {
                    Msg::FwBegin { size, crc32, .. } => {
                        self.size = size;
                        self.crc = crc32;
                        self.image.clear();
                        self.ok(0);
                    }
                    Msg::FwData { offset, data } => {
                        if self.lamp_during {
                            self.reply(Msg::Log("still here".into()));
                        }
                        if let Some(s) = self.stuck_at {
                            self.ok(s);
                        } else if self.resend_at == Some(offset) {
                            self.resend_at = None;
                            self.ok(offset);
                        } else {
                            assert_eq!(offset as usize, self.image.len(), "chunks arrive in order");
                            self.image.extend_from_slice(&data);
                            self.ok(self.image.len() as u32);
                        }
                    }
                    Msg::FwEnd => {
                        let good = self.image.len() as u32 == self.size && crate::fw::crc32(&self.image) == self.crc;
                        self.done = good;
                        let state = if good { FW_DONE } else { crate::protocol::FW_ERROR };
                        self.reply(Msg::FwStatus { state, offset: self.image.len() as u32, text: if good { "" } else { "crc mismatch" }.into() });
                    }
                    _ => {}
                }
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn fast() -> FwTimeouts {
        FwTimeouts { begin: Duration::from_millis(50), chunk: Duration::from_millis(50), end: Duration::from_millis(50) }
    }

    fn update(knob: FwKnob, image: &[u8]) -> (Result<(), String>, Session<FwKnob>, Vec<(usize, usize)>, Vec<Msg>) {
        let mut s = Session { io: knob, dec: Decoder::new(), sent_art: None };
        let (mut prog, mut other) = (vec![], vec![]);
        let r = s.update_firmware(image, "0.4.1", fast(), &mut |a, b| prog.push((a, b)), &mut |m| other.push(m));
        (r, s, prog, other)
    }

    #[test]
    fn firmware_update_sends_whole_image() {
        let img = crate::fw::fake_image("0.4.1", "knob", 3 * FW_CHUNK + 100);
        let (r, s, prog, _) = update(FwKnob::new(), &img);
        assert_eq!(r, Ok(()));
        assert!(s.io.done);
        assert_eq!(s.io.image, img);
        assert_eq!(prog.last(), Some(&(img.len(), img.len())));
        assert_eq!(prog.len(), 4);
    }

    #[test]
    fn firmware_update_resends_when_asked_and_passes_other_messages_on() {
        let img = crate::fw::fake_image("0.4.1", "knob", 2 * FW_CHUNK + 1);
        let mut k = FwKnob::new();
        k.resend_at = Some(FW_CHUNK as u32);
        k.lamp_during = true;
        let (r, s, _, other) = update(k, &img);
        assert_eq!(r, Ok(()));
        assert_eq!(s.io.image, img);
        assert!(other.iter().all(|m| matches!(m, Msg::Log(_))) && !other.is_empty());
    }

    #[test]
    fn firmware_update_fails_cleanly() {
        let img = crate::fw::fake_image("0.4.1", "knob", 2 * FW_CHUNK);
        let mut k = FwKnob::new();
        k.silent = true;
        assert_eq!(update(k, &img).0, Err("the knob stopped answering".into()));

        let mut k = FwKnob::new();
        k.stuck_at = Some(0);
        assert!(update(k, &img).0.unwrap_err().contains("keeps asking"));
    }
}
