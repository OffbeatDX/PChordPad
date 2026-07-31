pub const FADER_COUNT: usize = 2;

const SNAP: f32 = 0.005;

const DIR_ENTER: f32 = 0.5;
const DIR_LEAVE: f32 = 0.35;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FaderCfg {
    pub relative: bool,
    pub curve: f32,
    pub dead: f32,
    pub decay: f32,
    pub rel_travel: f32,
    pub speed_dead: f32,
}

impl Default for FaderCfg {
    fn default() -> Self {
        FaderCfg {
            relative: true,
            curve: 1.0,
            dead: 0.04,
            decay: 0.7,
            rel_travel: 100.0,
            speed_dead: 2.5,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Zone {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub center_x: f32,
    pub half: f32,
}

impl Zone {
    pub fn valid(&self) -> bool {
        self.w > 1.0 && self.h > 1.0 && self.half > 0.5
    }

    fn contains(&self, x: f32, y: f32) -> bool {
        self.valid() && x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

#[derive(Debug, Clone, Copy)]
struct Fader {
    owner: Option<u32>,
    settled: f32,
    value: f32,
    rel_value: f32,
    last_x: Option<f32>,
    dir: i32,
}

impl Default for Fader {
    fn default() -> Self {
        Fader {
            owner: None,
            settled: 0.0,
            value: 0.0,
            rel_value: 0.0,
            last_x: None,
            dir: 0,
        }
    }
}

impl Fader {
    fn recompute_dir(&mut self) {
        if self.value >= DIR_ENTER {
            self.dir = 1;
        } else if self.value <= -DIR_ENTER {
            self.dir = -1;
        } else if self.value.abs() < DIR_LEAVE {
            self.dir = 0;
        }
    }

    fn absolute(cfg: &FaderCfg, zone: &Zone, x: f32) -> f32 {
        let raw_t = ((x - zone.center_x) / zone.half.max(1.0)).clamp(-1.0, 1.0);
        let mag = raw_t.abs();
        let shaped = if mag < cfg.dead {
            0.0
        } else {
            ((mag - cfg.dead) / (1.0 - cfg.dead)).powf(cfg.curve)
        };
        if raw_t < 0.0 {
            -shaped
        } else {
            shaped
        }
    }

    fn advance_relative(&mut self, cfg: &FaderCfg, x: f32) {
        let Some(last) = self.last_x else {
            self.last_x = Some(x);
            return;
        };
        let delta = x - last;
        self.last_x = Some(x);
        if delta.abs() < cfg.speed_dead {
            return;
        }
        let reversing =
            (delta > 0.0 && self.rel_value < 0.0) || (delta < 0.0 && self.rel_value > 0.0);
        if self.rel_value != 0.0 && reversing {
            self.rel_value = 0.0;
        }
        let mut step = delta / cfg.rel_travel.max(1.0);
        if cfg.curve != 1.0 && step != 0.0 {
            step = step.signum() * step.abs().powf(cfg.curve);
        }
        self.rel_value = (self.rel_value + step).clamp(-1.0, 1.0);
    }

    fn drive(&mut self, cfg: &FaderCfg, zone: &Zone, x: f32) -> bool {
        let before = self.value;
        if cfg.relative {
            self.advance_relative(cfg, x);
            self.value = self.rel_value;
        } else {
            self.value = Self::absolute(cfg, zone, x);
        }
        self.settled = self.value;
        self.recompute_dir();
        self.value != before
    }

    fn grab(&mut self, cfg: &FaderCfg, id: u32) {
        self.owner = Some(id);
        self.last_x = None;
        if cfg.relative {
            self.rel_value = self.settled;
        }
    }

    fn release(&mut self) {
        self.owner = None;
        self.last_x = None;
    }

    fn decay(&mut self, cfg: &FaderCfg) -> bool {
        if self.owner.is_some() || self.settled == 0.0 {
            return false;
        }
        let before = self.value;
        self.settled = if self.settled.abs() < SNAP {
            0.0
        } else {
            self.settled * cfg.decay
        };
        self.value = self.settled;
        self.recompute_dir();
        self.value != before
    }
}

#[derive(Debug, Clone)]
pub struct Faders {
    faders: [Fader; FADER_COUNT],
    zones: [Zone; FADER_COUNT],
    cfg: FaderCfg,
}

impl Default for Faders {
    fn default() -> Self {
        Faders {
            faders: [Fader::default(); FADER_COUNT],
            zones: [Zone::default(); FADER_COUNT],
            cfg: FaderCfg::default(),
        }
    }
}

impl Faders {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_cfg(&mut self, cfg: FaderCfg) {
        self.cfg = cfg;
    }

    pub fn set_zones(&mut self, zones: [Zone; FADER_COUNT]) {
        self.zones = zones;
    }

    pub fn values(&self) -> [f32; FADER_COUNT] {
        [self.faders[0].value, self.faders[1].value]
    }

    pub fn grabbed(&self) -> [bool; FADER_COUNT] {
        [
            self.faders[0].owner.is_some(),
            self.faders[1].owner.is_some(),
        ]
    }

    pub fn dirs(&self) -> [i32; FADER_COUNT] {
        [self.faders[0].dir, self.faders[1].dir]
    }

    pub fn owns(&self, id: u32) -> bool {
        self.faders.iter().any(|f| f.owner == Some(id))
    }

    pub fn zone_hit(&self, x: f32, y: f32) -> bool {
        self.zones.iter().any(|z| z.contains(x, y))
    }

    pub fn offer(&mut self, id: u32, x: f32, y: f32) -> bool {
        let mut changed = false;
        for i in 0..FADER_COUNT {
            if self.faders[i].owner == Some(id) {
                changed |= self.faders[i].drive(&self.cfg, &self.zones[i], x);
                continue;
            }
            if self.faders[i].owner.is_none() && !self.owns(id) && self.zones[i].contains(x, y) {
                self.faders[i].grab(&self.cfg, id);
                changed |= self.faders[i].drive(&self.cfg, &self.zones[i], x);
            }
        }
        changed
    }

    pub fn release(&mut self, id: u32) {
        for f in &mut self.faders {
            if f.owner == Some(id) {
                f.release();
            }
        }
    }

    pub fn release_all(&mut self) {
        for f in &mut self.faders {
            f.release();
        }
    }

    pub fn decay_step(&mut self) -> bool {
        let mut changed = false;
        for f in &mut self.faders {
            changed |= f.decay(&self.cfg);
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abs_cfg() -> FaderCfg {
        FaderCfg {
            relative: false,
            curve: 1.0,
            dead: 0.04,
            decay: 0.7,
            rel_travel: 100.0,
            speed_dead: 2.5,
        }
    }

    fn zone() -> Zone {
        Zone {
            x: 0.0,
            y: 0.0,
            w: 300.0,
            h: 100.0,
            center_x: 150.0,
            half: 100.0,
        }
    }

    fn engine(cfg: FaderCfg) -> Faders {
        let mut f = Faders::new();
        f.set_cfg(cfg);
        f.set_zones([zone(), Zone::default()]);
        f
    }

    #[test]
    fn absolute_maps_position_through_deadzone_and_clamps() {
        let mut f = engine(abs_cfg());
        f.offer(1, 150.0, 50.0);
        assert_eq!(f.values()[0], 0.0);
        f.offer(1, 250.0, 50.0);
        assert!((f.values()[0] - 1.0).abs() < 1e-6);
        f.offer(1, 350.0, 50.0);
        assert!((f.values()[0] - 1.0).abs() < 1e-6);
        f.offer(1, 50.0, 50.0);
        assert!((f.values()[0] + 1.0).abs() < 1e-6);
    }

    #[test]
    fn absolute_curve_shapes_near_centre() {
        let mut cfg = abs_cfg();
        cfg.curve = 2.0;
        let mut f = engine(cfg);
        f.offer(1, 200.0, 50.0);
        assert!(f.values()[0] < 0.5, "got {}", f.values()[0]);
        assert!(f.values()[0] > 0.0);
    }

    #[test]
    fn a_contact_outside_the_zone_grabs_nothing() {
        let mut f = engine(abs_cfg());
        assert!(!f.offer(1, 150.0, 500.0));
        assert!(!f.grabbed()[0]);
        assert_eq!(f.values()[0], 0.0);
    }

    #[test]
    fn ownership_is_sticky_until_the_contact_lifts() {
        let mut f = engine(abs_cfg());
        f.offer(1, 250.0, 50.0);
        assert!(f.grabbed()[0]);
        f.offer(1, 200.0, 5000.0);
        assert!(f.grabbed()[0]);
        assert!(
            f.values()[0] > 0.4 && f.values()[0] < 1.0,
            "got {}",
            f.values()[0]
        );
        f.release(1);
        assert!(!f.grabbed()[0]);
    }

    #[test]
    fn a_second_contact_does_not_steal_an_owned_fader() {
        let mut f = engine(abs_cfg());
        f.offer(1, 180.0, 50.0);
        let v1 = f.values()[0];
        f.offer(2, 250.0, 50.0);
        assert!(f.owns(1));
        assert!(!f.owns(2));
        assert_eq!(f.values()[0], v1, "the intruder must not move the value");
    }

    #[test]
    fn released_fader_springs_back_to_rest() {
        let mut f = engine(abs_cfg());
        f.offer(1, 250.0, 50.0);
        assert!((f.values()[0] - 1.0).abs() < 1e-6);
        f.release(1);
        let mut steps = 0;
        while f.values()[0] != 0.0 && steps < 1000 {
            f.decay_step();
            steps += 1;
        }
        assert_eq!(f.values()[0], 0.0);
        assert!(steps > 1 && steps < 100, "decayed in {steps} steps");
    }

    #[test]
    fn an_owned_fader_does_not_decay() {
        let mut f = engine(abs_cfg());
        f.offer(1, 250.0, 50.0);
        assert!(!f.decay_step());
        assert!((f.values()[0] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn relative_accumulates_travel_and_holds_on_small_moves() {
        let cfg = FaderCfg {
            relative: true,
            rel_travel: 100.0,
            speed_dead: 2.5,
            ..abs_cfg()
        };
        let mut f = engine(cfg);
        f.offer(1, 150.0, 50.0);
        assert_eq!(f.values()[0], 0.0);
        f.offer(1, 151.0, 50.0);
        assert_eq!(f.values()[0], 0.0);
        f.offer(1, 201.0, 50.0);
        assert!((f.values()[0] - 0.5).abs() < 1e-6, "got {}", f.values()[0]);
    }

    #[test]
    fn relative_reversal_snaps_through_zero() {
        let cfg = FaderCfg {
            relative: true,
            rel_travel: 100.0,
            speed_dead: 2.5,
            ..abs_cfg()
        };
        let mut f = engine(cfg);
        f.offer(1, 150.0, 50.0);
        f.offer(1, 230.0, 50.0);
        assert!(f.values()[0] > 0.0);
        f.offer(1, 180.0, 50.0);
        assert!(f.values()[0] < 0.0, "got {}", f.values()[0]);
    }

    #[test]
    fn relative_resumes_from_settled_value_on_regrab() {
        let cfg = FaderCfg {
            relative: true,
            rel_travel: 100.0,
            speed_dead: 2.5,
            ..abs_cfg()
        };
        let mut f = engine(cfg);
        f.offer(1, 150.0, 50.0);
        f.offer(1, 200.0, 50.0);
        f.release(1);
        f.offer(2, 60.0, 50.0);
        assert!((f.values()[0] - 0.5).abs() < 1e-6, "got {}", f.values()[0]);
        f.offer(2, 160.0, 50.0);
        assert!((f.values()[0] - 1.0).abs() < 1e-6, "got {}", f.values()[0]);
    }

    #[test]
    fn release_all_drops_every_owner() {
        let mut f = engine(abs_cfg());
        f.set_zones([zone(), zone()]);
        f.offer(1, 250.0, 50.0);
        f.offer(2, 50.0, 50.0);
        assert!(f.grabbed()[0] && f.grabbed()[1]);
        f.release_all();
        assert!(!f.grabbed()[0] && !f.grabbed()[1]);
    }
}
