use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    pub relative_faders: bool,
    pub fader_decay: f32,
    pub fader_dead: f32,
    pub fader_curve: f32,
    pub fader_rel_travel: f32,
    pub fader_speed_dead: f32,
    pub playfield_fit: f32,
    #[serde(default = "default_key_height")]
    pub key_height: f32,
    #[serde(default)]
    pub fader_pos: f32,
    pub diagnostics: bool,
    pub api_port: u16,
    #[serde(default)]
    pub light_keys: bool,
    #[serde(default)]
    pub flip_vertical: bool,
    #[serde(default)]
    pub windowed: bool,
    #[serde(default = "default_mon_auto")]
    pub pad_monitor: i32,
    #[serde(default = "default_mon_auto")]
    pub nav_monitor: i32,
}

fn default_mon_auto() -> i32 {
    -1
}

fn default_key_height() -> f32 {
    0.66
}

impl Default for Config {
    fn default() -> Self {
        Config {
            relative_faders: true,
            fader_decay: 0.7,
            fader_dead: 0.04,
            fader_curve: 1.0,
            fader_rel_travel: 100.0,
            fader_speed_dead: 2.5,
            playfield_fit: 1.0,
            key_height: 0.66,
            fader_pos: 0.0,
            diagnostics: false,
            api_port: crate::spiceapi::DEFAULT_PORT,
            light_keys: false,
            flip_vertical: false,
            windowed: false,
            pad_monitor: -1,
            nav_monitor: -1,
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        let mut p = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
        p.set_file_name("pchordpad.json");
        p
    }

    pub fn load() -> Self {
        load_file(&Self::path()).unwrap_or_default()
    }

    pub fn save(&self) {
        if let Err(e) = save_to_path(self, &Self::path()) {
            log::warn!("could not write settings: {e}");
        }
    }
}

fn load_file(path: &Path) -> Option<Config> {
    match std::fs::read_to_string(path) {
        Ok(s) => match serde_json::from_str::<Config>(&s) {
            Ok(c) => {
                log::info!("loaded settings from {}", path.display());
                Some(c)
            }
            Err(e) => {
                log::warn!("settings parse failed ({e}); using defaults");
                None
            }
        },
        Err(_) => None,
    }
}

fn save_to_path(cfg: &Config, path: &Path) -> Result<(), String> {
    let json = serde_json::to_string_pretty(cfg).map_err(|e| format!("serialize: {e}"))?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("rename {}: {e}", path.display())
    })?;
    log::info!("saved settings to {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir(label: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pchordpad-config-{label}-{n}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert!(c.relative_faders);
        assert_eq!(c.api_port, crate::spiceapi::DEFAULT_PORT);
        assert_eq!(c.pad_monitor, -1);
    }

    #[test]
    fn legacy_json_with_api_enabled_still_loads() {
        let dir = temp_dir("legacy");
        let path = dir.join("pchordpad.json");
        let mut value = serde_json::to_value(Config::default()).unwrap();
        value["api_enabled"] = serde_json::json!(false);
        value["api_port"] = serde_json::json!(4242);
        std::fs::write(&path, serde_json::to_string_pretty(&value).unwrap()).unwrap();

        let cfg = load_file(&path).expect("load");
        assert_eq!(cfg.api_port, 4242);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_file_uses_defaults() {
        let dir = temp_dir("missing");
        let path = dir.join("pchordpad.json");
        assert!(!path.exists());
        assert_eq!(load_file(&path), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn malformed_input_falls_back_to_defaults() {
        let dir = temp_dir("bad");
        let path = dir.join("pchordpad.json");
        std::fs::write(&path, "{not-json").unwrap();
        assert!(load_file(&path).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn save_is_atomic_and_reports_failed_writes() {
        let dir = temp_dir("atomic");
        let path = dir.join("pchordpad.json");
        let cfg = Config::default();
        save_to_path(&cfg, &path).unwrap();
        assert!(path.exists());
        assert!(!path.with_extension("json.tmp").exists());

        let blocked = dir.join("blocked");
        std::fs::write(&blocked, "file").unwrap();
        let err = save_to_path(&cfg, &blocked.join("pchordpad.json")).expect_err("mkdir fails");
        assert!(err.contains("mkdir"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
