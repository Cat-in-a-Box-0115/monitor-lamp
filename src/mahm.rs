//! MSI Afterburner's hardware-monitoring shared memory ("MAHMSharedMemory"), parsed from a byte copy.
//!
//! Header (u32 each): signature 'MAHM' (0x4D41484D), version, header_size, num_entries, entry_size, ...
//! Entry: src_name[260], src_units[260], localized_name[260], localized_units[260], format[260],
//! data f32 @1300, min f32, max f32, flags u32, gpu u32 @1320, src_id u32.

pub const SIGNATURE: u32 = 0x4D41_484D;
const NAME_LEN: usize = 260;
const DATA_AT: usize = 1300;
const GPU_AT: usize = 1320;
const MIN_ENTRY: usize = GPU_AT + 4;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Mahm {
    pub gpu_c: Option<f32>,
    pub gpu_pct: Option<f32>,
    pub cpu_c: Option<f32>,
}

/// None if the signature is wrong (Afterburner not running, or shutting down) or the sizes don't fit the buffer
pub fn parse(buf: &[u8]) -> Option<Mahm> {
    let u32_at = |at: usize| buf.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    if u32_at(0)? != SIGNATURE {
        return None;
    }
    let header_size = u32_at(8)? as usize;
    let count = u32_at(12)? as usize;
    let entry_size = u32_at(16)? as usize;
    if entry_size < MIN_ENTRY || header_size < 20 {
        return None;
    }
    let end = count.checked_mul(entry_size)?.checked_add(header_size)?;
    if end > buf.len() {
        return None;
    }

    let mut m = Mahm::default();
    let mut core_max: Option<f32> = None;
    for i in 0..count {
        let e = &buf[header_size + i * entry_size..][..entry_size];
        let name_bytes = &e[..NAME_LEN];
        let name = std::str::from_utf8(&name_bytes[..name_bytes.iter().position(|b| *b == 0).unwrap_or(NAME_LEN)]).unwrap_or("");
        let v = f32::from_le_bytes([e[DATA_AT], e[DATA_AT + 1], e[DATA_AT + 2], e[DATA_AT + 3]]);
        // Afterburner reports a sensor it can't read as FLT_MAX
        let v = (v.is_finite() && v != f32::MAX).then_some(v);
        match name {
            "GPU temperature" => m.gpu_c = m.gpu_c.or(v),
            "GPU usage" => m.gpu_pct = m.gpu_pct.or(v),
            "CPU temperature" => m.cpu_c = m.cpu_c.or(v),
            _ => {
                let core = name.strip_prefix("CPU").and_then(|r| r.strip_suffix(" temperature"));
                if core.is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())) {
                    if let Some(v) = v {
                        core_max = Some(core_max.map_or(v, |c| c.max(v)));
                    }
                }
            }
        }
    }
    m.cpu_c = m.cpu_c.or(core_max);
    Some(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, data: f32, gpu: u32) -> Vec<u8> {
        let mut e = vec![0u8; 1328];
        e[..name.len()].copy_from_slice(name.as_bytes());
        e[DATA_AT..DATA_AT + 4].copy_from_slice(&data.to_le_bytes());
        e[GPU_AT..GPU_AT + 4].copy_from_slice(&gpu.to_le_bytes());
        e
    }

    fn buffer(entries: &[Vec<u8>]) -> Vec<u8> {
        let header_size = 32u32;
        let mut b = Vec::new();
        for v in [SIGNATURE, 0x0002_0000, header_size, entries.len() as u32, 1328, 0, 0, 0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        for e in entries {
            b.extend_from_slice(e);
        }
        b
    }

    #[test]
    fn mahm_parses_synthetic_buffer() {
        let b = buffer(&[
            entry("GPU temperature", 61.0, 0),
            entry("GPU usage", 3.0, 0),
            entry("GPU temperature", 80.0, 1),
            entry("CPU temperature", 52.5, 0),
            entry("Framerate", 144.0, 0),
        ]);
        assert_eq!(parse(&b), Some(Mahm { gpu_c: Some(61.0), gpu_pct: Some(3.0), cpu_c: Some(52.5) }));
    }

    #[test]
    fn mahm_missing_entries() {
        let b = buffer(&[entry("GPU temperature", 61.0, 0), entry("GPU usage", f32::MAX, 0)]);
        assert_eq!(parse(&b), Some(Mahm { gpu_c: Some(61.0), gpu_pct: None, cpu_c: None }));
        assert_eq!(parse(&buffer(&[])), Some(Mahm::default()));
    }

    #[test]
    fn mahm_cpu_core_fallback() {
        let b = buffer(&[entry("CPU1 temperature", 48.0, 0), entry("CPU2 temperature", 55.0, 0), entry("CPU3 temperature", f32::NAN, 0)]);
        assert_eq!(parse(&b).unwrap().cpu_c, Some(55.0));
        // the package value wins over cores
        let b = buffer(&[entry("CPU1 temperature", 70.0, 0), entry("CPU temperature", 50.0, 0)]);
        assert_eq!(parse(&b).unwrap().cpu_c, Some(50.0));
    }

    #[test]
    fn mahm_rejects_bad_signature_and_truncation() {
        let mut b = buffer(&[entry("GPU temperature", 61.0, 0)]);
        assert!(parse(&b[..b.len() - 1]).is_none());
        assert!(parse(&b[..10]).is_none());
        b[0] = 0xAD;
        assert!(parse(&b).is_none());
        // entry size smaller than the fields we read
        let mut b = buffer(&[entry("GPU temperature", 61.0, 0)]);
        b[16..20].copy_from_slice(&100u32.to_le_bytes());
        assert!(parse(&b).is_none());
        // absurd entry count
        let mut b = buffer(&[]);
        b[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(parse(&b).is_none());
    }
}
