//! Animated lighting effects rendered on top of the base color.
//!
//! Effects are opt-in: [`Effect::Static`] (the default) reproduces the original
//! solid-color behaviour exactly, so a device that never selects an animation
//! sees no change. The animated variants compute a per-frame [`Frame`] that the
//! hardware backend paints through its existing, validated write path.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;

/// Animation applied to the base color.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    /// Solid base color (original behaviour).
    #[default]
    Static,
    /// Base color pulsing in brightness.
    Breathing,
    /// Hue sweeping through the wheel, same color on every LED.
    ColorCycle,
    /// Hue sweeping through the wheel with a per-LED offset (spatial rainbow).
    Rainbow,
    /// Hue mapped to CPU load (cyan idle → red under load).
    Load,
    /// Hue mapped to battery charge (red empty → green full).
    Battery,
}

impl Effect {
    /// Whether this is the plain solid-color mode (no animation, no daemon needed).
    pub(crate) fn is_static(&self) -> bool {
        matches!(self, Effect::Static)
    }

    /// Whether rendering this effect requires the persistent `run` loop.
    pub fn is_animated(&self) -> bool {
        !self.is_static()
    }

    /// Effects that read live system state need periodic re-rendering even when
    /// the visible color changes slowly; they set their own cadence.
    pub fn frame_interval(&self, fps: u32) -> f64 {
        match self {
            Effect::Static => 0.5,
            Effect::Load => 0.3,
            Effect::Battery => 2.0,
            _ => 1.0 / f64::from(fps.max(1)),
        }
    }
}

impl FromStr for Effect {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "static" => Ok(Effect::Static),
            "breathing" => Ok(Effect::Breathing),
            "color_cycle" => Ok(Effect::ColorCycle),
            "rainbow" => Ok(Effect::Rainbow),
            "load" => Ok(Effect::Load),
            "battery" => Ok(Effect::Battery),
            other => Err(format!(
                "unknown effect '{other}' (expected static, breathing, color_cycle, rainbow, load, battery)"
            )),
        }
    }
}

/// A single rendered frame: either one color for every LED or one per LED.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    Uniform([u8; 3]),
    PerTarget(Vec<[u8; 3]>),
}

impl Frame {
    /// Expand to exactly `count` colors (broadcasting a uniform frame, and
    /// clamping/padding a per-target frame so the backend always gets a full set).
    pub fn expand(&self, count: usize) -> Vec<[u8; 3]> {
        match self {
            Frame::Uniform(color) => vec![*color; count],
            Frame::PerTarget(colors) => {
                let fallback: [u8; 3] = colors.first().copied().unwrap_or([0, 0, 0]);
                (0..count)
                    .map(|index| colors.get(index).copied().unwrap_or(fallback))
                    .collect()
            }
        }
    }
}

/// Mutable state carried between frames (CPU delta, smoothed load).
pub struct EffectState {
    cpu: Option<CpuSample>,
    smooth_load: f32,
    battery_path: PathBuf,
    stat_path: PathBuf,
}

#[derive(Clone, Copy)]
struct CpuSample {
    idle: u64,
    total: u64,
}

impl Default for EffectState {
    fn default() -> Self {
        Self {
            cpu: None,
            smooth_load: 0.0,
            battery_path: std::env::var_os("ARMADA_RGB_POWER_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/sys/class/power_supply")),
            stat_path: std::env::var_os("ARMADA_RGB_STAT_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/proc/stat")),
        }
    }
}

impl EffectState {
    /// Render one frame for `effect` at time `t` (seconds) over `count` LEDs.
    /// `base` is the configured RGB color and `brightness` the configured
    /// percentage; the returned brightness may differ (e.g. breathing).
    pub fn render(
        &mut self,
        effect: Effect,
        base: [u8; 3],
        brightness: u8,
        speed: u16,
        t: f64,
        count: usize,
    ) -> (Frame, u8) {
        let rate: f64 = f64::from(speed.max(1)) / 100.0;
        match effect {
            Effect::Static => (Frame::Uniform(base), brightness),
            Effect::Breathing => {
                let period: f64 = 4.0 / rate;
                let phase: f64 = (f64::sin(2.0 * std::f64::consts::PI * t / period) + 1.0) / 2.0;
                // Never fully dark: floor at 8% of the configured brightness.
                let scaled: f64 = f64::from(brightness) * (0.08 + 0.92 * phase);
                (Frame::Uniform(base), scaled.round() as u8)
            }
            Effect::ColorCycle => {
                let hue: f64 = t * rate * 0.04;
                (Frame::Uniform(hsv_to_rgb(hue, 1.0, 1.0)), brightness)
            }
            Effect::Rainbow => {
                let span: usize = count.max(1);
                let colors: Vec<[u8; 3]> = (0..span)
                    .map(|index| {
                        let hue: f64 = t * rate * 0.04 + index as f64 / span as f64;
                        hsv_to_rgb(hue, 1.0, 1.0)
                    })
                    .collect();
                (Frame::PerTarget(colors), brightness)
            }
            Effect::Load => {
                let load: f32 = self.sample_load();
                self.smooth_load = self.smooth_load * 0.7 + load * 0.3;
                // Cyan (0.5) idle → red (0.0) under full load.
                let hue: f64 = 0.5 * f64::from(1.0 - self.smooth_load);
                (Frame::Uniform(hsv_to_rgb(hue, 1.0, 1.0)), brightness)
            }
            Effect::Battery => {
                let pct: f64 = self.read_battery().unwrap_or(100) as f64 / 100.0;
                // Red (0.0) empty → green (0.33) full.
                let hue: f64 = pct * 0.33;
                (Frame::Uniform(hsv_to_rgb(hue, 1.0, 1.0)), brightness)
            }
        }
    }

    /// Instantaneous CPU load in `0.0..=1.0` from the delta of `/proc/stat`.
    fn sample_load(&mut self) -> f32 {
        let Some(sample) = read_cpu_sample(&self.stat_path) else {
            return 0.0;
        };
        let load: f32 = match self.cpu {
            Some(prev) => {
                let idle_delta: u64 = sample.idle.saturating_sub(prev.idle);
                let total_delta: u64 = sample.total.saturating_sub(prev.total);
                if total_delta == 0 {
                    0.0
                } else {
                    (1.0 - idle_delta as f32 / total_delta as f32).clamp(0.0, 1.0)
                }
            }
            None => 0.0,
        };
        self.cpu = Some(sample);
        load
    }

    /// Battery charge percentage, if a battery power supply is present.
    fn read_battery(&self) -> Option<u8> {
        let entries = fs::read_dir(&self.battery_path).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            let kind = fs::read_to_string(path.join("type")).ok()?;
            if kind.trim() != "Battery" {
                continue;
            }
            if let Ok(capacity) = fs::read_to_string(path.join("capacity")) {
                if let Ok(value) = capacity.trim().parse::<u8>() {
                    return Some(value.min(100));
                }
            }
        }
        None
    }
}

fn read_cpu_sample(stat_path: &std::path::Path) -> Option<CpuSample> {
    let contents: String = fs::read_to_string(stat_path).ok()?;
    let line: &str = contents.lines().next()?;
    let mut fields = line.split_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    let values: Vec<u64> = fields.filter_map(|field| field.parse::<u64>().ok()).collect();
    if values.len() < 4 {
        return None;
    }
    // idle + iowait (index 3 + 4) count as idle time.
    let idle: u64 = values[3] + values.get(4).copied().unwrap_or(0);
    let total: u64 = values.iter().sum();
    Some(CpuSample { idle, total })
}

/// HSV (hue wrapped to `0..1`, s and v in `0..=1`) to 8-bit RGB.
fn hsv_to_rgb(hue: f64, saturation: f64, value: f64) -> [u8; 3] {
    let hue: f64 = hue.rem_euclid(1.0) * 6.0;
    let sector: f64 = hue.floor();
    let fractional: f64 = hue - sector;
    let p: f64 = value * (1.0 - saturation);
    let q: f64 = value * (1.0 - saturation * fractional);
    let t: f64 = value * (1.0 - saturation * (1.0 - fractional));
    let (r, g, b): (f64, f64, f64) = match sector as u32 % 6 {
        0 => (value, t, p),
        1 => (q, value, p),
        2 => (p, value, t),
        3 => (p, q, value),
        4 => (t, p, value),
        _ => (value, p, q),
    };
    [
        (r * 255.0).round() as u8,
        (g * 255.0).round() as u8,
        (b * 255.0).round() as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_effect_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&Effect::ColorCycle).unwrap(), "\"color_cycle\"");
        assert_eq!(serde_json::to_string(&Effect::Static).unwrap(), "\"static\"");
    }

    #[test]
    fn hsv_primaries() {
        assert_eq!(hsv_to_rgb(0.0, 1.0, 1.0), [255, 0, 0]); // red
        assert_eq!(hsv_to_rgb(1.0 / 3.0, 1.0, 1.0), [0, 255, 0]); // green
        assert_eq!(hsv_to_rgb(2.0 / 3.0, 1.0, 1.0), [0, 0, 255]); // blue
        assert_eq!(hsv_to_rgb(0.5, 1.0, 1.0), [0, 255, 255]); // cyan
        assert_eq!(hsv_to_rgb(1.0, 1.0, 1.0), [255, 0, 0]); // wraps back to red
    }

    #[test]
    fn breathing_stays_between_floor_and_ceiling() {
        let mut state = EffectState::default();
        let mut min = u8::MAX;
        let mut max = u8::MIN;
        for step in 0..200 {
            let (_frame, brightness) =
                state.render(Effect::Breathing, [255, 0, 0], 100, 100, step as f64 * 0.05, 8);
            min = min.min(brightness);
            max = max.max(brightness);
        }
        assert!(min >= 8, "breathing never goes fully dark (min={min})");
        assert!(max <= 100, "breathing never exceeds base brightness (max={max})");
    }

    #[test]
    fn rainbow_spreads_hues_across_targets() {
        let mut state = EffectState::default();
        let (frame, _) = state.render(Effect::Rainbow, [255, 255, 255], 50, 100, 0.0, 8);
        match frame {
            Frame::PerTarget(colors) => {
                assert_eq!(colors.len(), 8);
                assert!(colors.windows(2).any(|pair| pair[0] != pair[1]));
            }
            other => panic!("rainbow must be per-target, got {other:?}"),
        }
    }

    #[test]
    fn uniform_expands_and_pertarget_pads() {
        assert_eq!(Frame::Uniform([1, 2, 3]).expand(3), vec![[1, 2, 3]; 3]);
        let padded = Frame::PerTarget(vec![[9, 9, 9]]).expand(3);
        assert_eq!(padded, vec![[9, 9, 9], [9, 9, 9], [9, 9, 9]]);
    }

    #[test]
    fn static_renders_base_color_unchanged() {
        let mut state = EffectState::default();
        let (frame, brightness) = state.render(Effect::Static, [10, 20, 30], 42, 100, 123.0, 8);
        assert_eq!(frame, Frame::Uniform([10, 20, 30]));
        assert_eq!(brightness, 42);
    }
}
