use crate::monitor::MonInfo;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
    pub status: String,
}

pub struct NavMirror {
    running: AtomicBool,
    frame: Mutex<Frame>,
    target: Mutex<Option<MonInfo>>,
    max_w: u32,
}

impl NavMirror {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            running: AtomicBool::new(false),
            frame: Mutex::new(Frame {
                status: "idle".into(),
                ..Frame::default()
            }),
            target: Mutex::new(None),
            max_w: 1920,
        })
    }

    pub fn start(&self, target: MonInfo, pad: &MonInfo) -> Result<(), String> {
        if target.same_screen(pad) {
            return Err("Nav monitor must be different from Pad monitor".into());
        }
        *self.target.lock().unwrap_or_else(|e| e.into_inner()) = Some(target);
        self.running.store(true, Ordering::Relaxed);
        self.set_status("capturing…");
        Ok(())
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
        inject_mouse_up(self);
        self.set_status("idle");
    }

    fn set_status(&self, s: &str) {
        if let Ok(mut f) = self.frame.lock() {
            f.status = s.into();
        }
    }

    pub fn capture_tick(&self) {
        if !self.running.load(Ordering::Relaxed) {
            return;
        }
        let target = self
            .target
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(mon) = target else { return };
        match capture_monitor(&mon, self.max_w) {
            Ok(frame) => {
                *self.frame.lock().unwrap_or_else(|e| e.into_inner()) = frame;
            }
            Err(e) => self.set_status(&format!("capture failed: {e}")),
        }
    }

    pub fn take_frame(&self) -> Frame {
        self.frame.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn pointer(&self, nx: f32, ny: f32, kind: i32) {
        if !self.running.load(Ordering::Relaxed) {
            return;
        }
        let mon = match self
            .target
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            Some(m) => m,
            None => return,
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
        .target
        .lock()
        .unwrap_or_else(|e| e.into_inner())
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

#[cfg(windows)]
fn capture_monitor(mon: &MonInfo, max_w: u32) -> Result<Frame, String> {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, ReleaseDC, SelectObject, SetStretchBltMode, StretchBlt, BITMAPINFO,
        BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HALFTONE, HBITMAP, HDC, HGDIOBJ, SRCCOPY,
    };

    let src_w = mon.width.max(1) as u32;
    let src_h = mon.height.max(1) as u32;
    let scale = if src_w > max_w {
        max_w as f32 / src_w as f32
    } else {
        1.0
    };
    let dst_w = ((src_w as f32 * scale).round() as u32).max(1);
    let dst_h = ((src_h as f32 * scale).round() as u32).max(1);

    unsafe {
        let screen_dc: HDC = GetDC(0 as HWND);
        if screen_dc.is_null() {
            return Err("GetDC failed".into());
        }
        let mem_dc = CreateCompatibleDC(screen_dc);
        if mem_dc.is_null() {
            ReleaseDC(0 as HWND, screen_dc);
            return Err("CreateCompatibleDC failed".into());
        }
        let bmp: HBITMAP = CreateCompatibleBitmap(screen_dc, dst_w as i32, dst_h as i32);
        if bmp.is_null() {
            DeleteDC(mem_dc);
            ReleaseDC(0 as HWND, screen_dc);
            return Err("CreateCompatibleBitmap failed".into());
        }
        let old: HGDIOBJ = SelectObject(mem_dc, bmp as HGDIOBJ);

        let ok = if dst_w == src_w && dst_h == src_h {
            BitBlt(
                mem_dc,
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
            SetStretchBltMode(mem_dc, HALFTONE);
            StretchBlt(
                mem_dc,
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
            SelectObject(mem_dc, old);
            DeleteObject(bmp as HGDIOBJ);
            DeleteDC(mem_dc);
            ReleaseDC(0 as HWND, screen_dc);
            return Err("BitBlt/StretchBlt failed".into());
        }

        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = dst_w as i32;
        info.bmiHeader.biHeight = -(dst_h as i32);
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB;

        let mut bgra = vec![0u8; (dst_w * dst_h * 4) as usize];
        let got = GetDIBits(
            mem_dc,
            bmp,
            0,
            dst_h,
            bgra.as_mut_ptr() as *mut _,
            &mut info,
            DIB_RGB_COLORS,
        );

        SelectObject(mem_dc, old);
        DeleteObject(bmp as HGDIOBJ);
        DeleteDC(mem_dc);
        ReleaseDC(0 as HWND, screen_dc);

        if got == 0 {
            return Err("GetDIBits failed".into());
        }

        Ok(Frame {
            width: dst_w,
            height: dst_h,
            bgra,
            status: if dst_w == src_w {
                format!("NAV  {src_w}×{src_h}  ·  touch = main screen")
            } else {
                format!("NAV  {src_w}×{src_h}→{dst_w}×{dst_h}  ·  touch = main screen")
            },
        })
    }
}

#[cfg(not(windows))]
fn capture_monitor(_mon: &MonInfo, _max_w: u32) -> Result<Frame, String> {
    Err("Nav mirror is Windows-only".into())
}

pub fn bgra_to_rgba(bgra: &[u8]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(bgra.len());
    for chunk in bgra.chunks_exact(4) {
        rgba.push(chunk[2]);
        rgba.push(chunk[1]);
        rgba.push(chunk[0]);
        rgba.push(255);
    }
    rgba
}
