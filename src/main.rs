#![cfg_attr(windows, windows_subsystem = "windows")]

mod config;
mod diagnostics;
mod faders;
mod monitor;
mod navmirror;
mod runtime;
mod spiceapi;
mod touchbridge;

use config::Config;
use runtime::Runtime;

slint::include_modules!();

pub(crate) fn stamp(elapsed: std::time::Duration) -> String {
    let ms = elapsed.as_millis();
    format!(
        "{:02}:{:02}.{:03}",
        ms / 60_000,
        (ms / 1000) % 60,
        ms % 1000
    )
}

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
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Instant;

    struct AsyncLog {
        start: Instant,
        tx: Mutex<Option<std::sync::mpsc::Sender<String>>>,
        queued: std::sync::Arc<std::sync::atomic::AtomicU64>,
    }

    const QUEUE_CAP: u64 = 4096;

    impl log::Log for AsyncLog {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }
        fn log(&self, r: &log::Record) {
            use std::sync::atomic::Ordering;
            let stamp = crate::stamp(self.start.elapsed());
            let line = format!("[{stamp}] [{:<5}] {}\n", r.level(), r.args());
            let Ok(g) = self.tx.lock() else { return };
            let Some(tx) = g.as_ref() else { return };
            if self.queued.load(Ordering::Relaxed) >= QUEUE_CAP {
                return;
            }
            self.queued.fetch_add(1, Ordering::Relaxed);
            if tx.send(line).is_err() {
                self.queued.fetch_sub(1, Ordering::Relaxed);
            }
        }
        fn flush(&self) {}
    }

    let mut path = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
    path.set_file_name("pchordpad.log");
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > 5_000_000 {
            let bak = path.with_extension("log.old");
            let _ = std::fs::rename(&path, &bak);
        }
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok();

    let queued = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let tx = file.and_then(|mut f| {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let queued = queued.clone();
        std::thread::Builder::new()
            .name("pchordpad-log".into())
            .spawn(move || {
                use std::sync::atomic::Ordering;
                while let Ok(line) = rx.recv() {
                    queued.fetch_sub(1, Ordering::Relaxed);
                    let _ = f.write_all(line.as_bytes());
                    while let Ok(more) = rx.try_recv() {
                        queued.fetch_sub(1, Ordering::Relaxed);
                        let _ = f.write_all(more.as_bytes());
                    }
                    let _ = f.flush();
                }
            })
            .ok()
            .map(|_| tx)
    });

    let logger = Box::leak(Box::new(AsyncLog {
        start: Instant::now(),
        tx: Mutex::new(tx),
        queued,
    }));
    let _ = log::set_logger(logger).map(|()| log::set_max_level(log::LevelFilter::Info));
    log::info!(
        "=== PChordPad {} starting; file log: {} ===",
        env!("PCHORDPAD_VERSION"),
        path.display()
    );
}
