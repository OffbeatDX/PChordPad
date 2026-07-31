use crate::config::Config;
use crate::diagnostics::Diagnostics;
use crate::faders::{FaderCfg, Zone, FADER_COUNT};
use crate::monitor;
use crate::navmirror::NavMirror;
use crate::spiceapi;
use crate::touchbridge;
use crate::PChordWindow;
use slint::{ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

const BUTTON_NAMES: [&str; touchbridge::KEY_COUNT] = [
    "Button 1",
    "Button 2",
    "Button 3",
    "Button 4",
    "Button 5",
    "Button 6",
    "Button 7",
    "Button 8",
    "Button 9",
    "Button 10",
    "Button 11",
    "Button 12",
];

fn set_if_changed<T: PartialEq>(want: T, current: T, set: impl FnOnce(T)) {
    if want != current {
        set(want);
    }
}

pub struct Runtime {
    pub diag: Rc<RefCell<Diagnostics>>,
    pub bridge: Arc<touchbridge::Bridge>,
    pub nav: Arc<NavMirror>,
    pub rotated_device: Rc<RefCell<Option<String>>>,
    pub api: Rc<RefCell<Option<Rc<spiceapi::Client>>>>,
    pub keys: Rc<VecModel<bool>>,
    pub log: Rc<VecModel<slint::SharedString>>,
    pub windowed: Rc<Cell<bool>>,
    diag_dirty: Rc<Cell<bool>>,
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
            diag_dirty: Rc::new(Cell::new(true)),
        }
    }

    pub fn apply_config(w: &PChordWindow, cfg: &Config) {
        w.set_relative_faders(cfg.relative_faders);
        w.set_fader_decay(cfg.fader_decay);
        w.set_fader_dead(cfg.fader_dead);
        w.set_fader_curve(cfg.fader_curve);
        w.set_fader_rel_travel(cfg.fader_rel_travel);
        w.set_fader_speed_dead(cfg.fader_speed_dead);
        w.set_playfield_fit(cfg.playfield_fit);
        w.set_key_height(cfg.key_height);
        w.set_fader_pos(cfg.fader_pos);
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
            fader_dead: w.get_fader_dead(),
            fader_curve: w.get_fader_curve(),
            fader_rel_travel: w.get_fader_rel_travel(),
            fader_speed_dead: w.get_fader_speed_dead(),
            playfield_fit: w.get_playfield_fit(),
            key_height: w.get_key_height(),
            fader_pos: w.get_fader_pos(),
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
                if bridge.restore_if_minimized() {
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

    pub fn key_geom_of(w: &PChordWindow) -> touchbridge::KeyGeom {
        touchbridge::KeyGeom {
            x: w.get_key_geom_x(),
            y: w.get_panel_y() + w.get_key_geom_y(),
            w: w.get_key_geom_w(),
            h: w.get_key_geom_h(),
            key_w: w.get_key_geom_key_w(),
            gap: w.get_key_geom_gap(),
        }
    }

    pub fn sync_key_geom(bridge: &touchbridge::Bridge, w: &PChordWindow) {
        bridge.set_key_geom(Self::key_geom_of(w));
    }

    pub fn fader_zones_of(w: &PChordWindow) -> [Zone; FADER_COUNT] {
        let py = w.get_panel_y();
        [
            Zone {
                x: w.get_fader_l_geom_x(),
                y: py + w.get_fader_l_geom_y(),
                w: w.get_fader_l_geom_w(),
                h: w.get_fader_l_geom_h(),
                center_x: w.get_fader_l_geom_center_x(),
                half: w.get_fader_l_geom_half(),
            },
            Zone {
                x: w.get_fader_r_geom_x(),
                y: py + w.get_fader_r_geom_y(),
                w: w.get_fader_r_geom_w(),
                h: w.get_fader_r_geom_h(),
                center_x: w.get_fader_r_geom_center_x(),
                half: w.get_fader_r_geom_half(),
            },
        ]
    }

    pub fn fader_cfg_of(w: &PChordWindow) -> FaderCfg {
        FaderCfg {
            relative: w.get_relative_faders(),
            curve: w.get_fader_curve(),
            dead: w.get_fader_dead(),
            decay: w.get_fader_decay(),
            rel_travel: w.get_fader_rel_travel(),
            speed_dead: w.get_fader_speed_dead(),
        }
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
        self.bind_keys();
        self.bind_analog();
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
        let keys = self.keys.clone();
        let log_model = self.log.clone();

        w.on_mark_stall(move || {
            let touch = bridge.stats();
            let api_snap = match api.borrow().as_ref() {
                Some(c) => c.debug_snapshot(),
                None => "spiceapi client: none".into(),
            };
            let detail = format!(
                "touch live={} peak={} dropped={} vetoed_moves={} expired={}\n{api_snap}",
                touch.live,
                touch.peak,
                touch.dropped,
                bridge.vetoed_moves(),
                bridge.expired_contacts()
            );
            let banner = diag.borrow_mut().mark_stall(&detail);
            let Some(w) = weak.upgrade() else { return };
            Self::refresh_diagnostics(&w, &diag, &keys, &log_model);
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

    fn bind_keys(&self) {
        let diag = self.diag.clone();
        let api = self.api.clone();
        let dirty = self.diag_dirty.clone();

        self.bridge.set_key_listener(move |index, down| {
            let name = BUTTON_NAMES[index.min(BUTTON_NAMES.len() - 1)];
            if let Some(api) = api.borrow().as_ref() {
                api.button(name, down);
            }
            diag.borrow_mut().on_digital(name, down);
            dirty.set(true);
        });
    }

    fn refresh_diagnostics(
        w: &PChordWindow,
        diag: &RefCell<Diagnostics>,
        keys: &VecModel<bool>,
        log: &VecModel<slint::SharedString>,
    ) {
        let d = diag.borrow();
        for (i, on) in d.keys().iter().enumerate() {
            if keys.row_data(i) != Some(*on) {
                keys.set_row_data(i, *on);
            }
        }
        if !w.get_diagnostics() {
            return;
        }
        log.set_vec(
            d.log_lines()
                .map(slint::SharedString::from)
                .collect::<Vec<_>>(),
        );
        w.set_digital_events(d.digital_events() as i32);
        w.set_analog_events(d.analog_events() as i32);
    }

    fn bind_analog(&self) {
        let diag = self.diag.clone();
        let api = self.api.clone();
        let dirty = self.diag_dirty.clone();
        self.bridge.set_analog_listener(move |l, r| {
            if let Some(api) = api.borrow().as_ref() {
                api.faders(l, r);
            }
            let mut d = diag.borrow_mut();
            d.on_analog("Fader-L", l);
            d.on_analog("Fader-R", r);
            dirty.set(true);
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
        let diag = self.diag.clone();
        let keys = self.keys.clone();
        let log = self.log.clone();
        let dirty = self.diag_dirty.clone();
        let windowed = self.windowed.clone();
        let installed = std::cell::Cell::new(false);
        let nav_generation = std::cell::Cell::new(0u64);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let last_tick = std::cell::Cell::new(std::time::Instant::now());
        let last_geom = std::cell::Cell::new(touchbridge::KeyGeom::default());
        let last_fader_zones = std::cell::Cell::new([Zone::default(); FADER_COUNT]);
        let last_fader_cfg = std::cell::Cell::new(FaderCfg::default());
        let last_diag_open = std::cell::Cell::new(false);
        let api_epoch = std::cell::Cell::new(0u64);
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

                if bridge.take_display_change() && !windowed.get() {
                    if let Some(m) = Self::pad_mon(&w) {
                        log::info!("re-pinning pad to {} after display change", m.label);
                        bridge.set_pinned(Some(touchbridge::PinRect {
                            left: m.left,
                            top: m.top,
                            width: m.width.max(1),
                            height: m.height.max(1),
                        }));
                        Self::repin_if_needed(&w);
                    }
                }

                let geom = Self::key_geom_of(&w);
                if geom != last_geom.get() {
                    last_geom.set(geom);
                    bridge.set_key_geom(geom);
                }
                let zones = Self::fader_zones_of(&w);
                if zones != last_fader_zones.get() {
                    last_fader_zones.set(zones);
                    bridge.set_fader_geom(zones);
                }
                let fcfg = Self::fader_cfg_of(&w);
                if fcfg != last_fader_cfg.get() {
                    last_fader_cfg.set(fcfg);
                    bridge.set_fader_cfg(fcfg);
                }
                Self::sync_keys_enabled(&bridge, &w);
                bridge.expire_lost_contacts();
                bridge.tick_faders();

                let (fvals, fgrab, fdir) = bridge.fader_snapshot();
                set_if_changed(fvals[0], w.get_fader_l(), |v| w.set_fader_l(v));
                set_if_changed(fvals[1], w.get_fader_r(), |v| w.set_fader_r(v));
                set_if_changed(fgrab[0], w.get_grabbed_l(), |v| w.set_grabbed_l(v));
                set_if_changed(fgrab[1], w.get_grabbed_r(), |v| w.set_grabbed_r(v));
                set_if_changed(fdir[0], w.get_dir_l(), |v| w.set_dir_l(v));
                set_if_changed(fdir[1], w.get_dir_r(), |v| w.set_dir_r(v));

                let diag_open = w.get_diagnostics();
                if dirty.replace(false) || diag_open != last_diag_open.replace(diag_open) {
                    Self::refresh_diagnostics(&w, &diag, &keys, &log);
                }

                if w.get_nav_active() {
                    let taken = nav.with_new_frame(nav_generation.get(), |frame| {
                        let want = (frame.width as usize) * (frame.height as usize) * 4;
                        let image = (want > 0 && frame.rgba.len() >= want).then(|| {
                            Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
                                &frame.rgba,
                                frame.width,
                                frame.height,
                            ))
                        });
                        (
                            frame.generation,
                            format!("{} · {:.1} ms", frame.status, frame.capture_ms),
                            image,
                        )
                    });
                    if let Some((generation, status, image)) = taken {
                        nav_generation.set(generation);
                        w.set_nav_status(status.into());
                        if let Some(image) = image {
                            w.set_nav_frame(image);
                        }
                    }
                }

                let s = bridge.stats();
                set_if_changed(s.live as i32, w.get_live_contacts(), |v| {
                    w.set_live_contacts(v)
                });
                set_if_changed(s.peak as i32, w.get_peak_contacts(), |v| {
                    w.set_peak_contacts(v)
                });
                set_if_changed(s.dropped as i32, w.get_dropped_contacts(), |v| {
                    w.set_dropped_contacts(v)
                });
                match api.borrow().as_ref() {
                    Some(api) => {
                        let epoch = api.status_epoch();
                        if epoch != api_epoch.replace(epoch) {
                            w.set_api_status(api.status().into());
                        }
                        set_if_changed(api.connected(), w.get_api_connected(), |v| {
                            w.set_api_connected(v)
                        });
                    }
                    None => {
                        const DISABLED: u64 = u64::MAX;
                        if api_epoch.replace(DISABLED) != DISABLED {
                            w.set_api_status("disabled".into());
                        }
                    }
                }
            },
        );
        t
    }
}
