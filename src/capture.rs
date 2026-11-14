// Screen sync: samples the main screen ~30 times a second and streams its
// dominant colour to the backlight.
//
// Capture uses DXGI desktop duplication: the GPU copies the screen into a
// texture and shrinks it with mipmaps, so only a ~160×100 image reaches the
// CPU, and only when the screen changed. GDI is the fallback (e.g. HDR
// screens), at a much higher CPU cost.

use crate::lamp::Lamp;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const FRAME: Duration = Duration::from_millis(33);

enum Sample {
    Color([f64; 3]),
    /// Nearly black screen: keep the last colour
    Dark,
    /// Nothing changed on screen since the last frame
    Same,
}

/// Dominant colour of a BGRA image at full intensity, favouring vivid, bright pixels
fn dominant(px: &[u8], width: usize, height: usize, stride: usize) -> Sample {
    let (mut r, mut g, mut b, mut w) = (0.0, 0.0, 0.0, 0.0);
    for row in 0..height {
        for p in px[row * stride..row * stride + width * 4].chunks_exact(4) {
            let (pb, pg, pr) = (p[0] as f64, p[1] as f64, p[2] as f64);
            let max = pr.max(pg).max(pb);
            let min = pr.min(pg).min(pb);
            let sat = if max > 0.0 { (max - min) / max } else { 0.0 };
            let wt = 0.03 + sat * sat * (max / 255.0);
            r += pr * wt;
            g += pg * wt;
            b += pb * wt;
            w += wt;
        }
    }
    if w == 0.0 {
        return Sample::Dark;
    }
    let (r, g, b) = (r / w, g / w, b / w);
    let peak = r.max(g).max(b);
    if peak < 10.0 {
        return Sample::Dark;
    }
    let k = 255.0 / peak; // full intensity; the brightness slider sets the level
    Sample::Color([r * k, g * k, b * k])
}

trait Source {
    fn sample(&mut self) -> Result<Sample, ()>;
}

mod dxgi {
    use super::{dominant, Sample, Source};
    use windows::core::Interface;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
    use windows::Win32::Graphics::Dxgi::*;

    pub struct Duplication {
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        output: IDXGIOutput1,
        dup: Option<IDXGIOutputDuplication>,
        mip: ID3D11Texture2D,
        srv: ID3D11ShaderResourceView,
        staging: ID3D11Texture2D,
        level: u32,
        small: (u32, u32),
        buf: Vec<u8>,
    }

    impl Duplication {
        pub fn new() -> windows::core::Result<Self> {
            unsafe {
                let mut device = None;
                let mut context = None;
                D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_HARDWARE,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )?;
                let device: ID3D11Device = device.unwrap();
                let context = context.unwrap();

                // The main screen is the output at the desktop origin
                let adapter = device.cast::<IDXGIDevice>()?.GetAdapter()?;
                let mut output = None;
                let mut i = 0;
                while let Ok(o) = adapter.EnumOutputs(i) {
                    let d = o.GetDesc()?;
                    if output.is_none() || (d.DesktopCoordinates.left == 0 && d.DesktopCoordinates.top == 0) {
                        output = Some((o, d));
                    }
                    i += 1;
                }
                let (output, desc) = output.ok_or_else(windows::core::Error::empty)?;
                let output: IDXGIOutput1 = output.cast()?;
                let (w, h) = (
                    (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left) as u32,
                    (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top) as u32,
                );

                // Full-size texture with a mip chain; the GPU averages it down
                let mut tex_desc = D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 0,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: D3D11_RESOURCE_MISC_GENERATE_MIPS.0 as u32,
                };
                let mut mip = None;
                device.CreateTexture2D(&tex_desc, None, Some(&mut mip))?;
                let mip = mip.unwrap();
                let mut srv = None;
                device.CreateShaderResourceView(&mip, None, Some(&mut srv))?;

                // Read back the first level that is at most ~160 pixels wide
                let mut level = 0;
                while (w >> level) > 160 && (h >> level) > 16 {
                    level += 1;
                }
                let small = ((w >> level).max(1), (h >> level).max(1));
                tex_desc.Width = small.0;
                tex_desc.Height = small.1;
                tex_desc.MipLevels = 1;
                tex_desc.Usage = D3D11_USAGE_STAGING;
                tex_desc.BindFlags = 0;
                tex_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                tex_desc.MiscFlags = 0;
                let mut staging = None;
                device.CreateTexture2D(&tex_desc, None, Some(&mut staging))?;

                let mut me = Duplication {
                    device,
                    context,
                    output,
                    dup: None,
                    mip,
                    srv: srv.unwrap(),
                    staging: staging.unwrap(),
                    level,
                    small,
                    buf: Vec::new(),
                };
                me.duplicate()?;
                Ok(me)
            }
        }

        fn duplicate(&mut self) -> windows::core::Result<()> {
            self.dup = None;
            self.dup = Some(unsafe { self.output.DuplicateOutput(&self.device)? });
            Ok(())
        }

        fn grab(&mut self) -> windows::core::Result<Option<Sample>> {
            let dup = match &self.dup {
                Some(d) => d.clone(),
                None => {
                    self.duplicate()?;
                    self.dup.clone().unwrap()
                }
            };
            unsafe {
                let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
                let mut res = None;
                match dup.AcquireNextFrame(0, &mut info, &mut res) {
                    Ok(()) => {}
                    Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(Some(Sample::Same)),
                    Err(e) => {
                        // Mode change, secure desktop (UAC), etc.: start over
                        self.dup = None;
                        return Err(e);
                    }
                }
                // Only the mouse moved: the picture is the same
                if info.LastPresentTime == 0 {
                    let _ = dup.ReleaseFrame();
                    return Ok(Some(Sample::Same));
                }
                let frame: ID3D11Texture2D = res.unwrap().cast()?;
                let mut fd = D3D11_TEXTURE2D_DESC::default();
                frame.GetDesc(&mut fd);
                if fd.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
                    let _ = dup.ReleaseFrame();
                    return Ok(None); // e.g. HDR: use GDI instead
                }
                self.context.CopySubresourceRegion(&self.mip, 0, 0, 0, 0, &frame, 0, None);
                let _ = dup.ReleaseFrame();
                self.context.GenerateMips(&self.srv);
                self.context.CopySubresourceRegion(&self.staging, 0, 0, 0, 0, &self.mip, self.level, None);
                let mut map = D3D11_MAPPED_SUBRESOURCE::default();
                self.context.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut map))?;
                let (w, h) = (self.small.0 as usize, self.small.1 as usize);
                let stride = map.RowPitch as usize;
                let src = std::slice::from_raw_parts(map.pData as *const u8, stride * h);
                self.buf.clear();
                self.buf.extend_from_slice(src);
                self.context.Unmap(&self.staging, 0);
                Ok(Some(dominant(&self.buf, w, h, stride)))
            }
        }
    }

    impl Source for Duplication {
        fn sample(&mut self) -> Result<Sample, ()> {
            match self.grab() {
                Ok(Some(s)) => Ok(s),
                Ok(None) => Err(()),
                // Lost access (e.g. UAC prompt): report "no change" and retry next frame
                Err(_) => Ok(Sample::Same),
            }
        }
    }
}

mod gdi {
    use super::{dominant, Sample, Source};
    use windows_sys::Win32::Graphics::Gdi::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

    const W: i32 = 160;
    const H: i32 = 90;

    pub struct Gdi {
        screen: HDC,
        mem: HDC,
        bmp: HBITMAP,
        bits: *const u8,
    }

    impl Gdi {
        pub fn new() -> Option<Gdi> {
            unsafe {
                let screen = GetDC(std::ptr::null_mut());
                let mem = CreateCompatibleDC(screen);
                let mut bi: BITMAPINFO = std::mem::zeroed();
                bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
                bi.bmiHeader.biWidth = W;
                bi.bmiHeader.biHeight = -H; // top-down
                bi.bmiHeader.biPlanes = 1;
                bi.bmiHeader.biBitCount = 32;
                let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
                let bmp = CreateDIBSection(mem, &bi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
                if bmp.is_null() || bits.is_null() {
                    DeleteDC(mem);
                    ReleaseDC(std::ptr::null_mut(), screen);
                    return None;
                }
                SelectObject(mem, bmp);
                // Point sampling: averaging the whole screen (HALFTONE) costs far more
                SetStretchBltMode(mem, COLORONCOLOR);
                Some(Gdi { screen, mem, bmp, bits: bits as *const u8 })
            }
        }
    }

    impl Source for Gdi {
        fn sample(&mut self) -> Result<Sample, ()> {
            let px = unsafe {
                let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
                StretchBlt(self.mem, 0, 0, W, H, self.screen, 0, 0, sw, sh, SRCCOPY);
                GdiFlush();
                std::slice::from_raw_parts(self.bits, (W * H * 4) as usize)
            };
            Ok(dominant(px, W as usize, H as usize, W as usize * 4))
        }
    }

    impl Drop for Gdi {
        fn drop(&mut self) {
            unsafe {
                DeleteObject(self.bmp);
                DeleteDC(self.mem);
                ReleaseDC(std::ptr::null_mut(), self.screen);
            }
        }
    }
}

pub struct ScreenSync {
    running: AtomicBool,
    /// 0 = instant … 10 = ambient
    pub smoothing: AtomicU32,
}

pub struct Frame {
    pub rgb: u32,
    pub rate: u32,
}

impl ScreenSync {
    pub fn new(smoothing: u32) -> Arc<Self> {
        Arc::new(ScreenSync { running: AtomicBool::new(false), smoothing: AtomicU32::new(smoothing) })
    }

    pub fn running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }

    /// `on_frame` gets the current colour about 10 times a second, for the preview.
    /// `dry_run` skips sending to the lamp (for testing).
    pub fn start(self: &Arc<Self>, lamp: Arc<Lamp>, dry_run: bool, on_frame: impl Fn(Frame) + Send + 'static) {
        if self.running.swap(true, Ordering::Relaxed) {
            return;
        }
        let me = self.clone();
        std::thread::spawn(move || {
            let mut source: Box<dyn Source> = match dxgi::Duplication::new() {
                Ok(d) => Box::new(d),
                Err(e) => {
                    crate::winutil::log(&format!("screen sync: desktop duplication unavailable ({e}), using GDI"));
                    match gdi::Gdi::new() {
                        Some(g) => Box::new(g),
                        None => {
                            me.running.store(false, Ordering::Relaxed);
                            return;
                        }
                    }
                }
            };
            // Without screen changes the smoothing still has to settle, so keep
            // stepping towards the last captured colour.
            let mut target: Option<[f64; 3]> = None;
            let mut ema: Option<[f64; 3]> = None;
            let mut last: Option<u32> = None;
            let (mut sent, mut rate_at, mut shown_at) = (0u32, Instant::now(), Instant::now());
            let mut rate = 0;
            while me.running.load(Ordering::Relaxed) {
                let t0 = Instant::now();
                match source.sample() {
                    Ok(Sample::Color(c)) => target = Some(c),
                    Ok(Sample::Dark | Sample::Same) => {}
                    Err(()) => {
                        crate::winutil::log("screen sync: switching to GDI capture");
                        if let Some(g) = gdi::Gdi::new() {
                            source = Box::new(g);
                        }
                    }
                }
                if let Some(c) = target {
                    let s = me.smoothing.load(Ordering::Relaxed).min(10);
                    let a = 1.0 - s as f64 / 12.0; // 1 = instant, lower = more smoothing between frames
                    let e = match ema {
                        Some(e) => [e[0] + (c[0] - e[0]) * a, e[1] + (c[1] - e[1]) * a, e[2] + (c[2] - e[2]) * a],
                        None => c,
                    };
                    ema = Some(e);
                    let rgb = ((e[0].round() as u32) << 16) | ((e[1].round() as u32) << 8) | e[2].round() as u32;
                    let diff = last.map_or(999, |l| {
                        let d = |sh: u32| ((rgb >> sh & 255) as i32 - (l >> sh & 255) as i32).unsigned_abs();
                        d(16) + d(8) + d(0)
                    });
                    if diff >= 4 {
                        if !dry_run {
                            lamp.stream(rgb, if s == 0 { 0 } else { 30 + s * 20 });
                        }
                        last = Some(rgb);
                        sent += 1;
                    }
                    if shown_at.elapsed() >= Duration::from_millis(100) {
                        shown_at = Instant::now();
                        on_frame(Frame { rgb, rate });
                    }
                }
                if rate_at.elapsed() >= Duration::from_secs(1) {
                    rate = (sent as f64 / rate_at.elapsed().as_secs_f64()).round() as u32;
                    sent = 0;
                    rate_at = Instant::now();
                }
                std::thread::sleep(FRAME.saturating_sub(t0.elapsed()));
            }
        });
    }
}
