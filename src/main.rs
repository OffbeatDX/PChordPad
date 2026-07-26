#![cfg_attr(windows, windows_subsystem = "windows")]

mod config;
mod diagnostics;
mod monitor;
mod navmirror;
mod runtime;
mod spiceapi;
mod touchbridge;

use config::Config;
use runtime::Runtime;

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    init_logging();
    let cfg = Config::load();

    let w = PChordWindow::new()?;
    w.set_window_title(touchbridge::WINDOW_TITLE.into());
    w.set_app_version(env!("PCHORDPAD_VERSION").into());
    Runtime::fill_monitor_options(&w);
    Runtime::apply_config(&w, &cfg);

    let rt = Runtime::new(&cfg);
    rt.bind(&w);

    let place = (!cfg.windowed).then(|| Runtime::pad_mon(&w)).flatten();
    if let Some(m) = &place {
        log::info!("placing panel on {}", m.label);
        Runtime::pin(&w, m);
    }

    w.show()?;

    if let Some(m) = &place {
        Runtime::pin(&w, m);
    }

    Runtime::sync_pad_orientation(&w, &rt.rotated_device);
    if !cfg.windowed {
        if let Some(m) = Runtime::pad_mon(&w) {
            Runtime::pin(&w, &m);
        }
    }

    Runtime::sync_key_geom(&rt.bridge, &w);
    let _tick = rt.start_tick(&w);
    let _pin = rt.start_pin_watch(&w);
    let _nav = rt.nav.clone();

    slint::run_event_loop()
}

fn init_logging() {
    struct Stderr;
    impl log::Log for Stderr {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }
        fn log(&self, r: &log::Record) {
            eprintln!("[{}] {}", r.level(), r.args());
        }
        fn flush(&self) {}
    }
    static LOGGER: Stderr = Stderr;
    let _ = log::set_logger(&LOGGER).map(|()| log::set_max_level(log::LevelFilter::Info));
}
