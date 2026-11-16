// Per-app volume and the microphone, from Windows' Core Audio. One thread owns the COM objects: it reads the
// mixer every 500 ms (cheap) and applies the knob's changes as they arrive.

use detent::apps::{self, Session};
use detent::protocol::AudioApp;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;
use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Foundation::BOOL;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, AudioSessionStateExpired, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator,
    ISimpleAudioVolume, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};

pub enum Cmd {
    App { id: u32, vol: u8, muted: bool },
    Mic(bool),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Audio {
    pub apps: Vec<AudioApp>,
    /// None: no microphone
    pub mic: Option<bool>,
}

/// The default output device's app sessions, without system sounds and finished sessions
unsafe fn sessions(en: &IMMDeviceEnumerator) -> windows::core::Result<Vec<(String, ISimpleAudioVolume)>> {
    let dev = en.GetDefaultAudioEndpoint(eRender, eConsole)?;
    let mgr: IAudioSessionManager2 = dev.Activate(CLSCTX_ALL, None)?;
    let list = mgr.GetSessionEnumerator()?;
    let mut out = Vec::new();
    for i in 0..list.GetCount()? {
        let Ok(ctl) = list.GetSession(i) else { continue };
        if ctl.GetState().map_or(true, |s| s == AudioSessionStateExpired) {
            continue;
        }
        let Ok(ctl2) = ctl.cast::<IAudioSessionControl2>() else { continue };
        if ctl2.IsSystemSoundsSession() == HRESULT(0) {
            continue;
        }
        let pid = ctl2.GetProcessId().unwrap_or(0);
        let exe = if pid == 0 { String::new() } else { crate::winutil::exe_of(pid).unwrap_or_default() };
        if exe.is_empty() {
            continue;
        }
        if let Ok(vol) = ctl.cast::<ISimpleAudioVolume>() {
            out.push((exe, vol));
        }
    }
    Ok(out)
}

unsafe fn mic(en: &IMMDeviceEnumerator) -> Option<IAudioEndpointVolume> {
    en.GetDefaultAudioEndpoint(eCapture, eConsole).ok()?.Activate(CLSCTX_ALL, None).ok()
}

unsafe fn read(en: &IMMDeviceEnumerator) -> Audio {
    let list: Vec<Session> = sessions(en)
        .unwrap_or_default()
        .iter()
        .map(|(exe, v)| Session {
            exe: exe.clone(),
            vol: v.GetMasterVolume().unwrap_or(0.0),
            muted: v.GetMute().map(|b| b.as_bool()).unwrap_or(false),
        })
        .collect();
    Audio { apps: apps::group(&list), mic: mic(en).and_then(|m| m.GetMute().ok()).map(|b| b.as_bool()) }
}

unsafe fn apply(en: &IMMDeviceEnumerator, cmd: Cmd) {
    let none: *const GUID = std::ptr::null();
    match cmd {
        Cmd::App { id, vol, muted } => {
            // Every session of that app (Chrome has several) gets the same volume
            for (exe, v) in sessions(en).unwrap_or_default() {
                if apps::app_id(&apps::display_name(&exe)) == id {
                    let _ = v.SetMasterVolume(vol.min(100) as f32 / 100.0, none);
                    let _ = v.SetMute(BOOL::from(muted), none);
                }
            }
        }
        Cmd::Mic(muted) => {
            if let Some(m) = mic(en) {
                let _ = m.SetMute(BOOL::from(muted), none);
            }
        }
    }
}

/// Starts the audio thread. `on_change` gets the apps and mic state at start and whenever they change.
pub fn start(on_change: impl Fn(Audio) + Send + 'static) -> Sender<Cmd> {
    let (tx, rx) = mpsc::channel::<Cmd>();
    thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let en: IMMDeviceEnumerator = match CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) {
            Ok(e) => e,
            Err(e) => {
                crate::winutil::log(&format!("audio: no Core Audio ({e})"));
                return;
            }
        };
        let mut last: Option<Audio> = None;
        loop {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(cmd) => {
                    apply(&en, cmd);
                    // A turn sends many changes: apply them all before reading back
                    while let Ok(cmd) = rx.try_recv() {
                        apply(&en, cmd);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            let now = read(&en);
            if last.as_ref() != Some(&now) {
                last = Some(now.clone());
                on_change(now);
            }
        }
    });
    tx
}
