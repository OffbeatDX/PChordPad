use crate::monitor::MonInfo;
use std::sync::{Arc, Condvar, Mutex};

#[derive(Clone, Default)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub status: String,
    pub generation: u64,
    pub capture_ms: f32,
}

pub struct NavMirror {
    shared: Arc<CaptureShared>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

#[derive(Default)]
struct CaptureState {
    running: bool,
    stop: bool,
    target: Option<MonInfo>,
    epoch: u64,
}

struct CaptureShared {
    state: Mutex<CaptureState>,
    wake: Condvar,
    frame: Mutex<Frame>,
}

impl NavMirror {
    pub fn new() -> Arc<Self> {
        let shared = Arc::new(CaptureShared {
            state: Mutex::new(CaptureState::default()),
            wake: Condvar::new(),
            frame: Mutex::new(Frame {
                status: "idle".into(),
                ..Frame::default()
            }),
        });
        let worker_shared = shared.clone();
        let worker = std::thread::Builder::new()
            .name("nav-capture".into())
            .spawn(move || capture_worker(worker_shared, 1920))
            .map_err(|e| log::error!("could not start nav capture worker: {e}"))
            .ok();
        Arc::new(Self {
            shared,
            worker: Mutex::new(worker),
        })
    }

    pub fn start(&self, target: MonInfo, pad: &MonInfo) -> Result<(), String> {
        if target.same_screen(pad) {
            return Err("Nav monitor must be different from Pad monitor".into());
        }
        if self
            .worker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
        {
            return Err("Nav capture worker unavailable".into());
        }
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.target = Some(target);
            state.running = true;
            state.epoch = state.epoch.wrapping_add(1);
        }
        self.set_status("capturing…");
        self.shared.wake.notify_one();
        Ok(())
    }

    pub fn stop(&self) {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .running = false;
        self.shared.wake.notify_one();
        inject_mouse_up(self);
        self.set_status("idle");
    }

    fn set_status(&self, s: &str) {
        if let Ok(mut f) = self.shared.frame.lock() {
            f.status = s.into();
            f.generation = f.generation.wrapping_add(1);
        }
    }

    pub fn with_new_frame<R>(&self, generation: u64, f: impl FnOnce(&Frame) -> R) -> Option<R> {
        let frame = self.shared.frame.lock().unwrap_or_else(|e| e.into_inner());
        (frame.generation != generation).then(|| f(&frame))
    }

    pub fn pointer(&self, nx: f32, ny: f32, kind: i32) {
        let mon = {
            let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            if !state.running {
                return;
            }
            match state.target.clone() {
                Some(m) => m,
                None => return,
            }
        };
        let nx = nx.clamp(0.0, 1.0);
        let ny = ny.clamp(0.0, 1.0);
        let x = mon.left + (nx * (mon.width.max(1) as f32 - 1.0)).round() as i32;
        let y = mon.top + (ny * (mon.height.max(1) as f32 - 1.0)).round() as i32;
        match kind {
            0 => inject_mouse(x, y, MouseOp::Down),
            1 => inject_mouse(x, y, MouseOp::Move),
            _ => inject_mouse(x, y, MouseOp::Up),
        }
    }
}

impl Drop for NavMirror {
    fn drop(&mut self) {
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.running = false;
            state.stop = true;
        }
        self.shared.wake.notify_all();
        if let Some(worker) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = worker.join();
        }
    }
}

const FRAME_BUDGET: std::time::Duration = std::time::Duration::from_millis(16);
const FRAME_SLOW: std::time::Duration = std::time::Duration::from_millis(50);

fn capture_worker(shared: Arc<CaptureShared>, max_w: u32) {
    let mut capturer = Capturer::default();
    loop {
        let (target, epoch) = {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            while !state.running && !state.stop {
                state = shared.wake.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            if state.stop {
                return;
            }
            (state.target.clone(), state.epoch)
        };
        let Some(mon) = target else { continue };

        let started = std::time::Instant::now();
        let result = capturer.capture(&mon, max_w);
        let elapsed = started.elapsed();
        {
            let state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            if !state.running || state.epoch != epoch {
                continue;
            }
        }
        let mut frame = shared.frame.lock().unwrap_or_else(|e| e.into_inner());
        frame.generation = frame.generation.wrapping_add(1);
        frame.capture_ms = elapsed.as_secs_f32() * 1000.0;
        match result {
            Ok(shot) => {
                if elapsed > FRAME_SLOW {
                    log::warn!("nav capture took {:.1} ms", frame.capture_ms);
                }
                frame.width = shot.width;
                frame.height = shot.height;
                frame.status = shot.status;
                std::mem::swap(&mut frame.rgba, capturer.pixels_mut());
            }
            Err(e) => {
                frame.status = format!("capture failed: {e}");
            }
        }
        drop(frame);

        let state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.stop {
            return;
        }
        let wait = FRAME_BUDGET.saturating_sub(elapsed).max(elapsed);
        if wait.is_zero() {
            continue;
        }
        let _ = shared
            .wake
            .wait_timeout(state, wait)
            .unwrap_or_else(|e| e.into_inner());
    }
}

#[derive(Clone, Copy)]
enum MouseOp {
    Down,
    Move,
    Up,
}

#[cfg(windows)]
fn inject_mouse(screen_x: i32, screen_y: i32, op: MouseOp) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
        MOUSEINPUT,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SetCursorPos;

    unsafe {
        if SetCursorPos(screen_x, screen_y) == 0 {
            log::warn!("SetCursorPos({screen_x},{screen_y}) failed");
        }
    }

    match op {
        MouseOp::Move => {
            post_mouse(screen_x, screen_y, MouseOp::Move);
        }
        MouseOp::Down | MouseOp::Up => {
            let flag = match op {
                MouseOp::Down => MOUSEEVENTF_LEFTDOWN,
                MouseOp::Up => MOUSEEVENTF_LEFTUP,
                MouseOp::Move => unreachable!(),
            };
            let input = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: 0,
                        dy: 0,
                        mouseData: 0,
                        dwFlags: flag,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            let sent = unsafe { SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) };
            post_mouse(screen_x, screen_y, op);
            if sent != 1 {
                log::warn!(
                    "SendInput returned {sent}. If the game is elevated, run PChordPad as admin."
                );
            }
        }
    }
}

#[cfg(windows)]
fn post_mouse(screen_x: i32, screen_y: i32, op: MouseOp) {
    use windows_sys::Win32::Foundation::{LPARAM, POINT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetAncestor, PostMessageW, WindowFromPoint, GA_ROOT, WM_LBUTTONDOWN, WM_LBUTTONUP,
        WM_MOUSEMOVE,
    };

    const MK_LBUTTON: usize = 0x0001;

    unsafe {
        let mut pt = POINT {
            x: screen_x,
            y: screen_y,
        };
        let mut hwnd = WindowFromPoint(pt);
        if hwnd.is_null() {
            return;
        }
        let root = GetAncestor(hwnd, GA_ROOT);
        if !root.is_null() {
            hwnd = root;
        }
        ScreenToClient(hwnd, &mut pt);
        let lp = (((pt.y as u16) as u32) << 16) | ((pt.x as u16) as u32);
        match op {
            MouseOp::Down => {
                PostMessageW(hwnd, WM_LBUTTONDOWN, MK_LBUTTON as WPARAM, lp as LPARAM);
            }
            MouseOp::Up => {
                PostMessageW(hwnd, WM_LBUTTONUP, 0, lp as LPARAM);
            }
            MouseOp::Move => {
                PostMessageW(hwnd, WM_MOUSEMOVE, MK_LBUTTON as WPARAM, lp as LPARAM);
            }
        }
    }
}

#[cfg(not(windows))]
fn inject_mouse(_x: i32, _y: i32, _op: MouseOp) {}

#[cfg(not(windows))]
fn inject_mouse_up(_nav: &NavMirror) {}

#[cfg(windows)]
fn inject_mouse_up(nav: &NavMirror) {
    let mon = nav
        .shared
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .target
        .clone()
        .unwrap_or(MonInfo {
            index: 0,
            left: 0,
            top: 0,
            width: 1,
            height: 1,
            primary: true,
            device: String::new(),
            label: String::new(),
        });
    inject_mouse(
        mon.left + mon.width / 2,
        mon.top + mon.height / 2,
        MouseOp::Up,
    );
}

struct Shot {
    width: u32,
    height: u32,
    status: String,
}

fn target_size(mon: &MonInfo, max_w: u32) -> (u32, u32, u32, u32) {
    let src_w = mon.width.max(1) as u32;
    let src_h = mon.height.max(1) as u32;
    let scale = if src_w > max_w {
        max_w as f32 / src_w as f32
    } else {
        1.0
    };
    let dst_w = ((src_w as f32 * scale).round() as u32).max(1);
    let dst_h = ((src_h as f32 * scale).round() as u32).max(1);
    (src_w, src_h, dst_w, dst_h)
}

fn shot_status(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> String {
    if dst_w == src_w {
        format!("NAV  {src_w}×{src_h}  ·  touch = main screen")
    } else {
        format!("NAV  {src_w}×{src_h}→{dst_w}×{dst_h}  ·  touch = main screen")
    }
}

#[cfg(windows)]
#[derive(Default)]
struct Capturer {
    surface: Option<Surface>,
    pixels: Vec<u8>,
}

#[cfg(windows)]
struct Surface {
    mem_dc: windows_sys::Win32::Graphics::Gdi::HDC,
    bmp: windows_sys::Win32::Graphics::Gdi::HBITMAP,
    old: windows_sys::Win32::Graphics::Gdi::HGDIOBJ,
    width: u32,
    height: u32,
}

#[cfg(windows)]
impl Drop for Surface {
    fn drop(&mut self) {
        use windows_sys::Win32::Graphics::Gdi::{DeleteDC, DeleteObject, SelectObject};
        unsafe {
            SelectObject(self.mem_dc, self.old);
            DeleteObject(self.bmp as _);
            DeleteDC(self.mem_dc);
        }
    }
}

#[cfg(windows)]
impl Capturer {
    fn pixels_mut(&mut self) -> &mut Vec<u8> {
        &mut self.pixels
    }

    fn capture(&mut self, mon: &MonInfo, max_w: u32) -> Result<Shot, String> {
        use windows_sys::Win32::Graphics::Gdi::{GetDC, ReleaseDC};
        unsafe {
            let screen_dc = GetDC(std::ptr::null_mut());
            if screen_dc.is_null() {
                return Err("GetDC failed".into());
            }
            let out = self.capture_onto(screen_dc, mon, max_w);
            ReleaseDC(std::ptr::null_mut(), screen_dc);
            out
        }
    }

    unsafe fn capture_onto(
        &mut self,
        screen_dc: windows_sys::Win32::Graphics::Gdi::HDC,
        mon: &MonInfo,
        max_w: u32,
    ) -> Result<Shot, String> {
        use windows_sys::Win32::Graphics::Gdi::{
            BitBlt, GetDIBits, SetStretchBltMode, StretchBlt, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
            DIB_RGB_COLORS, HALFTONE, SRCCOPY,
        };

        let (src_w, src_h, dst_w, dst_h) = target_size(mon, max_w);
        self.ensure_surface(screen_dc, dst_w, dst_h)?;
        let Some(surface) = self.surface.as_ref() else {
            return Err("no capture surface".into());
        };

        let ok = if dst_w == src_w && dst_h == src_h {
            BitBlt(
                surface.mem_dc,
                0,
                0,
                dst_w as i32,
                dst_h as i32,
                screen_dc,
                mon.left,
                mon.top,
                SRCCOPY,
            )
        } else {
            SetStretchBltMode(surface.mem_dc, HALFTONE);
            StretchBlt(
                surface.mem_dc,
                0,
                0,
                dst_w as i32,
                dst_h as i32,
                screen_dc,
                mon.left,
                mon.top,
                src_w as i32,
                src_h as i32,
                SRCCOPY,
            )
        };
        if ok == 0 {
            self.surface = None;
            return Err("BitBlt/StretchBlt failed".into());
        }

        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = dst_w as i32;
        info.bmiHeader.biHeight = -(dst_h as i32);
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB;

        let needed = (dst_w as usize) * (dst_h as usize) * 4;
        self.pixels.resize(needed, 0);
        let got = GetDIBits(
            surface.mem_dc,
            surface.bmp,
            0,
            dst_h,
            self.pixels.as_mut_ptr() as *mut _,
            &mut info,
            DIB_RGB_COLORS,
        );
        if got == 0 {
            return Err("GetDIBits failed".into());
        }

        bgra_to_rgba_in_place(&mut self.pixels);
        Ok(Shot {
            width: dst_w,
            height: dst_h,
            status: shot_status(src_w, src_h, dst_w, dst_h),
        })
    }

    unsafe fn ensure_surface(
        &mut self,
        screen_dc: windows_sys::Win32::Graphics::Gdi::HDC,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        use windows_sys::Win32::Graphics::Gdi::{
            CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, SelectObject,
        };

        if self
            .surface
            .as_ref()
            .is_some_and(|s| s.width == width && s.height == height)
        {
            return Ok(());
        }
        self.surface = None;

        let mem_dc = CreateCompatibleDC(screen_dc);
        if mem_dc.is_null() {
            return Err("CreateCompatibleDC failed".into());
        }

        let bmp = CreateCompatibleBitmap(screen_dc, width as i32, height as i32);
        if bmp.is_null() {
            DeleteDC(mem_dc);
            return Err("CreateCompatibleBitmap failed".into());
        }
        let old = SelectObject(mem_dc, bmp as _);
        self.surface = Some(Surface {
            mem_dc,
            bmp,
            old,
            width,
            height,
        });
        Ok(())
    }
}

#[cfg(not(windows))]
#[derive(Default)]
struct Capturer {
    pixels: Vec<u8>,
}

#[cfg(not(windows))]
impl Capturer {
    fn pixels_mut(&mut self) -> &mut Vec<u8> {
        &mut self.pixels
    }

    fn capture(&mut self, _mon: &MonInfo, _max_w: u32) -> Result<Shot, String> {
        Err("Nav mirror is Windows-only".into())
    }
}

pub fn bgra_to_rgba_in_place(buf: &mut [u8]) {
    for px in buf.chunks_exact_mut(4) {
        px.swap(0, 2);
        px[3] = 255;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(index: usize) -> MonInfo {
        MonInfo {
            index,
            left: index as i32 * 100,
            top: 0,
            width: 100,
            height: 100,
            primary: index == 0,
            device: format!("display-{index}"),
            label: format!("Display {index}"),
        }
    }

    fn peek(nav: &NavMirror, generation: u64) -> Option<(u64, String)> {
        nav.with_new_frame(generation, |f| (f.generation, f.status.clone()))
    }

    #[test]
    fn bgra_conversion_is_rgba_and_opaque() {
        let mut px = [3, 2, 1, 9, 30, 20, 10, 0];
        bgra_to_rgba_in_place(&mut px);
        assert_eq!(px, [1, 2, 3, 255, 10, 20, 30, 255]);
    }

    #[test]
    fn a_partial_pixel_is_left_alone_rather_than_panicking() {
        let mut px = [3, 2, 1];
        bgra_to_rgba_in_place(&mut px);
        assert_eq!(px, [3, 2, 1]);
    }

    #[test]
    fn frame_generation_only_yields_new_frames() {
        let nav = NavMirror::new();
        let (generation, _) = peek(&nav, u64::MAX).expect("initial frame");
        assert!(peek(&nav, generation).is_none());
        nav.set_status("changed");
        let (next, status) = peek(&nav, generation).expect("changed frame");
        assert_eq!(status, "changed");
        assert_ne!(next, generation);
    }

    #[test]
    fn same_monitor_is_rejected() {
        let nav = NavMirror::new();
        let mon = monitor(0);
        assert!(nav.start(mon.clone(), &mon).is_err());
    }

    #[test]
    fn start_retarget_stop_updates_state() {
        let nav = NavMirror::new();
        let pad = monitor(0);
        let nav_mon = monitor(1);
        let other = monitor(2);

        assert!(nav.start(nav_mon.clone(), &pad).is_ok());
        {
            let state = nav.shared.state.lock().unwrap();
            assert!(state.running);
            assert_eq!(state.target.as_ref().map(|m| m.index), Some(1));
            assert_eq!(state.epoch, 1);
        }

        assert!(nav.start(other, &pad).is_ok());
        {
            let state = nav.shared.state.lock().unwrap();
            assert!(state.running);
            assert_eq!(state.target.as_ref().map(|m| m.index), Some(2));
            assert_eq!(state.epoch, 2);
        }

        nav.stop();
        {
            let state = nav.shared.state.lock().unwrap();
            assert!(!state.running);
        }
        let (_, status) = peek(&nav, u64::MAX).expect("status frame");
        assert_eq!(status, "idle");
    }

    #[test]
    #[ignore = "measurement against the live desktop; not a CI assertion"]
    #[cfg(windows)]
    fn measure_capture_cost() {
        let mut mon = crate::monitor::enumerate()
            .first()
            .expect("a monitor")
            .clone();
        mon.width = 1920;
        mon.height = 1080;
        let mut cap = Capturer::default();
        for _ in 0..5 {
            let _ = cap.capture(&mon, 1920);
        }
        let mut times = Vec::new();
        for _ in 0..60 {
            let t = std::time::Instant::now();
            cap.capture(&mon, 1920).expect("capture");
            times.push(t.elapsed().as_secs_f32() * 1000.0);
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "capture {}×{} p50={:.2}ms p90={:.2}ms max={:.2}ms",
            mon.width, mon.height, times[30], times[54], times[59]
        );
    }

    #[test]
    fn worker_stops_cleanly() {
        let nav = NavMirror::new();
        nav.stop();
        drop(nav);
    }
}
