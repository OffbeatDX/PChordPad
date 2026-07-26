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
}

impl Default for State {
    fn default() -> Self {
        State {
            hwnd: 0,
            win: None,
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
        }
    }
}

impl State {
    fn classify(&mut self, id: u32, down: bool, up: bool, x: f32, y: f32) -> Option<Act> {
        if down {
            self.contact_key.remove(&id);
            if self.primary == Some(id) {
                return Some(Act::Move(x, y));
            }
            if let Some(s) = self.slots.iter().position(|s| *s == Some(id)) {
                return Some(Act::Slot(s, true, x, y));
            }
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
            return Some(Act::Slot(s, true, x, y));
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
        let s = self.slots.iter().position(|s| *s == Some(id))?;
        Some(Act::Slot(s, true, x, y))
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
            let prev = self.contact_key.get(&id).copied().flatten();
            let next = self.key_geom.hit(x, y, prev);
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
        self.lock()
            .last_down
            .take()
            .map(|t| t.elapsed().as_secs_f32() * 1000.0)
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
            st.hwnd = hwnd as isize;
            st.win = Some(weak);
        }
        set_no_activate(hwnd);
        install_subclass(bridge, hwnd)
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

        if matches!(msg, WM_POINTERLEAVE | WM_POINTERUP) {
            let mut info: POINTER_INFO = core::mem::zeroed();
            if GetPointerInfo(id, &mut info) == 0 {
                release_lost(bridge, id);
                return true;
            }
        }

        let mut info: POINTER_INFO = core::mem::zeroed();
        if GetPointerInfo(id, &mut info) == 0 {
            return false;
        }
        if info.pointerType != PT_TOUCH && info.pointerType != PT_PEN {
            return false;
        }
        let Some(w) = bridge.window() else {
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
                st.last_down = Some(std::time::Instant::now());
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
                    st.last_down = Some(std::time::Instant::now());
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
            RemoveWindowSubclass(hwnd, Some(subclass_proc), id);
            let res = DefSubclassProc(hwnd, msg, wparam, lparam);
            drop(Arc::from_raw(refdata as *const Bridge));
            return res;
        }

        let bridge = &*(refdata as *const Bridge);

        if (msg == WM_POINTERACTIVATE || msg == WM_MOUSEACTIVATE) && bridge.is_mine(hwnd as isize) {
            return MA_NOACTIVATE as LRESULT;
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
}
