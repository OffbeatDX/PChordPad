use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub relative_faders: bool,
    pub fader_decay: f32,
    pub fader_tick_ms: i64,
    pub fader_dead: f32,
    pub fader_curve: f32,
    pub fader_rel_travel: f32,
    pub fader_speed_dead: f32,
    pub playfield_fit: f32,
    pub diagnostics: bool,
    pub api_enabled: bool,
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

impl Default for Config {
    fn default() -> Self {
        Config {
            relative_faders: true,
            fader_decay: 0.7,
            fader_tick_ms: 16,
            fader_dead: 0.04,
            fader_curve: 1.0,
            fader_rel_travel: 100.0,
            fader_speed_dead: 2.5,
            playfield_fit: 1.0,
            diagnostics: false,
            api_enabled: true,
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
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(s) => match serde_json::from_str::<Config>(&s) {
                Ok(mut c) => {
                    c.api_enabled = true;
                    log::info!("loaded settings from {}", path.display());
                    c
                }
                Err(e) => {
                    log::warn!("settings parse failed ({e}); using defaults");
                    Config::default()
                }
            },
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self) {
        let path = Self::path();
        match serde_json::to_string_pretty(self) {
            Ok(s) => {
                if let Err(e) = std::fs::write(&path, s) {
                    log::warn!("could not write {}: {e}", path.display());
                } else {
                    log::info!("saved settings to {}", path.display());
                }
            }
            Err(e) => log::warn!("settings serialize failed: {e}"),
        }
    }
}
