// PC stats for the knob's PC screen (never shown in Detent itself): GPU temperature and load and CPU
// temperature from MSI Afterburner's shared memory; CPU load, RAM use and the host name from Windows.

use detent::mahm;
use detent::protocol::Msg;
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
use windows_sys::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, VirtualQuery, FILE_MAP_READ, MEMORY_BASIC_INFORMATION,
    MEMORY_MAPPED_VIEW_ADDRESS,
};
use windows_sys::Win32::System::SystemInformation::{
    ComputerNamePhysicalDnsHostname, GetComputerNameExW, GlobalMemoryStatusEx, MEMORYSTATUSEX,
};
use windows_sys::Win32::System::Threading::GetSystemTimes;

pub struct Sampler {
    last_cpu: Option<(u64, u64)>, // (idle, total) from the previous sample
    host: String,
}

fn ft(f: FILETIME) -> u64 {
    ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
}

/// A copy of Afterburner's shared memory, if it's running
fn read_mahm() -> Option<Vec<u8>> {
    let name: Vec<u16> = "MAHMSharedMemory\0".encode_utf16().collect();
    unsafe {
        let h = OpenFileMappingW(FILE_MAP_READ, 0, name.as_ptr());
        if h.is_null() {
            return None;
        }
        let view = MapViewOfFile(h, FILE_MAP_READ, 0, 0, 0);
        let out = if view.Value.is_null() {
            None
        } else {
            let mut info: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
            let ok = VirtualQuery(view.Value, &mut info, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) != 0;
            let copy = ok.then(|| std::slice::from_raw_parts(view.Value as *const u8, info.RegionSize).to_vec());
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: view.Value });
            copy
        };
        CloseHandle(h);
        out
    }
}

fn host_name() -> String {
    let mut buf = [0u16; 64];
    let mut len = buf.len() as u32;
    unsafe {
        if GetComputerNameExW(ComputerNamePhysicalDnsHostname, buf.as_mut_ptr(), &mut len) == 0 {
            return String::new();
        }
    }
    String::from_utf16_lossy(&buf[..len as usize])
}

fn pct(v: Option<f32>) -> Option<u8> {
    v.map(|v| v.round().clamp(0.0, 254.0) as u8)
}

impl Sampler {
    pub fn new() -> Self {
        Sampler { last_cpu: None, host: host_name() }
    }

    /// One PC_STATS message. CPU load is measured since the previous call (missing on the first).
    pub fn sample(&mut self) -> Msg {
        let m = read_mahm().and_then(|b| mahm::parse(&b)).unwrap_or_default();

        let mut cpu_pct = None;
        unsafe {
            let (mut idle, mut kernel, mut user) = std::mem::zeroed::<(FILETIME, FILETIME, FILETIME)>();
            if GetSystemTimes(&mut idle, &mut kernel, &mut user) != 0 {
                // kernel time includes idle time
                let (idle, total) = (ft(idle), ft(kernel) + ft(user));
                if let Some((li, lt)) = self.last_cpu {
                    let dt = total.saturating_sub(lt);
                    if dt > 0 {
                        let busy = dt.saturating_sub(idle.saturating_sub(li));
                        cpu_pct = Some((busy * 100 / dt).min(100) as u8);
                    }
                }
                self.last_cpu = Some((idle, total));
            }
        }

        let mut ram_pct = None;
        unsafe {
            let mut ms: MEMORYSTATUSEX = std::mem::zeroed();
            ms.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
            if GlobalMemoryStatusEx(&mut ms) != 0 {
                ram_pct = Some(ms.dwMemoryLoad.min(100) as u8);
            }
        }

        Msg::PcStats {
            cpu_pct,
            cpu_c: pct(m.cpu_c),
            gpu_pct: pct(m.gpu_pct),
            gpu_c: pct(m.gpu_c),
            ram_pct,
            host: self.host.clone(),
        }
    }
}
