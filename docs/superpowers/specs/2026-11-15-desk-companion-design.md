# Desk companion: Monitor Lamp grows into the knob's PC app

Date: 2026-11-15 · Status: draft for approval

## Why

Cat has a Windows tray app, Monitor Lamp, that controls the Yeelight Screen Light Bar Pro (`yeelink.light.lamp15`).
It's mostly used to switch the lamp on and off with the PC. It works well, but its interface "feels AI-looking".

Cat is also building a motorized haptic knob (repo `Cat-in-a-Box-0115/motorized-knob`). The knob needs a PC companion
for its MEDIA, lyrics and PC screens. Cat also wants the knob to **replace the lamp's own wireless dial**, which they
find poor.

The goal is one lightweight app that does both jobs, with a calmer, minimal look.

## Goals

1. **Keep everything Monitor Lamp does today, with no regressions:**
   - On at sign-in, off at sign-out, shutdown and sleep.
   - Brightness, warmth and backlight (solid colour, flows, screen sync).
   - Time of day.
   - Start with Windows, tray icon.
2. **New interface in the agreed look** (see "Look"): a tray popup for daily use, plus a restyled full window for
   settings and backlight modes.
3. **Companion data for the knob:**
   - Now playing (title, artist, playing/paused, position, duration, cover) from Windows' media controls (SMTC).
     This covers Melodia, which publishes SMTC with cover and timeline (`desktopApp/native-src/smtc`), and
     YouTube in a browser.
   - Synced lyrics from LRCLIB, matched to the playback position.
   - CPU and GPU temperatures read from MSI Afterburner, which Cat already runs.
4. **A link to the knob** that sends this data and set-up details, and receives the knob's lamp state so the popup
   stays in sync.
5. **Stay light:** a single exe, around 40 MB of memory, 0% CPU when idle.

## Non-goals

- A web-based interface (Tauri, Electron, WebView). Cat ruled this out.
- Rewriting the parts that work: the miIO client, the shutdown hooks, screen capture and flows stay as they are.
- macOS support.
- Emulating the stock dial's radio protocol. It isn't documented, and the miIO path (below) does the job better.

## Anti-goals (a failure even if it works)

- It looks generic or "AI-made": outlined cards everywhere, icons on everything, gradients, explanatory hint text
  under every setting.
- It's busy: too many separate elements competing in the popup.
- Monitor Lamp's on/off reliability gets worse.
- Volume or media keys only work while the app is running.

## Look (agreed with Cat in mockups)

Cat chose a mix: C's look, B's controls, A's dividing lines. The final pick was the "soft tiles" version.

- **Ground:** near-black `#0E0E0E`. Text `#E8E5DE`, muted text `#6A6762`. Lines `#242321`, 1 px, full-bleed,
  separating sections.
- **Font:** monospace everywhere (JetBrains Mono, embedded in the exe). Small spaced-out uppercase section labels
  in muted text: `LAMP`, `PLAYING`.
- **Popup, top to bottom:**
  1. **Lamp:**
     - Three flat tiles (no borders): `desk`, `back`, `auto`. Each shows its state (`on` / `sync` / `off`)
       underneath. A lighter shade means on. `desk` and `back` have a 6 px dot in the light's real colour.
     - Two thin sliders (2 px track, tick handle) with the value on the right: brightness `70%`, and warmth
       `4000K`, whose track is a muted warm-to-cool strip.
  2. **Playing:** a 42 px cover, title, artist in muted text, and `|◀ ❚❚ ▶|`. No progress bar.
  3. **Footer:** `cpu 52°  gpu 61°` on the left; knob status on the right (a dot, or `knob usb` / `knob ble`).
- **What's left out:** icons, outlined boxes, progress bars, slider symbols, hint text.
- **Behaviour:**
  - The popup opens from a left-click on the tray icon, anchored to the tray like Windows' volume flyout.
  - It closes when it loses focus.
  - Right-clicking the `back` tile, or the tray menu, opens the full window.
  - Sections without data are hidden: no music means no PLAYING section, no Afterburner means no temperatures.
- **Full window:** the same language. Plain rows separated by lines, no cards, no hint text. It covers:
  - Backlight modes (colour swatches, flows, screen sync).
  - Time of day.
  - With the PC.
  - Lamp IP and token.
  - Knob.
  - Starting up.

  It replaces today's `ui/app.slint` layout.

The software renderer stays, because GPU renderers cost about 150 MB. So shapes stay flat, and gradients only
appear in square-cornered strips.

## Approaches considered

1. **Extend Monitor Lamp: Rust, Slint software renderer (chosen).** All the hard parts are already solved, and
   memory stays around 30–40 MB. New Windows APIs are reached through the `windows` crate, which is already a
   dependency.
2. **Rewrite in C#/.NET.** Some APIs are easier, but it uses about twice the memory and means redoing the working
   lamp code.
3. **Web-based (Tauri/Electron).** 100–200 MB of memory. Monitor Lamp already moved away from Electron for this
   reason, and Cat ruled it out.

Cat: "whichever is easier, but don't use web based". I picked 1.

## Architecture: who talks to whom

```
            Wi-Fi (miIO, UDP 54321)
   knob  ───────────────────────────►  lamp  ◄──── Wi-Fi (miIO) ──── PC app
     │                                                               ▲
     └──── USB (v0, wired)  or  BLE (wireless v2) ───────────────────┘
           • HID volume/media keys  → Windows directly (no app needed)
           • data channel           ↔ PC app (now playing, cover, lyrics, temps, set-up)
```

### Lamp: the knob talks to it directly

This is what lets the knob replace the stock dial: it works while the PC is off or asleep.

- The knob uses the same miIO protocol over Wi-Fi as the app (no command limit, about 40 commands a second).
- Both the knob and the app control the lamp. The app keeps polling the lamp's state (it already does), so its
  popup follows changes made on the knob.
- **Who wins (my call):** a change made on the knob counts as a manual change, exactly like one in the app today:
  - a brightness or warmth change pauses time of day (the existing `touched()` rule);
  - a backlight change stops screen sync or a running flow.

  The app recognises knob changes from the knob's `LAMP_STATE` report. That makes `LAMP_STATE` required, not
  optional. While the knob is disconnected, the app treats a polled change it didn't make itself in the same way.
- **Set-up:** the app sends the knob the Wi-Fi name and password, and the lamp's IP and token, over the data
  channel. The knob stores them in its flash. This replaces the SoftAP web-page set-up in the knob plan.
  The token never goes in the hub or the repo.

### PC link: USB now, BLE when the knob goes wireless

- The knob is a USB device (v0 board, and any wired build). It's a composite device:
  - HID consumer control for volume, mute and media keys. Windows handles these natively, so they work without
    the app.
  - A CDC serial port for the data channel.
- On a wireless build it uses BLE instead:
  - HID-over-GATT for the same keys.
  - A custom GATT service for the data channel.
- Why BLE and not Wi-Fi for the PC link:
  - BLE HID keys work with no app and no network set-up.
  - It uses far less power than Wi-Fi for a battery build.
  - It avoids firewall prompts and finding the knob on the network.
- Wi-Fi is still used for the lamp and for weather and time. The ESP32-S3 (v1/v2) and the S31 (v0) both run
  Wi-Fi and BLE together.
- The data protocol is the same over both transports. The app has a `Link` trait with a USB-serial
  implementation first and a BLE (WinRT GATT) implementation later.

### Data channel protocol (v1)

- **Framing:** each message is `type u8, len u16 LE, payload`. Over serial, frames are COBS-encoded with a `0x00`
  delimiter. Over BLE, a frame is split across writes and notifications (builder's call).
- **App → knob:**
  - `HELLO(proto_ver, app_ver)`
  - `NOW_PLAYING(playing, pos_ms, dur_ms, title, artist)`
  - `COVER(art_id, w, h, chunk_idx, chunk_count, bytes)`: RGB565, sized to the knob's art size, sent only when
    `art_id` changes.
  - `LYRICS(line_id, prev, cur, next)`, or `LYRICS_NONE`, sent when the line changes. The knob's
    `knob_lyrics_t` is already this shape.
  - `PC_STATS(cpu_pct, cpu_c, gpu_pct, gpu_c, ram_pct, host)`, sent every 2 s, with a field marking a value as
    missing. This matches the knob's `knob_pc_t` and its PC screen, which shows CPU, GPU and RAM bars and the
    host name.
  - `PROVISION(ssid, pass, lamp_ip, lamp_token)`: sent only from a **"Set up knob"** button in the full window's Knob
    section (my call):
    - Cat types the Wi-Fi name and password there.
    - The password is sent once and not saved.
    - The lamp IP and token come from the existing lamp settings.
    - It is not read from Windows' saved Wi-Fi profiles, because that needs admin rights and the PC may be on
      Ethernet.
  - `TIME(unix, tz_offset)`, as a fallback when the knob has no network time yet.
- **Knob → app:**
  - `HELLO(proto_ver, fw_ver, caps)`
  - `LAMP_STATE(power, bright, ct, bg_power, bg_rgb, source)`: sent after each lamp command the knob sends.
  - `LOG(text)`, for debugging.
- The knob's existing hooks map to these one-to-one: `knob_data_media`, `knob_data_lyrics`, `knob_data_pc`,
  `knob_data_time`.

## New app modules

- `media.rs`: SMTC session manager. Follow the current session; read title, artist, status, timeline and thumbnail.
  React to change events rather than polling. Decode and resize the thumbnail for the popup and the knob.
- `lyrics.rs`: LRCLIB `GET /api/get` by title, artist and duration, falling back to `/api/search`. Parse synced LRC,
  pick the current line from the SMTC position plus elapsed time, and cache to disk per track.
  Lyrics are only passed to the knob, never stored in the repo.
- `stats.rs`: MSI Afterburner shared memory (`MAHMSharedMemory`). Read the GPU and CPU temperature, GPU usage,
  CPU usage and RAM usage entries. The host name comes from Windows. If Afterburner isn't running or the entry is missing, the value is missing and the
  popup hides it. CPU temperature needs Afterburner's monitoring of that sensor turned on; the full window says
  so in one line.
- `link.rs`: the `Link` trait, USB-serial discovery by the knob's VID/PID (my call: Espressif's VID `0x303A` with a
  project PID; both repos define it in one shared place in the protocol doc), reconnect, framing, and the message
  types above.
- `ui/popup.slint` and the new `ui/app.slint`, in the look above.

## Constraints

- Windows 10/11 only. It's built on the PC with MSVC (Rust and the Build Tools are installed there).
- It can be written and cross-compiled on the Mac to check it builds (`cargo-xwin`, which needs installing), but it
  can only be run and tested on the PC. Hand the testing to the PC through the hub inbox.
- Settings live in `%APPDATA%\Monitor Lamp\settings.json`. New fields are optional with defaults, so existing
  settings keep working.
- No secrets in the repo or the hub. The repo `Cat-in-a-Box-0115/monitor-lamp` is public.

## Testing without the knob

No knob firmware has the data port yet.
- A **fake knob** script talks to the app over a virtual COM-port pair on the PC. It sends `HELLO` and `LAMP_STATE`,
  and checks the frames it gets back.
- The protocol is written up as `docs/knob-protocol.md` in this repo, and the knob repo links to it.

## Left to the builder

- The app's name: it keeps "Monitor Lamp" until Cat picks a new one.
- How the popup is anchored and sized per DPI, and how it closes on focus loss.
- Chunk sizes, the cover's pixel size, and retry and timeout details of the protocol.
- How cached lyrics are stored.
- Tests: unit tests for the framing, LRC parsing and current-line selection, the MAHM parser (against a captured
  sample) and settings migration. These run on the Mac with `cargo test` for the platform-neutral modules.
- The knob-side firmware (USB composite, provisioning, miIO client) is planned separately in the knob repo, against
  this protocol.
