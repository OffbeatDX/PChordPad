use crate::faders::{FaderCfg, Faders, Zone, FADER_COUNT};
use crate::PChordWindow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub const WINDOW_TITLE: &str = "PChordPad";

pub const KEY_COUNT: usize = 12;

pub const MAX_CONTACTS: usize = 16;

pub const MOUSE_CONTACT: u32 = 0xFFFF_FFFE;

pub const CONTACT_SILENT: std::time::Duration = std::time::Duration::from_millis(400);

pub const TOUCH_SILENT: std::time::Duration = std::time::Duration::from_millis(1500);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CursorAction {
    None,
    Press(f32, f32),
    Move(f32, f32),
    Release(f32, f32),
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub live: usize,
    pub peak: usize,
    pub dropped: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
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

    fn in_band(&self, x: f32, y: f32) -> bool {
        self.valid() && x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
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

type AnalogListener = Arc<UiAnalogHandler>;

struct UiAnalogHandler {
    f: Box<dyn Fn(f32, f32)>,
}

unsafe impl Send for UiAnalogHandler {}
unsafe impl Sync for UiAnalogHandler {}

impl UiAnalogHandler {
    fn new(f: impl Fn(f32, f32) + 'static) -> Arc<Self> {
        Arc::new(UiAnalogHandler { f: Box::new(f) })
    }
    fn call(&self, l: f32, r: f32) {
        (self.f)(l, r);
    }
}

#[derive(Debug, Clone, Copy)]
struct Contact {
    x: f32,
    y: f32,
    seen: Instant,
    key: Option<usize>,
}

struct Ingest {
    key_edges: Vec<(usize, bool)>,
    analog: Option<[f32; FADER_COUNT]>,
    cursor: CursorAction,
}

pub struct Bridge {
    state: Mutex<State>,
}

struct State {
    hwnd: isize,
    win: Option<slint::Weak<PChordWindow>>,
    install_thread: Option<std::thread::ThreadId>,
    thread_mismatch_logged: bool,
    contacts: HashMap<u32, Contact>,
    cursor: Option<u32>,
    stats: Stats,
    key_geom: KeyGeom,
    keys_enabled: bool,
    held: [bool; KEY_COUNT],
    on_key: Option<KeyListener>,
    on_analog: Option<AnalogListener>,
    faders: Faders,
    last_analog: [f32; FADER_COUNT],
    pinned: Option<PinRect>,
    vetoed: u64,
    minimized: bool,
    route: Route,
    expired: u64,
    displays_changed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Route {
    #[default]
    Unknown,
    Pointer,
    Touch,
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
            contacts: HashMap::new(),
            cursor: None,
            stats: Stats::default(),
            key_geom: KeyGeom::default(),
            keys_enabled: true,
            held: [false; KEY_COUNT],
            on_key: None,
            on_analog: None,
            faders: Faders::new(),
            last_analog: [0.0; FADER_COUNT],
            pinned: None,
            vetoed: 0,
            minimized: false,
            route: Route::Unknown,
            expired: 0,
            displays_changed: false,
        }
    }
}

impl State {
    fn ingest(&mut self, id: u32, x: f32, y: f32, alive: bool) -> Ingest {
        if alive {
            self.ingest_alive(id, x, y)
        } else {
            self.ingest_gone(id, x, y)
        }
    }

    fn ingest_alive(&mut self, id: u32, x: f32, y: f32) -> Ingest {
        let fresh = !self.contacts.contains_key(&id);
        if fresh && self.contacts.len() >= MAX_CONTACTS {
            self.stats.dropped += 1;
            return Ingest {
                key_edges: Vec::new(),
                analog: None,
                cursor: CursorAction::None,
            };
        }

        let analog_changed = self.keys_enabled && self.faders.offer(id, x, y);
        let owns_fader = self.faders.owns(id);

        let prev_key = self.contacts.get(&id).and_then(|c| c.key);
        let key = if self.keys_enabled && !owns_fader {
            self.key_geom.hit(x, y, prev_key)
        } else {
            None
        };
        self.contacts.insert(
            id,
            Contact {
                x,
                y,
                seen: Instant::now(),
                key,
            },
        );
        self.recount();

        let key_edges = self.recompute_held();
        let analog = analog_changed.then(|| self.faders.values());
        let cursor = self.cursor_action(id, x, y, true, fresh);
        Ingest {
            key_edges,
            analog,
            cursor,
        }
    }

    fn ingest_gone(&mut self, id: u32, x: f32, y: f32) -> Ingest {
        let last = self.contacts.remove(&id);
        self.faders.release(id);
        self.recount();
        let key_edges = self.recompute_held();
        let (rx, ry) = last.map(|c| (c.x, c.y)).unwrap_or((x, y));
        let cursor = self.cursor_action(id, rx, ry, false, false);
        Ingest {
            key_edges,
            analog: None,
            cursor,
        }
    }

    fn cursor_action(&mut self, id: u32, x: f32, y: f32, alive: bool, fresh: bool) -> CursorAction {
        if id == MOUSE_CONTACT {
            return CursorAction::None;
        }
        if alive {
            if self.cursor == Some(id) {
                return CursorAction::Move(x, y);
            }
            if fresh && self.cursor.is_none() && self.is_chrome_point(x, y) {
                self.cursor = Some(id);
                return CursorAction::Press(x, y);
            }
            CursorAction::None
        } else if self.cursor == Some(id) {
            self.cursor = None;
            CursorAction::Release(x, y)
        } else {
            CursorAction::None
        }
    }

    fn is_chrome_point(&self, x: f32, y: f32) -> bool {
        if !self.keys_enabled {
            return true;
        }
        !(self.faders.zone_hit(x, y) || self.key_geom.in_band(x, y))
    }

    fn recount(&mut self) {
        self.stats.live = self.contacts.len();
        self.stats.peak = self.stats.peak.max(self.stats.live);
    }

    fn recompute_held(&mut self) -> Vec<(usize, bool)> {
        let mut next = [false; KEY_COUNT];
        if self.keys_enabled {
            for c in self.contacts.values() {
                if let Some(k) = c.key {
                    if k < KEY_COUNT {
                        next[k] = true;
                    }
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

    fn reset_contacts(&mut self) -> Vec<(usize, bool)> {
        self.contacts.clear();
        self.faders.release_all();
        self.cursor = None;
        self.recount();
        self.recompute_held()
    }

    fn quiet_contacts(&self, silent: std::time::Duration) -> Vec<u32> {
        self.contacts
            .iter()
            .filter(|(_, c)| c.seen.elapsed() >= silent)
            .map(|(id, _)| *id)
            .collect()
    }

    fn restamp(&mut self, id: u32) {
        if let Some(c) = self.contacts.get_mut(&id) {
            c.seen = Instant::now();
        }
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

    pub fn set_analog_listener(&self, f: impl Fn(f32, f32) + 'static) {
        self.lock().on_analog = Some(UiAnalogHandler::new(f));
    }

    pub fn set_key_geom(&self, geom: KeyGeom) {
        self.lock().key_geom = geom;
    }

    pub fn set_fader_geom(&self, zones: [Zone; FADER_COUNT]) {
        self.lock().faders.set_zones(zones);
    }

    pub fn set_fader_cfg(&self, cfg: FaderCfg) {
        self.lock().faders.set_cfg(cfg);
    }

    pub fn fader_snapshot(&self) -> ([f32; FADER_COUNT], [bool; FADER_COUNT], [i32; FADER_COUNT]) {
        let st = self.lock();
        (st.faders.values(), st.faders.grabbed(), st.faders.dirs())
    }

    pub fn tick_faders(&self) {
        let vals = {
            let mut st = self.lock();
            st.faders.decay_step().then(|| st.faders.values())
        };
        if let Some(v) = vals {
            self.emit_analog(v);
        }
    }

    pub fn set_pinned(&self, rect: Option<PinRect>) {
        self.lock().pinned = rect;
    }

    pub fn take_display_change(&self) -> bool {
        std::mem::take(&mut self.lock().displays_changed)
    }

    pub fn vetoed_moves(&self) -> u64 {
        self.lock().vetoed
    }

    #[cfg(windows)]
    pub fn restore_if_minimized(&self) -> bool {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            IsIconic, ShowWindow, SW_SHOWNOACTIVATE,
        };
        let hwnd = self.lock().hwnd;
        if hwnd == 0 {
            return false;
        }
        let iconic = unsafe { IsIconic(hwnd as _) != 0 };
        let (announce, edges) = {
            let mut st = self.lock();
            let first = iconic && !st.minimized;
            st.minimized = iconic;
            let edges = if first {
                st.reset_contacts()
            } else {
                Vec::new()
            };
            (first, edges)
        };
        self.emit_edges(edges);
        if !iconic {
            return false;
        }
        if announce {
            log::warn!("pad window was minimized — restoring it without stealing focus");
        }
        unsafe { ShowWindow(hwnd as _, SW_SHOWNOACTIVATE) };
        true
    }

    #[cfg(not(windows))]
    pub fn restore_if_minimized(&self) -> bool {
        false
    }

    #[cfg(windows)]
    pub fn expire_lost_contacts(&self) {
        let (route, quiet) = {
            let st = self.lock();
            if st.contacts.is_empty() {
                return;
            }
            let silent = match st.route {
                Route::Pointer => CONTACT_SILENT,
                Route::Touch => TOUCH_SILENT,
                Route::Unknown => return,
            };
            (st.route, st.quiet_contacts(silent))
        };
        for id in quiet {
            if route == Route::Pointer && win::pointer_still_down(id) {
                self.lock().restamp(id);
                continue;
            }
            let n = {
                let mut st = self.lock();
                st.expired += 1;
                st.expired
            };
            log::warn!(
                "contact {id} vanished without an UP (#{n}, {route:?}) — releasing it so its key cannot stick"
            );
            self.release_contact(id, route);
        }
    }

    #[cfg(not(windows))]
    pub fn expire_lost_contacts(&self) {}

    pub fn expired_contacts(&self) -> u64 {
        self.lock().expired
    }

    pub fn set_keys_enabled(&self, on: bool) {
        let edges = {
            let mut st = self.lock();
            let was = st.keys_enabled;
            st.keys_enabled = on;
            if !on {
                if was {
                    st.faders.release_all();
                }
                for c in st.contacts.values_mut() {
                    c.key = None;
                }
            }
            st.recompute_held()
        };
        self.emit_edges(edges);
    }

    pub fn keys_enabled(&self) -> bool {
        self.lock().keys_enabled
    }

    pub fn track_mouse(&self, x: f32, y: f32, up: bool) {
        self.process(MOUSE_CONTACT, x, y, !up, Route::Pointer);
    }

    fn process(&self, id: u32, x: f32, y: f32, alive: bool, route: Route) -> CursorAction {
        let ing = {
            let mut st = self.lock();
            st.route = route;
            st.ingest(id, x, y, alive)
        };
        self.emit_edges(ing.key_edges);
        if let Some(v) = ing.analog {
            self.emit_analog(v);
        }
        ing.cursor
    }

    #[cfg(windows)]
    fn release_contact(&self, id: u32, route: Route) {
        let action = self.process(id, 0.0, 0.0, false, route);
        if let Some(w) = self.window() {
            dispatch_cursor(&w, action);
        }
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

    fn emit_analog(&self, vals: [f32; FADER_COUNT]) {
        let (listener, changed) = {
            let mut st = self.lock();
            let changed = st.last_analog != vals;
            if changed {
                st.last_analog = vals;
            }
            (st.on_analog.clone(), changed)
        };
        if !changed {
            return;
        }
        if let Some(f) = listener {
            f.call(vals[0], vals[1]);
        }
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
fn dispatch_cursor(win: &PChordWindow, action: CursorAction) {
    use slint::platform::{PointerEventButton, WindowEvent};
    use slint::{ComponentHandle, LogicalPosition};
    match action {
        CursorAction::None => {}
        CursorAction::Press(x, y) => win.window().dispatch_event(WindowEvent::PointerPressed {
            position: LogicalPosition::new(x, y),
            button: PointerEventButton::Left,
        }),
        CursorAction::Move(x, y) => win.window().dispatch_event(WindowEvent::PointerMoved {
            position: LogicalPosition::new(x, y),
        }),
        CursorAction::Release(x, y) => {
            win.window().dispatch_event(WindowEvent::PointerReleased {
                position: LogicalPosition::new(x, y),
                button: PointerEventButton::Left,
            });
            win.window().dispatch_event(WindowEvent::PointerExited);
        }
    }
}

#[cfg(windows)]
mod win {
    use super::*;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};

    const SUBCLASS_ID: usize = 0x5043_4831;

    const WM_WINDOWPOSCHANGING: u32 = 0x0046;
    const WM_DISPLAYCHANGE: u32 = 0x007E;
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
            log::info!("{route} intercepted; all contacts routed to the flat table");
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
                bridge.release_contact(id, Route::Pointer);
                return true;
            }
            return false;
        }
        let Some(w) = bridge.window() else {
            if releasing {
                bridge.release_contact(id, Route::Pointer);
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
        let up = msg == WM_POINTERUP
            || msg == WM_POINTERLEAVE
            || canceled
            || (flags & POINTER_FLAG_INCONTACT) == 0;
        let action = bridge.process(id, x, y, !up, Route::Pointer);
        dispatch_cursor(&w, action);
        true
    }

    pub(super) fn pointer_still_down(id: u32) -> bool {
        use windows_sys::Win32::UI::Input::Pointer::{GetPointerInfo, POINTER_INFO};
        unsafe {
            let mut info: POINTER_INFO = core::mem::zeroed();
            GetPointerInfo(id, &mut info) != 0
                && info.pointerFlags & POINTER_FLAG_INCONTACT != 0
                && info.pointerFlags & POINTER_FLAG_CANCELED == 0
        }
    }

    unsafe fn handle_touch(bridge: &Bridge, hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> bool {
        use slint::ComponentHandle;
        use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
        use windows_sys::Win32::UI::Input::Touch::{
            CloseTouchInputHandle, GetTouchInputInfo, TOUCHEVENTF_DOWN, TOUCHEVENTF_UP, TOUCHINPUT,
        };

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

        let mut cursors: Vec<CursorAction> = Vec::with_capacity(count);
        for ti in &inputs[..count] {
            let mut pt = POINT {
                x: ti.x / 100,
                y: ti.y / 100,
            };
            ScreenToClient(hwnd, &mut pt);
            let (x, y) = (pt.x as f32 / scale, pt.y as f32 / scale);
            let up = ti.dwFlags & TOUCHEVENTF_UP != 0;
            let _ = TOUCHEVENTF_DOWN;
            cursors.push(bridge.process(ti.dwID, x, y, !up, Route::Touch));
        }
        CloseTouchInputHandle(handle);

        for action in cursors {
            dispatch_cursor(&w, action);
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

        if msg == WM_DISPLAYCHANGE {
            log::info!("WM_DISPLAYCHANGE — desktop reconfigured; will re-pin the pad");
            bridge.lock().displays_changed = true;
            return DefSubclassProc(hwnd, msg, wparam, lparam);
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
            bridge.release_contact((wparam & 0xffff) as u32, Route::Pointer);
            return 0;
        }

        let touch = matches!(
            msg,
            WM_POINTERDOWN | WM_POINTERUPDATE | WM_POINTERUP | WM_POINTERLEAVE | WM_TOUCH
        );
        if touch && bridge.is_mine(hwnd as isize) {
            let handled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if msg == WM_TOUCH {
                    handle_touch(bridge, hwnd, wparam, lparam)
                } else {
                    handle_pointer(bridge, hwnd, msg, wparam)
                }
            }))
            .unwrap_or_else(|_| {
                log::error!("panic while handling touch message {msg:#06x}; contact dropped");
                false
            });
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

    fn with_keys() -> State {
        State {
            key_geom: geom(),
            ..State::default()
        }
    }

    fn down(st: &mut State, id: u32, x: f32, y: f32) -> Ingest {
        st.ingest(id, x, y, true)
    }
    fn up(st: &mut State, id: u32, x: f32, y: f32) -> Ingest {
        st.ingest(id, x, y, false)
    }

    #[test]
    fn a_single_contact_presses_and_releases_its_key() {
        let mut st = with_keys();
        let edges = down(&mut st, 7, 45.0, 150.0).key_edges;
        assert_eq!(edges, vec![(0, true)]);
        assert!(st.held[0]);
        assert_eq!(st.stats.live, 1);
        let edges = up(&mut st, 7, 45.0, 150.0).key_edges;
        assert_eq!(edges, vec![(0, false)]);
        assert_eq!(st.stats.live, 0);
    }

    #[test]
    fn ten_contacts_are_all_kept_and_none_dropped() {
        let mut st = with_keys();
        for id in 0..10u32 {
            let x = geom().center(id as usize % KEY_COUNT);
            down(&mut st, id, x, 150.0);
        }
        assert_eq!(st.stats.live, 10);
        assert_eq!(st.stats.peak, 10);
        assert_eq!(st.stats.dropped, 0);
    }

    #[test]
    fn past_the_ceiling_contacts_are_dropped_not_stolen() {
        let mut st = with_keys();
        for id in 0..(MAX_CONTACTS as u32) {
            down(&mut st, id, geom().center(0), 150.0);
        }
        assert_eq!(st.stats.live, MAX_CONTACTS);
        let overflow = MAX_CONTACTS as u32;
        let ing = down(&mut st, overflow, 45.0, 150.0);
        assert!(ing.key_edges.is_empty());
        assert_eq!(st.stats.dropped, 1);
        assert_eq!(st.stats.live, MAX_CONTACTS);
    }

    #[test]
    fn two_contacts_on_the_same_key_hold_it_until_both_lift() {
        let mut st = with_keys();
        assert_eq!(down(&mut st, 1, 45.0, 150.0).key_edges, vec![(0, true)]);
        assert!(down(&mut st, 2, 46.0, 150.0).key_edges.is_empty());
        assert!(st.held[0]);
        assert!(up(&mut st, 1, 45.0, 150.0).key_edges.is_empty());
        assert!(st.held[0]);
        assert_eq!(up(&mut st, 2, 46.0, 150.0).key_edges, vec![(0, false)]);
        assert!(!st.held[0]);
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
    fn a_contact_sliding_along_the_row_moves_the_held_key() {
        let mut st = with_keys();
        assert_eq!(down(&mut st, 1, 45.0, 150.0).key_edges, vec![(0, true)]);
        let edges = down(&mut st, 1, 145.0, 150.0).key_edges;
        assert!(edges.contains(&(0, false)), "{edges:?}");
        assert!(edges.contains(&(1, true)), "{edges:?}");
        assert!(!st.held[0] && st.held[1]);
    }

    #[test]
    fn disabling_keys_releases_everything_held() {
        let mut st = with_keys();
        down(&mut st, 1, 45.0, 150.0);
        assert!(st.held[0]);
        st.keys_enabled = false;
        for c in st.contacts.values_mut() {
            c.key = None;
        }
        let edges = st.recompute_held();
        assert_eq!(edges, vec![(0, false)]);
    }

    #[test]
    fn reset_contacts_releases_every_key_and_clears_the_table() {
        let mut st = with_keys();
        down(&mut st, 1, 45.0, 150.0);
        down(&mut st, 2, 145.0, 150.0);
        let edges = st.reset_contacts();
        assert_eq!(st.stats.live, 0);
        assert!(st.contacts.is_empty());
        assert!(edges.contains(&(0, false)));
        assert!(edges.contains(&(1, false)));
        assert!(
            st.quiet_contacts(std::time::Duration::ZERO).is_empty(),
            "a released contact must not be left for the watchdog to find"
        );
    }

    #[test]
    fn only_silent_contacts_are_offered_to_the_watchdog() {
        let silent = std::time::Duration::from_millis(400);
        let mut st = with_keys();
        down(&mut st, 7, 45.0, 150.0);
        down(&mut st, 8, 145.0, 150.0);
        assert!(st.quiet_contacts(silent).is_empty());

        if let Some(c) = st.contacts.get_mut(&7) {
            c.seen = Instant::now() - std::time::Duration::from_secs(1);
        }
        assert_eq!(st.quiet_contacts(silent), vec![7]);

        st.restamp(7);
        assert!(st.quiet_contacts(silent).is_empty());
    }

    #[test]
    fn the_top_strip_is_chrome_but_the_key_band_is_not() {
        let st = with_keys();
        assert!(st.is_chrome_point(20.0, 20.0));
        assert!(!st.is_chrome_point(45.0, 150.0));
    }

    #[test]
    fn an_overlay_makes_the_whole_screen_chrome() {
        let mut st = with_keys();
        st.keys_enabled = false;
        assert!(st.is_chrome_point(45.0, 150.0));
    }

    #[test]
    fn a_chrome_contact_becomes_the_cursor_and_a_key_contact_does_not() {
        let mut st = with_keys();
        assert_eq!(down(&mut st, 1, 45.0, 150.0).cursor, CursorAction::None);
        assert_eq!(st.cursor, None);
        assert_eq!(
            down(&mut st, 2, 20.0, 20.0).cursor,
            CursorAction::Press(20.0, 20.0)
        );
        assert_eq!(st.cursor, Some(2));
        assert_eq!(
            down(&mut st, 2, 30.0, 25.0).cursor,
            CursorAction::Move(30.0, 25.0)
        );
        assert_eq!(
            up(&mut st, 2, 30.0, 25.0).cursor,
            CursorAction::Release(30.0, 25.0)
        );
        assert_eq!(st.cursor, None);
    }

    #[test]
    fn a_finger_already_down_cannot_claim_the_cursor_when_an_overlay_opens() {
        let mut st = with_keys();
        assert_eq!(down(&mut st, 1, 45.0, 150.0).key_edges, vec![(0, true)]);
        assert_eq!(st.cursor, None);

        assert_eq!(
            down(&mut st, 2, 20.0, 20.0).cursor,
            CursorAction::Press(20.0, 20.0)
        );
        st.keys_enabled = false;
        for c in st.contacts.values_mut() {
            c.key = None;
        }
        st.recompute_held();
        assert_eq!(
            up(&mut st, 2, 20.0, 20.0).cursor,
            CursorAction::Release(20.0, 20.0)
        );
        assert_eq!(st.cursor, None);

        assert_eq!(down(&mut st, 1, 46.0, 151.0).cursor, CursorAction::None);
        assert_eq!(down(&mut st, 1, 48.0, 152.0).cursor, CursorAction::None);
        assert_eq!(st.cursor, None);

        assert_eq!(
            down(&mut st, 3, 300.0, 400.0).cursor,
            CursorAction::Press(300.0, 400.0)
        );
        assert_eq!(st.cursor, Some(3));
    }

    #[test]
    fn a_contact_that_arrived_while_the_cursor_was_taken_never_inherits_it() {
        let mut st = with_keys();
        assert_eq!(
            down(&mut st, 1, 20.0, 20.0).cursor,
            CursorAction::Press(20.0, 20.0)
        );
        assert_eq!(down(&mut st, 2, 40.0, 20.0).cursor, CursorAction::None);
        assert_eq!(
            up(&mut st, 1, 20.0, 20.0).cursor,
            CursorAction::Release(20.0, 20.0)
        );
        assert_eq!(down(&mut st, 2, 41.0, 21.0).cursor, CursorAction::None);
        assert_eq!(st.cursor, None);

        assert_eq!(up(&mut st, 2, 41.0, 21.0).cursor, CursorAction::None);
        assert_eq!(
            down(&mut st, 2, 41.0, 21.0).cursor,
            CursorAction::Press(41.0, 21.0)
        );
        assert_eq!(st.cursor, Some(2));
    }

    #[test]
    fn only_one_cursor_at_a_time() {
        let mut st = with_keys();
        assert_eq!(
            down(&mut st, 1, 20.0, 20.0).cursor,
            CursorAction::Press(20.0, 20.0)
        );
        assert_eq!(down(&mut st, 2, 40.0, 20.0).cursor, CursorAction::None);
        assert_eq!(st.cursor, Some(1));
    }

    #[test]
    fn the_mouse_contact_never_drives_the_cursor() {
        let mut st = with_keys();
        assert_eq!(
            down(&mut st, MOUSE_CONTACT, 20.0, 20.0).cursor,
            CursorAction::None
        );
        assert_eq!(st.cursor, None);
    }

    #[test]
    fn display_change_flag_is_one_shot() {
        let b = Bridge::create();
        assert!(!b.take_display_change(), "starts clear");
        b.lock().displays_changed = true;
        assert!(b.take_display_change(), "reports the change once");
        assert!(!b.take_display_change(), "and clears itself");
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
}
