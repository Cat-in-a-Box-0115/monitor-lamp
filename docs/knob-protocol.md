# Knob link protocol, v2

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
| 0x15 | AUDIO_APPS | app → knob | `n u8 (≤ 8)`, then n × `id u32, vol u8 (0–100), muted u8, name str[23]` |
| 0x16 | MIC | app → knob | `state u8`: 0 on, 1 muted, 0xFF no microphone |
| 0x17 | MONITOR | app → knob | `bright opt8` (0–100); missing = no monitor takes brightness over DDC/CI |
| 0x18 | FOCUS | app → knob | `mode u8, label str[23]`: the program in front. Modes: 0 volume, 1 scroll, 2 zoom, 3 sideways |
| 0x20 | PROVISION | app → knob | `ssid str[32], pass str[64], lamp_ip str[15], lamp_token str[32]` (token as 32 hex chars) |
| 0x21 | TIME | app → knob | `unix u32, tz_min i16` (minutes east of UTC) |
| 0x30 | LAMP_STATE | knob → app | `changed u8, power u8, bright u8 (1–100), ct u16 (K), bg_power u8, bg_rgb u32 (0xRRGGBB)`. `changed` bits: 1 power, 2 brightness, 4 warmth, 8 backlight power, 16 backlight colour |
| 0x31 | LOG | knob → app | `text str[200]` |
| 0x32 | SET_APP_VOLUME | knob → app | `id u32, vol u8, muted u8` |
| 0x33 | SET_MIC | knob → app | `muted u8` |
| 0x34 | SET_MONITOR | knob → app | `bright u8` (0–100) |
| 0x35 | TURN | knob → app | `mode u8, steps i8`: detents turned in an app mode (the mode from the last FOCUS), positive = clockwise |
| 0x40 | FW_BEGIN | app → knob | `size u32, crc32 u32, version str[15]` |
| 0x41 | FW_DATA | app → knob | `offset u32, data[≤1024]` |
| 0x42 | FW_END | app → knob | (empty) |
| 0x43 | FW_STATUS | knob → app | `state u8, offset u32, text str[63]`. States: 0 ok, 1 error, 2 done |

## Rules

- **On connect:**
  - Both sides send HELLO.
  - The app then sends a full snapshot: TIME, NOW_PLAYING, the COVER (if there is one), LYRICS or LYRICS_NONE,
    PC_STATS, AUDIO_APPS, MIC, MONITOR and FOCUS (each once it is known).
  - If the knob's HELLO has a different `proto`, Detent logs it and carries on; update the older side.
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
- **AUDIO_APPS and MIC:**
  - Sent when they change (Detent reads the Windows mixer every 500 ms).
  - One entry per app, for the default output device: Chrome's many sessions are one "Chrome". `id` is a hash of the
    app's name, so it stays the same across restarts.
  - SET_APP_VOLUME sets every session of that app. While the user is turning, the knob keeps showing its own value
    and ignores AUDIO_APPS for that app for about 500 ms, so older readings don't make it jump back.
  - SET_MIC mutes or unmutes the default recording device.
- **MONITOR:**
  - Sent at start, after every change, and when the level changes on the monitor itself (read every minute).
  - SET_MONITOR applies to every monitor that supports DDC/CI. The same 500 ms rule as AUDIO_APPS applies.
- **FOCUS and TURN:**
  - FOCUS is sent whenever the program in front changes. The mode comes from `appModes` in Detent's settings.
  - In mode 0 the knob works as the volume knob (HID keys). In modes 1–3 it sends TURN instead, with the mode it
    is in. Detent drops a TURN whose mode isn't the current one (sent just before a switch). Detent turns that
    into mouse-wheel input for the window under the pointer:
    - scroll: the plain wheel, with clockwise scrolling down. Fusion 360 and KiCad zoom with this.
    - zoom: Ctrl + wheel, with clockwise zooming in.
    - sideways: the horizontal wheel, with clockwise moving right.
  - The knob shows `label` while an app mode is active. A long press could hand the turn back to volume
    (knob firmware decides).
- **Firmware update** (Detent: "update firmware"):
  1. The app sends FW_BEGIN. The knob prepares its spare OTA slot (erasing can take a few seconds; the app waits up
     to 20 s), then answers FW_STATUS ok with offset 0.
  2. The app sends FW_DATA at the offset the knob asked for. The knob writes it and answers FW_STATUS ok with the
     next offset it wants. It may ask for the same offset again to get a chunk resent. The app gives up after 3 s
     without an answer, or after 3 answers in a row that make no progress.
  3. After the last chunk the app sends FW_END. The knob checks the size and the CRC-32 (IEEE, as in zlib and
     `esp_rom_crc32_le`), marks the new slot to boot, answers FW_STATUS done and reboots.
  4. FW_STATUS error (with `text`) at any point stops the update; the knob keeps running its old firmware.
  - If FW_END gets no answer, the result is unknown: the knob may have rebooted into the new firmware before
    answering. Detent says so instead of reporting a failure; the HELLO after the reconnect shows the version.
  - The image is the app `.bin` from the ESP-IDF build. Detent checks that it is one (magic 0xE9 and an app
    description) and shows its version before sending.
  - While an update runs, the app sends nothing else. After it, the knob reconnects and gets a new snapshot.
