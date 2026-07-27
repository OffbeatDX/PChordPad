use crate::PChordWindow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub const WINDOW_TITLE: &str = "PChordPad";

pub const MT_SLOTS: usize = 9;

pub const KEY_COUNT: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Act {
    Down(f32, f32),
    Move(f32, f32),
    Up(f32, f32),
    Slot(usize, bool, f32, f32),
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub live: usize,
    pub peak: usize,
    pub dropped: u64,
    pub adopted: u64,
    pub revived: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct KeyGeom {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub key_w: f32,
    pub gap: f32,
}

impl KeyGeom {
    pub fn valid(&self) -> bool {
        self.w > 1.0 && self.h > 1.0 && self.key_w > 1.0
    }

    fn pitch(&self) -> f32 {
        self.key_w + self.gap
    }

    fn center(&self, i: usize) -> f32 {
        self.x + i as f32 * self.pitch() + self.key_w * 0.5
    }

    pub fn hit(&self, x: f32, y: f32, prev: Option<usize>) -> Option<usize> {
        if !self.valid() || y < self.y || y >= self.y + self.h {
            return None;
        }
        if x < self.x || x >= self.x + self.w {
            return None;
        }
        let pitch = self.pitch().max(1.0);
        let absolute = {
            let i = ((x - self.x) / pitch).floor() as i32;
            (0..KEY_COUNT as i32).contains(&i).then_some(i as usize)
        };
        if let (Some(p), Some(n)) = (prev.filter(|p| *p < KEY_COUNT), absolute) {
            if n.abs_diff(p) > 1 {
                return Some(n);
            }
            let c = self.center(p);
            if x < c - pitch * 0.5 {
                return Some(p.saturating_sub(1));
            }
            if x >= c + pitch * 0.5 {
                return Some((p + 1).min(KEY_COUNT - 1));
            }
            return Some(p);
        }
        absolute
    }
}

type KeyListener = Arc<UiKeyHandler>;

struct UiKeyHandler {
    f: Box<dyn Fn(usize, bool)>,
}

unsafe impl Send for UiKeyHandler {}
unsafe impl Sync for UiKeyHandler {}

impl UiKeyHandler {
    fn new(f: impl Fn(usize, bool) + 'static) -> Arc<Self> {
        Arc::new(UiKeyHandler { f: Box::new(f) })
    }
    fn call(&self, index: usize, down: bool) {
        (self.f)(index, down);
    }
}

pub struct Bridge {
    state: Mutex<State>,
}

struct State {
    hwnd: isize,
    win: Option<slint::Weak<PChordWindow>>,
    install_thread: Option<std::thread::ThreadId>,
    thread_mismatch_logged: bool,
    primary: Option<u32>,
    slots: [Option<u32>; MT_SLOTS],
    stats: Stats,
    #[cfg(windows)]
    last_down: Option<std::time::Instant>,
    key_geom: KeyGeom,
    keys_enabled: bool,
    contact_key: HashMap<u32, Option<usize>>,
    held: [bool; KEY_COUNT],
    on_key: Option<KeyListener>,
    pinned: Option<PinRect>,
    vetoed: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinRect {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

impl Default for State {
    fn default() -> Self {
        State {
            hwnd: 0,
            win: None,
            install_thread: None,
            thread_mismatch_logged: false,
            primary: None,
            slots: [None; MT_SLOTS],
            stats: Stats::default(),
            #[cfg(windows)]
            last_down: None,
            key_geom: KeyGeom::default(),
            keys_enabled: true,
            contact_key: HashMap::new(),
            held: [false; KEY_COUNT],
            on_key: None,
            pinned: None,
            vetoed: 0,
        }
    }
}

impl State {
    fn classify(&mut self, id: u32, down: bool, up: bool, x: f32, y: f32) -> Option<Act> {
        if down {
            self.contact_key.insert(id, None);
            if self.primary == Some(id) {
                return Some(Act::Move(x, y));
            }
            if let Some(s) = self.slots.iter().position(|s| *s == Some(id)) {
                return Some(Act::Slot(s, true, x, y));
            }
            return self.adopt(id, x, y);
        }
        if up {
            if self.primary == Some(id) {
                self.primary = None;
                self.recount();
                return Some(Act::Up(x, y));
            }
            let s = self.slots.iter().position(|s| *s == Some(id))?;
            self.slots[s] = None;
            self.recount();
            return Some(Act::Slot(s, false, x, y));
        }
        if self.primary == Some(id) {
            return Some(Act::Move(x, y));
        }
        if let Some(s) = self.slots.iter().position(|s| *s == Some(id)) {
            return Some(Act::Slot(s, true, x, y));
        }
        self.stats.adopted += 1;
        self.adopt(id, x, y)
    }

    fn adopt(&mut self, id: u32, x: f32, y: f32) -> Option<Act> {
        if self.primary.is_none() {
            self.primary = Some(id);
            self.recount();
            return Some(Act::Down(x, y));
        }
        let Some(s) = self.slots.iter().position(Option::is_none) else {
            self.stats.dropped += 1;
            return None;
        };
        self.slots[s] = Some(id);
        self.recount();
        Some(Act::Slot(s, true, x, y))
    }

    #[cfg(windows)]
    fn stamp_down(&mut self, x: f32, y: f32) {
        if self.keys_enabled && self.key_geom.hit(x, y, None).is_some() {
            self.last_down = Some(std::time::Instant::now());
        }
    }

    fn recount(&mut self) {
        self.stats.live =
            usize::from(self.primary.is_some()) + self.slots.iter().filter(|s| s.is_some()).count();
        self.stats.peak = self.stats.peak.max(self.stats.live);
    }

    fn track_key(&mut self, id: u32, x: f32, y: f32, up: bool) -> Vec<(usize, bool)> {
        if up || !self.keys_enabled {
            self.contact_key.remove(&id);
        } else {
            let known = self.contact_key.contains_key(&id);
            let prev = self.contact_key.get(&id).copied().flatten();
            let next = self.key_geom.hit(x, y, prev);
            if !known && next.is_some() {
                self.stats.revived += 1;
            }
            self.contact_key.insert(id, next);
        }
        if self.stats.live == 0 {
            self.contact_key.clear();
        }
        self.recompute_held()
    }

    fn clear_all_keys(&mut self) -> Vec<(usize, bool)> {
        self.contact_key.clear();
        self.recompute_held()
    }

    fn reset_contacts(&mut self) -> Vec<(usize, bool)> {
        self.primary = None;
        self.slots.fill(None);
        self.recount();
        self.clear_all_keys()
    }

    fn recompute_held(&mut self) -> Vec<(usize, bool)> {
        let mut next = [false; KEY_COUNT];
        if self.keys_enabled {
            for k in self.contact_key.values().flatten() {
                if *k < KEY_COUNT {
                    next[*k] = true;
                }
            }
        }
        let mut edges = Vec::new();
        for (i, &on) in next.iter().enumerate() {
            if on != self.held[i] {
                self.held[i] = on;
                edges.push((i, on));
            }
        }
        edges
    }
}

impl Bridge {
    pub fn create() -> Arc<Bridge> {
        Arc::new(Bridge {
            state: Mutex::new(State::default()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn stats(&self) -> Stats {
        self.lock().stats
    }

    pub fn set_key_listener(&self, f: impl Fn(usize, bool) + 'static) {
        self.lock().on_key = Some(UiKeyHandler::new(f));
    }

    pub fn set_key_geom(&self, geom: KeyGeom) {
        self.lock().key_geom = geom;
    }

    pub fn set_pinned(&self, rect: Option<PinRect>) {
        self.lock().pinned = rect;
    }

    pub fn vetoed_moves(&self) -> u64 {
        self.lock().vetoed
    }

    pub fn set_keys_enabled(&self, on: bool) {
        let edges = {
            let mut st = self.lock();
            st.keys_enabled = on;
            if !on {
                st.clear_all_keys()
            } else {
                Vec::new()
            }
        };
        self.emit_edges(edges);
    }

    pub fn keys_enabled(&self) -> bool {
        self.lock().keys_enabled
    }

    pub const MOUSE_CONTACT: u32 = 0xFFFF_FFFE;

    pub fn track_mouse(&self, x: f32, y: f32, up: bool) {
        let edges = {
            let mut st = self.lock();
            st.track_key(Self::MOUSE_CONTACT, x, y, up)
        };
        self.emit_edges(edges);
    }

    fn emit_edges(&self, edges: Vec<(usize, bool)>) {
        if edges.is_empty() {
            return;
        }
        let listener = self.lock().on_key.clone();
        if let Some(f) = listener {
            for (i, down) in edges {
                f.call(i, down);
            }
        }
    }

    #[cfg(windows)]
    pub fn take_latency_ms(&self) -> Option<f32> {
        const STALE: std::time::Duration = std::time::Duration::from_millis(500);
        let t = self.lock().last_down.take()?;
        let elapsed = t.elapsed();
        (elapsed < STALE).then_some(elapsed.as_secs_f32() * 1000.0)
    }
    #[cfg(not(windows))]
    pub fn take_latency_ms(&self) -> Option<f32> {
        None
    }

    #[cfg(windows)]
    fn window(&self) -> Option<PChordWindow> {
        self.lock().win.clone()?.upgrade()
    }

    #[cfg(windows)]
    fn is_mine(&self, hwnd: isize) -> bool {
        let h = self.lock().hwnd;
        h != 0 && h == hwnd
    }

    #[cfg(windows)]
    fn hold_position(
        &self,
        moving: bool,
        sizing: bool,
        x: &mut i32,
        y: &mut i32,
        cx: &mut i32,
        cy: &mut i32,
    ) -> Option<u64> {
        let mut st = self.lock();
        let p = st.pinned?;
        let drifting = (moving && (*x, *y) != (p.left, p.top))
            || (sizing && (*cx, *cy) != (p.width, p.height));
        if !drifting {
            return None;
        }
        (*x, *y, *cx, *cy) = (p.left, p.top, p.width, p.height);
        st.vetoed += 1;
        Some(st.vetoed)
    }

    #[cfg(windows)]
    fn verify_callback_thread(&self) {
        let current = std::thread::current().id();
        let should_log = {
            let mut st = self.lock();
            let mismatch = st
                .install_thread
                .is_some_and(|installed| installed != current);
            if mismatch && !st.thread_mismatch_logged {
                st.thread_mismatch_logged = true;
                true
            } else {
                false
            }
        };
        if should_log {
            log::error!("touch callback arrived off the Slint installation thread");
        }
    }

    #[cfg(windows)]
    fn reset(&self) {
        let edges = {
            let mut st = self.lock();
            st.hwnd = 0;
            st.win = None;
            st.install_thread = None;
            st.reset_contacts()
        };
        self.emit_edges(edges);
    }
}

#[cfg(not(windows))]
pub fn install(_bridge: &Arc<Bridge>, _win: slint::Weak<PChordWindow>) -> bool {
    false
}

#[cfg(windows)]
fn set_slot(win: &PChordWindow, s: usize, down: bool, x: f32, y: f32) {
    use slint::ComponentHandle;
    let mt = win.global::<crate::MultiTouch>();
    macro_rules! slot {
        ($set_x:ident, $set_y:ident, $set_a:ident) => {{
            mt.$set_x(x);
            mt.$set_y(y);
            mt.$set_a(down);
        }};
    }
    match s {
        0 => slot!(set_x0, set_y0, set_a0),
        1 => slot!(set_x1, set_y1, set_a1),
        2 => slot!(set_x2, set_y2, set_a2),
        3 => slot!(set_x3, set_y3, set_a3),
        4 => slot!(set_x4, set_y4, set_a4),
        5 => slot!(set_x5, set_y5, set_a5),
        6 => slot!(set_x6, set_y6, set_a6),
        7 => slot!(set_x7, set_y7, set_a7),
        8 => slot!(set_x8, set_y8, set_a8),
        _ => debug_assert!(false, "slot {s} is past MT_SLOTS ({MT_SLOTS})"),
    }
}

#[cfg(windows)]
fn apply(bridge: &Bridge, win: &PChordWindow, act: Act, id: u32) {
    use slint::platform::{PointerEventButton, WindowEvent};
    use slint::{ComponentHandle, LogicalPosition};

    let (x, y, up) = match act {
        Act::Down(x, y) | Act::Move(x, y) => (x, y, false),
        Act::Up(x, y) => (x, y, true),
        Act::Slot(_, down, x, y) => (x, y, !down),
    };

    let edges = {
        let mut st = bridge.lock();
        st.track_key(id, x, y, up)
    };
    bridge.emit_edges(edges);

    match act {
        Act::Down(..) => win.window().dispatch_event(WindowEvent::PointerPressed {
            position: LogicalPosition::new(x, y),
            button: PointerEventButton::Left,
        }),
        Act::Move(..) => win.window().dispatch_event(WindowEvent::PointerMoved {
            position: LogicalPosition::new(x, y),
        }),
        Act::Up(..) => {
            win.window().dispatch_event(WindowEvent::PointerReleased {
                position: LogicalPosition::new(x, y),
                button: PointerEventButton::Left,
            });
            win.window().dispatch_event(WindowEvent::PointerExited);
        }
        Act::Slot(s, down, ..) => set_slot(win, s, down, x, y),
    }
}

#[cfg(windows)]
mod win {
    use super::*;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};

    const SUBCLASS_ID: usize = 0x5043_4831;

    const WM_WINDOWPOSCHANGING: u32 = 0x0046;
    const WM_TOUCH: u32 = 0x0240;
    const WM_POINTERUPDATE: u32 = 0x0245;
    const WM_POINTERDOWN: u32 = 0x0246;
    const WM_POINTERUP: u32 = 0x0247;
    const WM_POINTERLEAVE: u32 = 0x024A;
    const WM_POINTERACTIVATE: u32 = 0x024B;
    const WM_POINTERCAPTURECHANGED: u32 = 0x024C;

    const POINTER_FLAG_INCONTACT: u32 = 0x0000_0004;
    const POINTER_FLAG_CANCELED: u32 = 0x0000_8000;

    pub fn install(bridge: &Arc<Bridge>, weak: slint::Weak<PChordWindow>) -> bool {
        let hwnd = find_window(WINDOW_TITLE);
        if hwnd.is_null() {
            return false;
        }
        {
            let mut st = bridge.lock();
            if st.hwnd == hwnd as isize {
                return true;
            }
            st.hwnd = hwnd as isize;
            st.win = Some(weak);
            st.install_thread = Some(std::thread::current().id());
            st.thread_mismatch_logged = false;
        }
        set_no_activate(hwnd);
        let installed = install_subclass(bridge, hwnd);
        if !installed {
            let mut st = bridge.lock();
            st.hwnd = 0;
            st.win = None;
            st.install_thread = None;
        }
        installed
    }

    fn set_no_activate(hwnd: HWND) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE,
        };
        unsafe {
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_NOACTIVATE as isize);
        }
    }

    fn install_subclass(bridge: &Arc<Bridge>, hwnd: HWND) -> bool {
        use windows_sys::Win32::UI::Shell::SetWindowSubclass;
        let owned = Arc::into_raw(bridge.clone()) as usize;
        let ok = unsafe { SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, owned) != 0 };
        if !ok {
            unsafe { drop(Arc::from_raw(owned as *const Bridge)) };
        }
        ok
    }

    struct FindCtx {
        want: String,
        found: HWND,
    }

    fn find_window(title: &str) -> HWND {
        use windows_sys::Win32::System::Threading::GetCurrentThreadId;
        use windows_sys::Win32::UI::WindowsAndMessaging::EnumThreadWindows;
        let mut ctx = FindCtx {
            want: title.to_string(),
            found: std::ptr::null_mut(),
        };
        unsafe {
            EnumThreadWindows(
                GetCurrentThreadId(),
                Some(enum_cb),
                &mut ctx as *mut FindCtx as isize,
            );
        }
        ctx.found
    }

    unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> i32 {
        use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowTextW;
        let ctx = &mut *(lparam as *mut FindCtx);
        let mut buf = [0u16; 96];
        let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        if String::from_utf16_lossy(&buf[..n.max(0) as usize]) == ctx.want {
            ctx.found = hwnd;
            return 0;
        }
        1
    }

    fn announce_once(route: &str) {
        static ANNOUNCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !ANNOUNCED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            log::info!("{route} intercepted; {MT_SLOTS} extra contacts routed");
        }
    }

    unsafe fn handle_pointer(bridge: &Bridge, hwnd: HWND, msg: u32, wparam: WPARAM) -> bool {
        use slint::ComponentHandle;
        use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
        use windows_sys::Win32::UI::Input::Pointer::{GetPointerInfo, POINTER_INFO};
        use windows_sys::Win32::UI::WindowsAndMessaging::{PT_PEN, PT_TOUCH};

        let id = (wparam & 0xffff) as u32;

        let releasing = matches!(msg, WM_POINTERUP | WM_POINTERLEAVE);

        let mut info: POINTER_INFO = core::mem::zeroed();
        if GetPointerInfo(id, &mut info) == 0
            || (info.pointerType != PT_TOUCH && info.pointerType != PT_PEN)
        {
            if releasing {
                release_lost(bridge, id);
                return true;
            }
            return false;
        }
        let Some(w) = bridge.window() else {
            if releasing {
                release_lost(bridge, id);
                return true;
            }
            return false;
        };
        let scale = w.window().scale_factor().max(0.01);

        let mut pt = POINT {
            x: info.ptPixelLocation.x,
            y: info.ptPixelLocation.y,
        };
        ScreenToClient(hwnd, &mut pt);
        let (x, y) = (pt.x as f32 / scale, pt.y as f32 / scale);

        announce_once("WM_POINTER");
        let flags = info.pointerFlags;
        let canceled = flags & POINTER_FLAG_CANCELED != 0;
        let down = msg == WM_POINTERDOWN && !canceled;
        let up = msg == WM_POINTERUP
            || msg == WM_POINTERLEAVE
            || canceled
            || (flags & POINTER_FLAG_INCONTACT) == 0;
        let act = {
            let mut st = bridge.lock();
            if down {
                st.stamp_down(x, y);
            }
            st.classify(id, down, up, x, y)
        };
        if let Some(act) = act {
            apply(bridge, &w, act, id);
        } else if up {
            let edges = bridge.lock().track_key(id, x, y, true);
            bridge.emit_edges(edges);
        }
        true
    }

    unsafe fn release_lost(bridge: &Bridge, id: u32) {
        let act = bridge.lock().classify(id, false, true, 0.0, 0.0);
        if let (Some(act), Some(w)) = (act, bridge.window()) {
            apply(bridge, &w, act, id);
            return;
        }
        let edges = bridge.lock().track_key(id, 0.0, 0.0, true);
        bridge.emit_edges(edges);
    }

    unsafe fn handle_touch(bridge: &Bridge, hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> bool {
        use slint::ComponentHandle;
        use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
        use windows_sys::Win32::UI::Input::Touch::{
            CloseTouchInputHandle, GetTouchInputInfo, TOUCHEVENTF_DOWN, TOUCHEVENTF_UP, TOUCHINPUT,
        };

        const MAX_CONTACTS: usize = 1 + MT_SLOTS;

        let reported = wparam & 0xffff;
        let count = reported.min(MAX_CONTACTS);
        if count == 0 {
            return false;
        }
        let handle = lparam as *mut core::ffi::c_void;
        let mut inputs: [TOUCHINPUT; MAX_CONTACTS] = core::mem::zeroed();
        if GetTouchInputInfo(
            handle,
            count as u32,
            inputs.as_mut_ptr(),
            core::mem::size_of::<TOUCHINPUT>() as i32,
        ) == 0
        {
            return false;
        }
        let Some(w) = bridge.window() else {
            return false;
        };
        let scale = w.window().scale_factor().max(0.01);
        announce_once("WM_TOUCH");

        let mut acts: Vec<(u32, Act)> = Vec::with_capacity(count);
        let mut orphan_edges: Vec<(usize, bool)> = Vec::new();
        {
            let mut st = bridge.lock();
            st.stats.dropped += (reported - count) as u64;
            for ti in &inputs[..count] {
                let mut pt = POINT {
                    x: ti.x / 100,
                    y: ti.y / 100,
                };
                ScreenToClient(hwnd, &mut pt);
                let (x, y) = (pt.x as f32 / scale, pt.y as f32 / scale);
                let down = ti.dwFlags & TOUCHEVENTF_DOWN != 0;
                let up = ti.dwFlags & TOUCHEVENTF_UP != 0;
                if down {
                    st.stamp_down(x, y);
                }
                if let Some(act) = st.classify(ti.dwID, down, up, x, y) {
                    acts.push((ti.dwID, act));
                } else if up {
                    orphan_edges.extend(st.track_key(ti.dwID, x, y, true));
                }
            }
        }
        CloseTouchInputHandle(handle);

        bridge.emit_edges(orphan_edges);
        for (id, act) in acts {
            apply(bridge, &w, act, id);
        }
        true
    }

    unsafe extern "system" fn subclass_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        id: usize,
        refdata: usize,
    ) -> LRESULT {
        use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            MA_NOACTIVATE, WM_MOUSEACTIVATE, WM_NCDESTROY,
        };

        if refdata == 0 {
            return DefSubclassProc(hwnd, msg, wparam, lparam);
        }
        if msg == WM_NCDESTROY {
            let bridge = &*(refdata as *const Bridge);
            bridge.verify_callback_thread();
            bridge.reset();
            RemoveWindowSubclass(hwnd, Some(subclass_proc), id);
            let res = DefSubclassProc(hwnd, msg, wparam, lparam);
            drop(Arc::from_raw(refdata as *const Bridge));
            return res;
        }

        let bridge = &*(refdata as *const Bridge);
        bridge.verify_callback_thread();

        if (msg == WM_POINTERACTIVATE || msg == WM_MOUSEACTIVATE) && bridge.is_mine(hwnd as isize) {
            return MA_NOACTIVATE as LRESULT;
        }

        if msg == WM_WINDOWPOSCHANGING && bridge.is_mine(hwnd as isize) {
            use windows_sys::Win32::UI::WindowsAndMessaging::{SWP_NOMOVE, SWP_NOSIZE, WINDOWPOS};
            let wp = &mut *(lparam as *mut WINDOWPOS);
            let moving = wp.flags & SWP_NOMOVE == 0;
            let sizing = wp.flags & SWP_NOSIZE == 0;
            if moving || sizing {
                let attempted = (wp.x, wp.y, wp.cx, wp.cy);
                let vetoed = bridge
                    .hold_position(moving, sizing, &mut wp.x, &mut wp.y, &mut wp.cx, &mut wp.cy);
                if let Some(n) = vetoed {
                    wp.flags &= !(SWP_NOMOVE | SWP_NOSIZE);
                    if n == 1 || n % 25 == 0 {
                        log::warn!(
                            "refused pad window move/resize to {},{} {}×{} (#{n}) — \
                             something is trying to maximize or reposition the pad",
                            attempted.0,
                            attempted.1,
                            attempted.2,
                            attempted.3
                        );
                    }
                }
            }
            return DefSubclassProc(hwnd, msg, wparam, lparam);
        }

        if msg == WM_POINTERCAPTURECHANGED && bridge.is_mine(hwnd as isize) {
            release_lost(bridge, (wparam & 0xffff) as u32);
            return 0;
        }

        let touch = matches!(
            msg,
            WM_POINTERDOWN | WM_POINTERUPDATE | WM_POINTERUP | WM_POINTERLEAVE | WM_TOUCH
        );
        if touch && bridge.is_mine(hwnd as isize) {
            let handled = if msg == WM_TOUCH {
                handle_touch(bridge, hwnd, wparam, lparam)
            } else {
                handle_pointer(bridge, hwnd, msg, wparam)
            };
            if handled {
                return 0;
            }
        }

        DefSubclassProc(hwnd, msg, wparam, lparam)
    }
}

#[cfg(windows)]
pub use win::install;

#[cfg(test)]
mod tests {
    use super::*;

    fn down(st: &mut State, id: u32, x: f32, y: f32) -> Option<Act> {
        st.classify(id, true, false, x, y)
    }
    fn moved(st: &mut State, id: u32, x: f32, y: f32) -> Option<Act> {
        st.classify(id, false, false, x, y)
    }
    fn up(st: &mut State, id: u32, x: f32, y: f32) -> Option<Act> {
        st.classify(id, false, true, x, y)
    }

    fn geom() -> KeyGeom {
        KeyGeom {
            x: 0.0,
            y: 100.0,
            w: 1200.0,
            h: 200.0,
            key_w: 90.0,
            gap: 10.0,
        }
    }

    #[test]
    fn first_contact_drives_the_ordinary_pointer_path() {
        let mut st = State::default();
        assert_eq!(down(&mut st, 7, 10.0, 20.0), Some(Act::Down(10.0, 20.0)));
        assert_eq!(moved(&mut st, 7, 11.0, 20.0), Some(Act::Move(11.0, 20.0)));
        assert_eq!(up(&mut st, 7, 11.0, 20.0), Some(Act::Up(11.0, 20.0)));
        assert_eq!(st.primary, None);
    }

    #[test]
    fn further_contacts_fill_slots_and_track() {
        let mut st = State::default();
        down(&mut st, 1, 0.0, 0.0);
        assert_eq!(
            down(&mut st, 2, 5.0, 6.0),
            Some(Act::Slot(0, true, 5.0, 6.0))
        );
        assert_eq!(
            down(&mut st, 3, 7.0, 8.0),
            Some(Act::Slot(1, true, 7.0, 8.0))
        );
        assert_eq!(
            moved(&mut st, 2, 5.0, 9.0),
            Some(Act::Slot(0, true, 5.0, 9.0))
        );
        assert_eq!(
            up(&mut st, 2, 5.0, 9.0),
            Some(Act::Slot(0, false, 5.0, 9.0))
        );
        assert_eq!(st.slots[0], None);
        assert_eq!(st.slots[1], Some(3));
        assert_eq!(
            down(&mut st, 4, 1.0, 1.0),
            Some(Act::Slot(0, true, 1.0, 1.0))
        );
    }

    #[test]
    fn a_lifted_primary_does_not_promote_an_extra() {
        let mut st = State::default();
        down(&mut st, 1, 0.0, 0.0);
        down(&mut st, 2, 5.0, 5.0);
        up(&mut st, 1, 0.0, 0.0);
        assert_eq!(st.primary, None);
        assert_eq!(st.slots[0], Some(2));
        assert_eq!(
            moved(&mut st, 2, 6.0, 5.0),
            Some(Act::Slot(0, true, 6.0, 5.0))
        );
    }

    #[test]
    fn a_full_hand_of_contacts_is_accepted() {
        let mut st = State::default();
        for id in 1..=(MT_SLOTS as u32 + 1) {
            assert!(down(&mut st, id, id as f32, 0.0).is_some(), "contact {id}");
        }
        assert_eq!(st.stats.live, MT_SLOTS + 1);
        assert_eq!(st.stats.peak, MT_SLOTS + 1);
        assert_eq!(st.stats.dropped, 0);
        for (s, held) in st.slots.iter().enumerate() {
            assert_eq!(*held, Some(s as u32 + 2), "slot {s}");
        }
        assert_eq!(st.primary, Some(1));
    }

    #[test]
    fn contacts_beyond_the_slots_are_dropped_not_stolen_and_counted() {
        let mut st = State::default();
        for id in 1..=(MT_SLOTS as u32 + 1) {
            down(&mut st, id, 0.0, 0.0);
        }
        let overflow = MT_SLOTS as u32 + 2;
        assert_eq!(down(&mut st, overflow, 3.0, 3.0), None);
        assert_eq!(st.stats.dropped, 1);
        assert_eq!(moved(&mut st, overflow, 4.0, 3.0), None);
        assert_eq!(up(&mut st, overflow, 4.0, 3.0), None);
        assert_eq!(st.slots[0], Some(2));
        assert_eq!(st.primary, Some(1));
    }

    #[test]
    fn peak_is_a_high_water_mark_and_live_falls_back() {
        let mut st = State::default();
        down(&mut st, 1, 0.0, 0.0);
        down(&mut st, 2, 0.0, 0.0);
        down(&mut st, 3, 0.0, 0.0);
        assert_eq!(st.stats.live, 3);
        up(&mut st, 2, 0.0, 0.0);
        up(&mut st, 3, 0.0, 0.0);
        assert_eq!(st.stats.live, 1);
        assert_eq!(st.stats.peak, 3, "peak must not fall with the fingers");
    }

    #[test]
    fn a_slot_freed_and_refilled_in_one_frame_keeps_the_new_contact() {
        let mut st = State::default();
        down(&mut st, 1, 0.0, 0.0);
        down(&mut st, 2, 5.0, 5.0);
        assert_eq!(
            up(&mut st, 2, 5.0, 5.0),
            Some(Act::Slot(0, false, 5.0, 5.0))
        );
        assert_eq!(
            down(&mut st, 9, 80.0, 90.0),
            Some(Act::Slot(0, true, 80.0, 90.0))
        );
        assert_eq!(st.slots[0], Some(9));
    }

    #[test]
    fn key_hit_assigns_columns_and_hysteresis_holds_in_gutter() {
        let g = geom();
        assert_eq!(g.hit(45.0, 150.0, None), Some(0));
        assert_eq!(g.hit(145.0, 150.0, None), Some(1));
        assert_eq!(g.hit(90.0, 150.0, Some(0)), Some(0));
        assert_eq!(g.hit(96.0, 150.0, Some(0)), Some(1));
    }

    #[test]
    fn key_hit_snaps_across_multi_key_jumps() {
        let g = geom();
        assert_eq!(g.hit(1045.0, 150.0, Some(1)), Some(10));
        assert_eq!(g.hit(245.0, 150.0, Some(1)), Some(2));
    }

    #[test]
    fn reused_pointer_id_on_down_does_not_keep_old_key() {
        let mut st = State {
            key_geom: geom(),
            ..State::default()
        };
        down(&mut st, 1, 0.0, 0.0);
        down(&mut st, 2, 0.0, 0.0);
        assert!(down(&mut st, 99, 145.0, 150.0).is_some());
        st.stats.live = 3;
        assert_eq!(st.track_key(99, 145.0, 150.0, false), vec![(1, true)]);
        assert!(down(&mut st, 99, 1045.0, 150.0).is_some());
        let edges = st.track_key(99, 1045.0, 150.0, false);
        assert!(edges.contains(&(1, false)), "{edges:?}");
        assert!(edges.contains(&(10, true)), "{edges:?}");
        assert!(!st.held[1]);
        assert!(st.held[10]);
    }

    #[test]
    fn track_key_emits_edges_and_clears_on_up() {
        let mut st = State {
            key_geom: geom(),
            ..State::default()
        };
        down(&mut st, 1, 45.0, 150.0);
        let edges = st.track_key(1, 45.0, 150.0, false);
        assert_eq!(edges, vec![(0, true)]);
        assert!(st.held[0]);
        st.stats.live = 0;
        let edges = st.track_key(1, 45.0, 150.0, true);
        assert_eq!(edges, vec![(0, false)]);
    }

    #[test]
    fn a_contact_that_lost_its_down_is_adopted_on_the_next_move() {
        let mut st = State {
            key_geom: geom(),
            ..State::default()
        };
        assert_eq!(
            moved(&mut st, 42, 45.0, 150.0),
            Some(Act::Down(45.0, 150.0))
        );
        assert_eq!(st.primary, Some(42));
        assert_eq!(st.stats.adopted, 1);
        assert_eq!(st.track_key(42, 45.0, 150.0, false), vec![(0, true)]);
    }

    #[test]
    fn adoption_uses_a_slot_when_the_primary_is_taken() {
        let mut st = State::default();
        down(&mut st, 1, 0.0, 0.0);
        assert_eq!(
            moved(&mut st, 77, 5.0, 6.0),
            Some(Act::Slot(0, true, 5.0, 6.0))
        );
        assert_eq!(st.slots[0], Some(77));
        assert_eq!(st.stats.adopted, 1);
        assert_eq!(st.stats.live, 2);
    }

    #[test]
    fn a_genuine_new_contact_is_not_counted_as_adopted_or_revived() {
        let mut st = State {
            key_geom: geom(),
            ..State::default()
        };
        down(&mut st, 1, 45.0, 150.0);
        st.track_key(1, 45.0, 150.0, false);
        assert_eq!(st.stats.adopted, 0);
        assert_eq!(st.stats.revived, 0, "a first press is not a revival");
    }

    #[test]
    fn a_contact_whose_key_mapping_was_wiped_is_counted_as_revived() {
        let mut st = State {
            key_geom: geom(),
            ..State::default()
        };
        down(&mut st, 1, 45.0, 150.0);
        st.track_key(1, 45.0, 150.0, false);
        st.clear_all_keys();
        assert_eq!(st.track_key(1, 45.0, 150.0, false), vec![(0, true)]);
        assert_eq!(st.stats.revived, 1);
    }

    #[cfg(windows)]
    #[test]
    fn a_pinned_window_refuses_the_maximize_geometry() {
        let b = Bridge::create();
        let pin = PinRect {
            left: 0,
            top: 1440,
            width: 1920,
            height: 1080,
        };
        b.set_pinned(Some(pin));

        let (mut x, mut y, mut cx, mut cy) = (-8, 1432, 1936, 1096);
        assert_eq!(
            b.hold_position(true, true, &mut x, &mut y, &mut cx, &mut cy),
            Some(1)
        );
        assert_eq!((x, y, cx, cy), (0, 1440, 1920, 1080));
        assert_eq!(b.vetoed_moves(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn holding_position_ignores_fields_windows_will_not_read() {
        let b = Bridge::create();
        b.set_pinned(Some(PinRect {
            left: 0,
            top: 1440,
            width: 1920,
            height: 1080,
        }));

        let (mut x, mut y, mut cx, mut cy) = (0, 1440, 12345, 6789);
        assert_eq!(
            b.hold_position(true, false, &mut x, &mut y, &mut cx, &mut cy),
            None
        );
        assert_eq!(b.vetoed_moves(), 0);
    }

    #[cfg(windows)]
    #[test]
    fn windowed_mode_leaves_the_window_alone() {
        let b = Bridge::create();
        b.set_pinned(None);
        let (mut x, mut y, mut cx, mut cy) = (100, 200, 1280, 720);
        assert_eq!(
            b.hold_position(true, true, &mut x, &mut y, &mut cx, &mut cy),
            None
        );
        assert_eq!((x, y, cx, cy), (100, 200, 1280, 720));
    }

    #[test]
    fn reset_contacts_releases_everything() {
        let mut st = State {
            key_geom: geom(),
            ..State::default()
        };
        down(&mut st, 1, 45.0, 150.0);
        down(&mut st, 2, 145.0, 150.0);
        st.track_key(1, 45.0, 150.0, false);
        st.track_key(2, 145.0, 150.0, false);

        let edges = st.reset_contacts();

        assert_eq!(st.stats.live, 0);
        assert!(st.primary.is_none());
        assert!(st.slots.iter().all(Option::is_none));
        assert!(edges.contains(&(0, false)));
        assert!(edges.contains(&(1, false)));
    }
}
