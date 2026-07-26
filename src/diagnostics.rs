use std::collections::VecDeque;
use std::time::Instant;

const LOG_LINES: usize = 5;

pub struct Diagnostics {
    start: Instant,
    keys: [bool; 12],
    log: VecDeque<String>,
    digital_events: u64,
    analog_events: u64,
    latency: Vec<f32>,
}

impl Diagnostics {
    pub fn new() -> Self {
        Diagnostics {
            start: Instant::now(),
            keys: [false; 12],
            log: VecDeque::with_capacity(LOG_LINES + 1),
            digital_events: 0,
            analog_events: 0,
            latency: Vec::new(),
        }
    }

    pub fn keys(&self) -> [bool; 12] {
        self.keys
    }
    pub fn digital_events(&self) -> u64 {
        self.digital_events
    }
    pub fn analog_events(&self) -> u64 {
        self.analog_events
    }
    pub fn log_lines(&self) -> impl Iterator<Item = &String> {
        self.log.iter()
    }

    pub fn on_digital(&mut self, name: &str, down: bool) {
        self.digital_events += 1;
        if let Some(i) = button_index(name) {
            self.keys[i] = down;
        }
        let line = format!(
            "{}  {:<14} {}",
            self.stamp(),
            name,
            if down { "DOWN" } else { "up" }
        );
        log::info!("{line}");
        self.push(line);
    }

    pub fn on_analog(&mut self, name: &str, value: f32) {
        self.analog_events += 1;
        let _ = (name, value);
    }

    pub fn on_latency(&mut self, ms: f32) {
        self.latency.push(ms);
    }

    pub fn latency_summary(&self) -> String {
        if self.latency.is_empty() {
            return "—".into();
        }
        let mut s = self.latency.clone();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let p50 = s[s.len() / 2];
        let max = s[s.len() - 1];
        format!("p50 {p50:.1} ms · max {max:.1} ms · n={}", s.len())
    }

    fn stamp(&self) -> String {
        let ms = self.start.elapsed().as_millis();
        format!(
            "{:02}:{:02}.{:03}",
            ms / 60_000,
            (ms / 1000) % 60,
            ms % 1000
        )
    }

    fn push(&mut self, line: String) {
        if self.log.len() == LOG_LINES {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }
}

fn button_index(name: &str) -> Option<usize> {
    let n: usize = name.strip_prefix("Button ")?.parse().ok()?;
    (1..=12).contains(&n).then(|| n - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_names_map_to_lamps_and_auxiliaries_do_not() {
        assert_eq!(button_index("Button 1"), Some(0));
        assert_eq!(button_index("Button 12"), Some(11));
        assert_eq!(button_index("Button 13"), None);
        assert_eq!(button_index("Button 0"), None);
        assert_eq!(button_index("Service"), None);
        assert_eq!(button_index("Fader-L Left"), None);
    }

    #[test]
    fn digital_events_track_held_keys() {
        let mut st = Diagnostics::new();
        st.on_digital("Button 3", true);
        st.on_digital("Service", true);
        assert!(st.keys()[2]);
        assert_eq!(
            st.keys().iter().filter(|k| **k).count(),
            1,
            "Service is not a lamp"
        );
        st.on_digital("Button 3", false);
        assert!(!st.keys()[2]);
        assert_eq!(st.digital_events(), 3);
    }

    #[test]
    fn the_log_is_bounded() {
        let mut st = Diagnostics::new();
        for _ in 0..(LOG_LINES * 3) {
            st.on_digital("Button 1", true);
        }
        assert_eq!(st.log_lines().count(), LOG_LINES);
    }

    #[test]
    fn latency_summary_is_empty_until_sampled() {
        let mut st = Diagnostics::new();
        assert_eq!(st.latency_summary(), "—");
        st.on_latency(4.0);
        st.on_latency(9.0);
        assert!(st.latency_summary().contains("n=2"));
    }
}
