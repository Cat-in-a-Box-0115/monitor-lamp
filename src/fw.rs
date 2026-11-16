//! Knob firmware images: the checks Detent runs before sending one over USB (FW_BEGIN / FW_DATA / FW_END).

/// CRC-32 (IEEE, the zlib / esp_rom_crc32_le one)
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// What an ESP-IDF app image says about itself
#[derive(Debug, PartialEq)]
pub struct AppInfo {
    pub version: String,
    pub project: String,
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

/// Checks that `image` is an ESP-IDF app image (not a merged or bootloader binary) and reads its version.
/// Layout: 24-byte image header (magic 0xE9), 8-byte segment header, then esp_app_desc_t
/// (magic 0xABCD5432, version at +16, project name at +48).
pub fn app_info(image: &[u8]) -> Result<AppInfo, String> {
    if image.len() < 112 || image[0] != 0xE9 {
        return Err("not a knob firmware file (pick the app .bin from the build folder)".into());
    }
    let magic = u32::from_le_bytes([image[32], image[33], image[34], image[35]]);
    if magic != 0xABCD_5432 {
        return Err("this .bin has no app description: pick the app .bin, not the merged or bootloader one".into());
    }
    Ok(AppInfo { version: cstr(&image[48..80]), project: cstr(&image[80..112]) })
}

#[cfg(test)]
pub(crate) fn fake_image(version: &str, project: &str, len: usize) -> Vec<u8> {
    let mut img = vec![0u8; len.max(112)];
    img[0] = 0xE9;
    img[32..36].copy_from_slice(&0xABCD_5432u32.to_le_bytes());
    img[48..48 + version.len()].copy_from_slice(version.as_bytes());
    img[80..80 + project.len()].copy_from_slice(project.as_bytes());
    for (i, b) in img.iter_mut().enumerate().skip(112) {
        *b = (i * 31 % 251) as u8;
    }
    img
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn app_info_reads_version_and_rejects_other_files() {
        let img = fake_image("0.4.1", "knob", 4000);
        assert_eq!(app_info(&img).unwrap(), AppInfo { version: "0.4.1".into(), project: "knob".into() });
        assert!(app_info(b"PK\x03\x04 a zip").is_err());
        let mut no_desc = img.clone();
        no_desc[32] = 0;
        assert!(app_info(&no_desc).unwrap_err().contains("merged"));
    }
}
