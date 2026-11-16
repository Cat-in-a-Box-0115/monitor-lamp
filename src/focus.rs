// The program in front (for the knob's per-app modes) and the wheel input a knob turn becomes there.

use detent::apps::Wheel;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_WHEEL,
    MOUSEINPUT, VK_CONTROL,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

/// The foreground window's process id (0 when there is none)
pub fn foreground_pid() -> u32 {
    let mut pid = 0u32;
    unsafe {
        let w = GetForegroundWindow();
        if !w.is_null() {
            GetWindowThreadProcessId(w, &mut pid);
        }
    }
    pid
}

fn key(vk: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: 0, dwFlags: if up { KEYEVENTF_KEYUP } else { 0 }, time: 0, dwExtraInfo: 0 } },
    }
}

/// Sends the wheel input to whatever is under the mouse, holding Ctrl around it for zoom
pub fn inject(w: &Wheel) {
    let wheel = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: w.delta as u32,
                dwFlags: if w.horizontal { MOUSEEVENTF_HWHEEL } else { MOUSEEVENTF_WHEEL },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let inputs: Vec<INPUT> = if w.ctrl { vec![key(VK_CONTROL, false), wheel, key(VK_CONTROL, true)] } else { vec![wheel] };
    unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
}
