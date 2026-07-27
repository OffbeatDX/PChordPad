use crate::config::Config;
use crate::diagnostics::Diagnostics;
use crate::monitor;
use crate::navmirror::NavMirror;
use crate::spiceapi;
use crate::touchbridge;
use crate::PChordWindow;
use slint::{ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

pub struct Runtime {
    pub diag: Rc<RefCell<Diagnostics>>,
    pub bridge: Arc<touchbridge::Bridge>,
    pub nav: Arc<NavMirror>,
    pub rotated_device: Rc<RefCell<Option<String>>>,
    pub api: Rc<RefCell<Option<Rc<spiceapi::Client>>>>,
    pub keys: Rc<VecModel<bool>>,
    pub log: Rc<VecModel<slint::SharedString>>,
    pub windowed: Rc<Cell<bool>>,
}

impl Runtime {
    pub fn new(cfg: &Config) -> Self {
        let api = Some(Rc::new(spiceapi::Client::start(cfg.api_port)));
        Self {
            diag: Rc::new(RefCell::new(Diagnostics::new())),
            bridge: touchbridge::Bridge::create(),
            nav: NavMirror::new(),
            rotated_device: Rc::new(RefCell::new(None)),
            api: Rc::new(RefCell::new(api)),
            keys: Rc::new(VecModel::from(vec![false; 12])),
            log: Rc::new(VecModel::default()),
            windowed: Rc::new(Cell::new(cfg.windowed)),
        }
    }

    pub fn apply_config(w: &PChordWindow, cfg: &Config) {
        w.set_relative_faders(cfg.relative_faders);
        w.set_fader_decay(cfg.fader_decay);
        w.set_fader_tick(cfg.fader_tick_ms);
        w.set_fader_dead(cfg.fader_dead);
        w.set_fader_curve(cfg.fader_curve);
        w.set_fader_rel_travel(cfg.fader_rel_travel);
        w.set_fader_speed_dead(cfg.fader_speed_dead);
        w.set_playfield_fit(cfg.playfield_fit);
        w.set_diagnostics(cfg.diagnostics);
        w.set_api_port(cfg.api_port as i32);
        w.set_light_keys(cfg.light_keys);
        w.set_flip_vertical(cfg.flip_vertical);
        w.set_windowed(cfg.windowed);
        w.set_pad_monitor(
            monitor::resolve_index(cfg.pad_monitor, monitor::default_pad_index()) as i32,
        );
        w.set_nav_monitor(
            monitor::resolve_index(cfg.nav_monitor, monitor::default_nav_index()) as i32,
        );
    }

    pub fn read_config(w: &PChordWindow) -> Config {
        Config {
            relative_faders: w.get_relative_faders(),
            fader_decay: w.get_fader_decay(),
            fader_tick_ms: w.get_fader_tick(),
            fader_dead: w.get_fader_dead(),
            fader_curve: w.get_fader_curve(),
            fader_rel_travel: w.get_fader_rel_travel(),
            fader_speed_dead: w.get_fader_speed_dead(),
            playfield_fit: w.get_playfield_fit(),
            diagnostics: w.get_diagnostics(),
            api_port: w.get_api_port().clamp(1, 65535) as u16,
            light_keys: w.get_light_keys(),
            flip_vertical: w.get_flip_vertical(),
            windowed: w.get_windowed(),
            pad_monitor: w.get_pad_monitor(),
            nav_monitor: w.get_nav_monitor(),
        }
    }

    pub fn fill_monitor_options(w: &PChordWindow) {
        let labels = monitor::labels();
        let model: Rc<VecModel<slint::SharedString>> = Rc::new(VecModel::from(
            labels.into_iter().map(Into::into).collect::<Vec<_>>(),
        ));
        w.set_monitor_options(ModelRc::from(model));
    }

    pub fn pad_mon(w: &PChordWindow) -> Option<monitor::MonInfo> {
        monitor::by_index(w.get_pad_monitor().max(0) as usize)
    }

    pub fn nav_mon(w: &PChordWindow) -> Option<monitor::MonInfo> {
        monitor::by_index(w.get_nav_monitor().max(0) as usize)
    }

    pub fn pin(w: &PChordWindow, m: &monitor::MonInfo) {
        w.window()
            .set_position(slint::PhysicalPosition::new(m.left, m.top));
        w.window().set_size(slint::PhysicalSize::new(
            m.width.max(1) as u32,
            m.height.max(1) as u32,
        ));
    }

    pub fn make_windowed(w: &PChordWindow) {
        let Some(m) = Self::pad_mon(w) else {
            return;
        };
        let width = 1280.min((m.width - 80).max(900));
        let height = 720.min((m.height - 80).max(600));
        w.window()
            .set_size(slint::PhysicalSize::new(width as u32, height as u32));
        w.window().set_position(slint::PhysicalPosition::new(
            m.left + (m.width - width) / 2,
            m.top + (m.height - height) / 2,
        ));
    }

    pub fn repin_if_needed(w: &PChordWindow) {
        let Some(m) = Self::pad_mon(w) else {
            return;
        };
        let pos = slint::PhysicalPosition::new(m.left, m.top);
        let size = slint::PhysicalSize::new(m.width.max(1) as u32, m.height.max(1) as u32);
        let win = w.window();
        if win.position() != pos || win.size() != size {
            log::info!(
                "pad window drifted to {:?} {:?}; re-pinning to {}",
                win.position(),
                win.size(),
                m.label
            );
            Self::pin(w, &m);
        }
    }

    pub fn start_pin_watch(&self, w: &PChordWindow) -> slint::Timer {
        let weak = w.as_weak();
        let windowed = self.windowed.clone();
        let bridge = self.bridge.clone();
        let t = slint::Timer::default();
        t.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(1500),
            move || {
                let Some(w) = weak.upgrade() else { return };
                if windowed.get() {
                    bridge.set_pinned(None);
                    return;
                }
                bridge.set_pinned(Self::pad_mon(&w).map(|m| touchbridge::PinRect {
                    left: m.left,
                    top: m.top,
                    width: m.width.max(1),
                    height: m.height.max(1),
                }));
                Self::repin_if_needed(&w);
            },
        );
        t
    }

    pub fn sync_key_geom(bridge: &touchbridge::Bridge, w: &PChordWindow) {
        let panel_y = w.get_panel_y();
        bridge.set_key_geom(touchbridge::KeyGeom {
            x: w.get_key_geom_x(),
            y: panel_y + w.get_key_geom_y(),
            w: w.get_key_geom_w(),
            h: w.get_key_geom_h(),
            key_w: w.get_key_geom_key_w(),
            gap: w.get_key_geom_gap(),
        });
    }

    pub fn sync_keys_enabled(bridge: &touchbridge::Bridge, w: &PChordWindow) {
        let want = w.get_keys_interactive();
        if bridge.keys_enabled() != want {
            bridge.set_keys_enabled(want);
        }
    }

    pub fn sync_pad_orientation(w: &PChordWindow, rotated_device: &RefCell<Option<String>>) {
        let flip = w.get_flip_vertical();
        let Some(pad) = Self::pad_mon(w) else {
            return;
        };

        let prev = rotated_device.borrow().clone();
        if let Some(old) = prev {
            if old != pad.device || !flip {
                if let Err(e) = monitor::set_flipped(&old, false) {
                    log::warn!("restore orientation on {old}: {e}");
                }
                *rotated_device.borrow_mut() = None;
            }
        }

        if flip {
            match monitor::set_flipped(&pad.device, true) {
                Ok(()) => *rotated_device.borrow_mut() = Some(pad.device.clone()),
                Err(e) => {
                    log::warn!("flip pad monitor failed: {e}");
                    w.set_flip_vertical(false);
                    w.set_nav_status(format!("Flip failed: {e}").into());
                }
            }
        }
    }

    pub fn bind(&self, w: &PChordWindow) {
        w.set_keys(ModelRc::from(self.keys.clone()));
        w.set_log(ModelRc::from(self.log.clone()));
        w.set_mouse_keys(true);

        self.bind_nav(w);
        self.bind_keys(w);
        self.bind_analog(w);
        self.bind_mouse_key(w);
        self.bind_mark(w);
        self.bind_quit(w);
        self.bind_settings(w);
    }

    fn bind_mark(&self, w: &PChordWindow) {
        let weak = w.as_weak();
        let diag = self.diag.clone();
        let api = self.api.clone();
        let bridge = self.bridge.clone();
        let log_model = self.log.clone();

        w.on_mark_stall(move || {
            let touch = bridge.stats();
            let api_snap = match api.borrow().as_ref() {
                Some(c) => c.debug_snapshot(),
                None => "spiceapi client: none".into(),
            };
            let detail = format!(
                "touch live={} peak={} dropped={} adopted={} revived={} vetoed_moves={}\n{api_snap}",
                touch.live,
                touch.peak,
                touch.dropped,
                touch.adopted,
                touch.revived,
                bridge.vetoed_moves()
            );
            let banner = diag.borrow_mut().mark_stall(&detail);
            let Some(w) = weak.upgrade() else { return };
            let d = diag.borrow();
            log_model.set_vec(
                d.log_lines()
                    .map(slint::SharedString::from)
                    .collect::<Vec<_>>(),
            );
            w.set_latency(d.latency_summary().into());
            let _ = banner;
        });
    }

    fn bind_nav(&self, w: &PChordWindow) {
        let weak = w.as_weak();
        let nav = self.nav.clone();
        let bridge = self.bridge.clone();
        w.on_nav_toggled(move || {
            let Some(w) = weak.upgrade() else { return };
            if w.get_nav_active() {
                match (Self::nav_mon(&w), Self::pad_mon(&w)) {
                    (Some(nav_m), Some(pad_m)) => {
                        let label = nav_m.label.clone();
                        match nav.start(nav_m, &pad_m) {
                            Ok(()) => {
                                log::info!("Nav mode on — mirroring {label}");
                                bridge.set_keys_enabled(false);
                            }
                            Err(e) => {
                                log::warn!("Nav mode failed: {e}");
                                w.set_nav_active(false);
                                w.set_nav_status(e.into());
                            }
                        }
                    }
                    _ => {
                        w.set_nav_active(false);
                        w.set_nav_status("invalid monitor selection".into());
                    }
                }
            } else {
                nav.stop();
                bridge.set_keys_enabled(w.get_keys_interactive());
                w.set_nav_status("".into());
                log::info!("Nav mode off");
            }
        });

        let nav = self.nav.clone();
        w.on_nav_pointer(move |nx, ny, kind| {
            nav.pointer(nx, ny, kind);
        });
    }

    fn bind_keys(&self, w: &PChordWindow) {
        let weak = w.as_weak();
        let diag = self.diag.clone();
        let bridge_latency = self.bridge.clone();
        let api = self.api.clone();
        let keys = self.keys.clone();
        let log = self.log.clone();

        self.bridge.set_key_listener(move |index, down| {
            let name = format!("Button {}", index + 1);
            if let Some(api) = api.borrow().as_ref() {
                api.button(&name, down);
            }
            if down {
                if let Some(ms) = bridge_latency.take_latency_ms() {
                    diag.borrow_mut().on_latency(ms);
                }
            }
            diag.borrow_mut().on_digital(&name, down);
            let Some(w) = weak.upgrade() else { return };
            let d = diag.borrow();
            for (i, on) in d.keys().iter().enumerate() {
                if keys.row_data(i) != Some(*on) {
                    keys.set_row_data(i, *on);
                }
            }
            log.set_vec(
                d.log_lines()
                    .map(slint::SharedString::from)
                    .collect::<Vec<_>>(),
            );
            w.set_digital_events(d.digital_events() as i32);
            w.set_analog_events(d.analog_events() as i32);
            w.set_latency(d.latency_summary().into());
        });
    }

    fn bind_analog(&self, w: &PChordWindow) {
        let diag = self.diag.clone();
        let api = self.api.clone();
        let levels = std::cell::Cell::new((0.0f32, 0.0f32));
        w.on_analog(move |name, value| {
            let (mut l, mut r) = levels.get();
            match name.as_str() {
                "Fader-L" => l = value,
                "Fader-R" => r = value,
                _ => log::warn!("unexpected analog {name}"),
            }
            levels.set((l, r));
            if let Some(api) = api.borrow().as_ref() {
                api.faders(l, r);
            }
            diag.borrow_mut().on_analog(&name, value);
        });
    }

    fn bind_mouse_key(&self, w: &PChordWindow) {
        let bridge = self.bridge.clone();
        w.on_mouse_key(move |x, y, up| {
            bridge.track_mouse(x, y, up);
        });
    }

    fn bind_quit(&self, w: &PChordWindow) {
        let rotated_device = self.rotated_device.clone();
        w.on_quit(move || {
            if let Some(dev) = rotated_device.borrow_mut().take() {
                let _ = monitor::set_flipped(&dev, false);
            }
            slint::quit_event_loop().unwrap_or_default();
        });
    }

    fn bind_settings(&self, w: &PChordWindow) {
        let weak = w.as_weak();
        let api = self.api.clone();
        let bridge = self.bridge.clone();
        let nav = self.nav.clone();
        let rotated_device = self.rotated_device.clone();
        let windowed = self.windowed.clone();

        w.on_settings_changed(move || {
            let Some(w) = weak.upgrade() else { return };
            Self::fill_monitor_options(&w);
            let cfg = Self::read_config(&w);
            cfg.save();

            let want = Some(cfg.api_port);
            let cur = api.borrow().as_ref().map(|c| c.port());
            if cur != want {
                *api.borrow_mut() = want.map(|p| Rc::new(spiceapi::Client::start(p)));
            }

            Self::sync_pad_orientation(&w, &rotated_device);
            Self::fill_monitor_options(&w);
            w.set_pad_monitor(monitor::resolve_index(
                w.get_pad_monitor(),
                monitor::default_pad_index(),
            ) as i32);
            w.set_nav_monitor(monitor::resolve_index(
                w.get_nav_monitor(),
                monitor::default_nav_index(),
            ) as i32);

            let was_windowed = windowed.replace(cfg.windowed);
            if cfg.windowed {
                if !was_windowed {
                    Self::make_windowed(&w);
                }
            } else {
                if let Some(m) = Self::pad_mon(&w) {
                    log::info!("pinning pad to {}", m.label);
                    Self::pin(&w, &m);
                }
            }

            if w.get_nav_active() {
                if let (Some(nav_m), Some(pad_m)) = (Self::nav_mon(&w), Self::pad_mon(&w)) {
                    if let Err(e) = nav.start(nav_m, &pad_m) {
                        log::warn!("Nav retarget failed: {e}");
                        w.set_nav_status(e.into());
                    }
                }
            }

            Self::sync_key_geom(&bridge, &w);
            Self::sync_keys_enabled(&bridge, &w);
        });
    }

    pub fn start_tick(&self, w: &PChordWindow) -> slint::Timer {
        let bridge = self.bridge.clone();
        let nav = self.nav.clone();
        let weak = w.as_weak();
        let api = self.api.clone();
        let installed = std::cell::Cell::new(false);
        let nav_generation = std::cell::Cell::new(0u64);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let last_tick = std::cell::Cell::new(std::time::Instant::now());
        const TICK_LATE: std::time::Duration = std::time::Duration::from_millis(250);
        let t = slint::Timer::default();
        t.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(16),
            move || {
                let late = last_tick.replace(std::time::Instant::now()).elapsed();
                if late > TICK_LATE {
                    log::warn!(
                        "UI thread stalled {:.0} ms — no touch delivered in that window",
                        late.as_secs_f32() * 1000.0
                    );
                }
                let Some(w) = weak.upgrade() else { return };
                if !installed.get() {
                    if touchbridge::install(&bridge, w.as_weak()) {
                        log::info!("touch interception installed");
                        installed.set(true);
                        w.set_mouse_keys(false);
                    } else if std::time::Instant::now() > deadline {
                        log::error!(
                            "touch interception never installed; mouse key fallback active"
                        );
                        installed.set(true);
                        w.set_mouse_keys(true);
                    }
                }
                Self::sync_key_geom(&bridge, &w);
                Self::sync_keys_enabled(&bridge, &w);

                if w.get_nav_active() {
                    if let Some(frame) = nav.take_frame_after(nav_generation.get()) {
                        nav_generation.set(frame.generation);
                        w.set_nav_status(
                            format!("{} · {:.1} ms", frame.status, frame.capture_ms).into(),
                        );
                        if frame.width > 0 && frame.height > 0 && !frame.rgba.is_empty() {
                            let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
                                &frame.rgba,
                                frame.width,
                                frame.height,
                            );
                            w.set_nav_frame(Image::from_rgba8(buf));
                        }
                    }
                }

                let s = bridge.stats();
                w.set_live_contacts(s.live as i32);
                w.set_peak_contacts(s.peak as i32);
                w.set_dropped_contacts(s.dropped as i32);
                match api.borrow().as_ref() {
                    Some(api) => {
                        w.set_api_status(api.status().into());
                        w.set_api_connected(api.connected());
                    }
                    None => w.set_api_status("disabled".into()),
                }
            },
        );
        t
    }
}
