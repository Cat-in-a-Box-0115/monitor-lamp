# Monitor Lamp

A small Windows app that controls a **Yeelight Screen Light Bar** (`yeelink.light.lamp15`) over your
local network, with no cloud involved.

- **Desk light:** on/off, brightness and colour temperature.
- **Backlight:** solid colours, smooth colour flows, or real-time **screen sync** (matches your main
  screen's colour about 30 times a second).
- **Time of day:** cooler light by day, warmer and dimmer towards bedtime.
- **Follows your PC:** starts with Windows and turns the lamp on when you sign in; both lights fade
  off at shutdown, sign-out and sleep, and come back on when the PC wakes; optionally follows lock.

One ~11 MB exe, about 30 MB of memory, no CPU when idle; screen sync uses about 2% of one core.

## Setup
You need the lamp's IP address and its **token** (its local-network key). The token can be read
from your Xiaomi/Mijia account with a token extractor such as
[Xiaomi-cloud-tokens-extractor](https://github.com/PiotrMachowski/Xiaomi-cloud-tokens-extractor).
Enter both in the app under **Settings → Lamp** and press **Save and connect**.

Settings are stored in `%APPDATA%\Monitor Lamp\settings.json`; the log is
`%APPDATA%\Monitor Lamp\monitor-lamp.log` (tray menu → Open log).

## How it talks to the lamp
It uses the Xiaomi **miIO** protocol (UDP port 54321, AES-encrypted with the token). Unlike
Yeelight's own LAN protocol (TCP 55443, capped at about 60 commands a minute), miIO has no command
quota, which is what makes screen sync possible.

## Build & install
Needs Rust (MSVC toolchain) and the Visual Studio C++ Build Tools.
```
cargo build --release
powershell -ExecutionPolicy Bypass -File install.ps1            # installs to %LOCALAPPDATA%\Programs
powershell -ExecutionPolicy Bypass -File install.ps1 -Uninstall
```
If the linker fails with `LNK1104` because the project's path is very long, point cargo's
`target-dir` at a short folder in `.cargo/config.toml`.

## Files
- `src/main.rs`: window, tray, startup, scheduling, UI wiring
- `src/miio.rs`: miIO protocol client; `src/lamp.rs` is the lamp wrapper and colour streaming
- `src/hooks.rs`: shutdown, sign-out, sleep and lock notifications, from a hidden window on its own
  thread. Windows ends the process right after sign-out, so "off" is sent synchronously there.
- `src/capture.rs`: screen sync, using DXGI desktop duplication and a GPU downscale, with GDI as a
  fallback
- `src/flow.rs`: colour flows, played by the app as a smooth colour stream
- `src/circadian.rs`: time-of-day curve
- `src/winutil.rs`: log, start with Windows, single instance, window styling, and keeping the
  window's size when it's dragged between screens with different scaling
- `ui/app.slint`: the interface (Slint, software renderer)
- Debug: set `LAMP_DRY_RUN=1` to run screen sync without sending colours to the lamp, and
  `LAMP_DPI_DEBUG=1` to log scale changes
