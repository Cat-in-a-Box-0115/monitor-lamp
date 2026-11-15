# Knob link protocol, v1

This is the shared reference for Detent (this repo, `src/protocol.rs`) and the knob firmware
(`Cat-in-a-Box-0115/motorized-knob`). If either side changes the protocol, change this file and bump `PROTO_VER`.

## Transport

- **USB:** the knob is a composite device with **VID `0x303A`** (Espressif) and **PID `0x8D01`** (this project).
  - HID consumer control: volume up/down, mute, play/pause, next, previous. Windows handles these without the app.
  - CDC serial: the data channel below. Detent finds the port by VID/PID and ignores the baud rate.
- **BLE (wireless builds, later):** HID-over-GATT carries the same keys, and a custom GATT service carries the same
  frames, split across writes and notifications.

## Framing

Each message is `type u8 · len u16 LE · payload[len]`, COBS-encoded and followed by a single `0x00`.
- A receiver drops any frame whose length doesn't match, whose type is unknown, or that has leftover bytes.
- After about 1.1 KB without a `0x00`, the receiver drops everything up to the next `0x00`.

**Field types:**
- Integers are little-endian.
- `str` is a `u8` length followed by that many bytes of UTF-8. The sender cuts it on a character boundary to the
  limit in brackets.
- `opt8` is a `u8` where `0xFF` means missing.

## Messages

| Type | Name | Direction | Payload |
|---|---|---|---|
| 0x01 | HELLO | both | `proto u8, caps u32, version str[15]` |
| 0x10 | NOW_PLAYING | app → knob | `playing u8, pos_ms u32, dur_ms u32, title str[39], artist str[31]` |
| 0x11 | COVER | app → knob | `art_id u32, w u16, h u16, chunk u16, chunks u16, data[≤1024]`: RGB565 LE, row-major |
| 0x12 | LYRICS | app → knob | `line_id u32, prev str[63], cur str[95], next str[63]` |
| 0x13 | LYRICS_NONE | app → knob | (empty) |
| 0x14 | PC_STATS | app → knob | `cpu_pct opt8, cpu_c opt8, gpu_pct opt8, gpu_c opt8, ram_pct opt8, host str[23]` |
| 0x20 | PROVISION | app → knob | `ssid str[32], pass str[64], lamp_ip str[15], lamp_token str[32]` (token as 32 hex chars) |
| 0x21 | TIME | app → knob | `unix u32, tz_min i16` (minutes east of UTC) |
| 0x30 | LAMP_STATE | knob → app | `changed u8, power u8, bright u8 (1–100), ct u16 (K), bg_power u8, bg_rgb u32 (0xRRGGBB)`. `changed` bits: 1 power, 2 brightness, 4 warmth, 8 backlight power, 16 backlight colour |
| 0x31 | LOG | knob → app | `text str[200]` |

## Rules

- **On connect:**
  - Both sides send HELLO.
  - The app then sends a full snapshot: TIME, NOW_PLAYING, the COVER (if there is one), LYRICS or LYRICS_NONE, and
    PC_STATS.
  - After a reconnect, the app sends everything again. When nothing is playing, the snapshot still includes an empty,
    stopped NOW_PLAYING so the knob clears an old song.
- **COVER:**
  - Sent only when `art_id` changes (and in the snapshot).
  - The knob shows a cover only once all `chunks` chunks for one `art_id` have arrived. A new `art_id` throws away
    a half-received cover.
- **NOW_PLAYING:**
  - Sent on every change, and every 5 s while playing, so the knob's position doesn't drift.
  - The knob extrapolates the position between messages while `playing`.
- **LYRICS:**
  - Sent when the line changes.
  - `line_id` 0 means "before the first line". Any other change of `line_id` means a new line, even if the text is
    the same.
- **PC_STATS:** sent every 2 s.
- **LAMP_STATE:**
  - The knob sends it after each command it sends to the lamp (over Wi-Fi, miIO), with `changed` naming what that
    command set and the other fields holding the full state.
  - Detent treats it as a manual change: a brightness or warmth change pauses time of day, and a backlight change
    stops screen sync and colour flows.
- **PROVISION:**
  - Sent only when the user presses "Set up knob" in Detent.
  - The knob stores it in NVS and connects to Wi-Fi.
  - Detent never saves the Wi-Fi password.
