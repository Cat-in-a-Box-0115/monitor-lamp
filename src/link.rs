//! One connection to the knob over any byte stream (USB serial now, BLE later): HELLO plus a full snapshot on
//! connect, then live messages. The port scanning and reconnect loop live in the app (`knob.rs`).

use crate::protocol::{cover_chunks, encode, Decoder, Msg, PROTO_VER};
use std::io::{self, ErrorKind, Read, Write};

/// Everything the knob should know right now; sent in full on every (re)connect
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub time: Option<Msg>,
    pub now: Option<Msg>,
    /// (art_id, w, h, RGB565)
    pub cover: Option<(u32, u16, u16, Vec<u8>)>,
    pub lyrics: Msg,
    pub stats: Option<Msg>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Snapshot { time: None, now: None, cover: None, lyrics: Msg::LyricsNone, stats: None }
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
        if let Some(m) = &snap.stats {
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

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(got.len(), 6);

        // An empty snapshot still tells the knob there are no lyrics
        let s = Session::new(Pipe::default(), &Snapshot::default()).unwrap();
        let got = sent(&s.io);
        assert_eq!(got.len(), 2);
        assert_eq!(got[1], Msg::LyricsNone);
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
        // tools/fake_knob.py's LAMP_STATE frame for (power 1, bright 50, ct 4000, bg off, rgb 0)
        let st = Msg::LampState { power: true, bright: 50, ct: 4000, bg_power: false, bg_rgb: 0 };
        assert_eq!(hex(&encode(&st)), "033009050132a00f010101010100");
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn poll_returns_lamp_state() {
        let mut s = Session::new(Pipe::default(), &Snapshot::default()).unwrap();
        assert_eq!(s.poll().unwrap(), vec![]);
        let st = Msg::LampState { power: true, bright: 40, ct: 3500, bg_power: false, bg_rgb: 0 };
        let f = encode(&st);
        s.io.inp.extend(&f[..3]);
        assert_eq!(s.poll().unwrap(), vec![]);
        s.io.inp.extend(&f[3..]);
        assert_eq!(s.poll().unwrap(), vec![st]);
    }
}
