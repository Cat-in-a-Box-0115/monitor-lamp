//! Synced lyrics in LRC format: `[mm:ss.xx]text`, several stamps per line allowed, `[offset:±ms]` honoured.

#[derive(Clone, Debug, PartialEq)]
pub struct Lrc {
    /// (time in ms, text), sorted by time; blank text = an instrumental gap
    pub lines: Vec<(u32, String)>,
}

/// None if the text has no timed lines (plain lyrics or nothing)
pub fn parse(text: &str) -> Option<Lrc> {
    let mut offset: i64 = 0;
    let mut lines: Vec<(u32, String)> = Vec::new();
    for line in text.lines() {
        let mut rest = line.trim();
        let mut stamps = Vec::new();
        while let Some(body) = rest.strip_prefix('[') {
            let Some(close) = body.find(']') else { break };
            let tag = &body[..close];
            if let Some(v) = tag.strip_prefix("offset:") {
                offset = v.trim().parse().unwrap_or(0);
            } else if let Some(ms) = stamp(tag) {
                stamps.push(ms);
            }
            rest = &body[close + 1..];
        }
        for ms in stamps {
            lines.push((ms, rest.trim().to_string()));
        }
    }
    if lines.is_empty() {
        return None;
    }
    // [offset:+n] means the lyrics are shown n ms sooner
    for l in &mut lines {
        l.0 = (l.0 as i64 - offset).clamp(0, u32::MAX as i64) as u32;
    }
    lines.sort_by_key(|l| l.0);
    Some(Lrc { lines })
}

/// "mm:ss", "mm:ss.x", "mm:ss.xx" or "mm:ss.xxx" in ms
fn stamp(tag: &str) -> Option<u32> {
    let (m, s) = tag.split_once(':')?;
    let m: u32 = m.trim().parse().ok()?;
    let (sec, frac) = s.split_once('.').unwrap_or((s, ""));
    let sec: u32 = sec.trim().parse().ok()?;
    if sec >= 60 || frac.len() > 3 || !frac.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let frac_ms = if frac.is_empty() { 0 } else { frac.parse::<u32>().ok()? * 10u32.pow(3 - frac.len() as u32) };
    Some(m * 60_000 + sec * 1000 + frac_ms)
}

impl Lrc {
    /// The line playing at `pos_ms`: (line_id, prev, cur, next). line_id is index + 1; 0 = before the first line.
    pub fn at(&self, pos_ms: u32) -> Option<(u32, &str, &str, &str)> {
        let first = self.lines.first()?;
        // index of the last line that has started
        let n = self.lines.partition_point(|l| l.0 <= pos_ms);
        if n == 0 {
            return Some((0, "", "", &first.1));
        }
        let i = n - 1;
        let text = |j: usize| self.lines.get(j).map(|l| l.1.as_str()).unwrap_or("");
        let prev = if i == 0 { "" } else { text(i - 1) };
        Some((i as u32 + 1, prev, text(i), text(i + 1)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Invented lyrics (the knob simulator's "Glass Garden")
    const SONG: &str = "[ar:The Placeholders]\n[ti:Glass Garden]\n[00:01.00]paper moons\n[00:04.50]over a glass garden\n[00:08.00]\n[00:09.20][00:20.00]we water the light\n[00:12.75]and it grows\n";

    #[test]
    fn parse_basic_and_multi_stamp() {
        let l = parse(SONG).unwrap();
        let times: Vec<u32> = l.lines.iter().map(|x| x.0).collect();
        assert_eq!(times, vec![1000, 4500, 8000, 9200, 12750, 20000]);
        assert_eq!(l.lines[2].1, "");
        assert_eq!(l.lines[3].1, "we water the light");
        assert_eq!(l.lines[5].1, "we water the light");
    }

    #[test]
    fn parse_offset() {
        // A positive offset makes lyrics appear sooner
        let l = parse("[offset:+500]\n[00:02.00]a\n[00:03.00]b\n").unwrap();
        assert_eq!(l.lines[0].0, 1500);
        let l = parse("[offset:-250]\n[00:02.00]a\n").unwrap();
        assert_eq!(l.lines[0].0, 2250);
        let l = parse("[offset:+5000]\n[00:02.00]a\n").unwrap();
        assert_eq!(l.lines[0].0, 0);
    }

    #[test]
    fn lyrics_none_when_no_synced() {
        assert_eq!(parse("plain text\nwith no stamps"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("[ar:Someone]\n[ti:Something]"), None);
    }

    #[test]
    fn at_before_first_line() {
        let l = parse(SONG).unwrap();
        assert_eq!(l.at(0), Some((0, "", "", "paper moons")));
        assert_eq!(l.at(999), Some((0, "", "", "paper moons")));
    }

    #[test]
    fn lrc_seek_back_and_repeat() {
        let l = parse(SONG).unwrap();
        assert_eq!(l.at(1000), Some((1, "", "paper moons", "over a glass garden")));
        assert_eq!(l.at(10_000), Some((4, "", "we water the light", "and it grows")));
        assert_eq!(l.at(25_000), Some((6, "and it grows", "we water the light", "")));
        // seek back
        assert_eq!(l.at(5000).unwrap().0, 2);
        // the repeated line has a different id the second time
        assert_ne!(l.at(9300).unwrap().0, l.at(20_100).unwrap().0);
    }

    #[test]
    fn stamp_formats() {
        let l = parse("[1:02]a\n[01:03.5]b\n[01:04.123]c\n[xx:yy]d\n").unwrap();
        let times: Vec<u32> = l.lines.iter().map(|x| x.0).collect();
        assert_eq!(times, vec![62_000, 63_500, 64_123]);
    }
}
