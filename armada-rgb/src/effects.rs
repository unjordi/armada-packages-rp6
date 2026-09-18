//! Animated lighting effects rendered on top of the base color.
//!
//! Effects are opt-in: [`Effect::Static`] (the default) reproduces the original
//! solid-color behaviour exactly, so a device that never selects an animation
//! sees no change. The animated variants compute a per-frame [`Frame`] that the
//! hardware backend paints through its existing, validated write path.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::thread::sleep;
use std::time::{Duration, Instant};

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
    /// Base color, brightness scaled by the screen backlight percentage.
    BacklightSync,
    /// Per-side color sampled from the screen content (ambilight).
    ScreenSync,
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
            Effect::BacklightSync => 0.5,
            // Sampling + decoding a screenshot is real CPU/IO work compared to
            // a sysfs read; keep this effect's own cadence slow on purpose so
            // it cannot become a background thermal/CPU drain.
            Effect::ScreenSync => 3.0,
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
            "backlight_sync" => Ok(Effect::BacklightSync),
            "screen_sync" => Ok(Effect::ScreenSync),
            other => Err(format!(
                "unknown effect '{other}' (expected static, breathing, color_cycle, rainbow, \
                 load, battery, backlight_sync, screen_sync)"
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

/// Mutable state carried between frames (CPU delta, smoothed load, the last
/// successfully sampled backlight/screen colors so a transient read/capture
/// failure degrades to "keep showing the last good value" instead of
/// flickering to black).
pub struct EffectState {
    cpu: Option<CpuSample>,
    smooth_load: f32,
    battery_path: PathBuf,
    stat_path: PathBuf,
    backlight_root: PathBuf,
    backlight_name: Option<String>,
    backlight_warned: bool,
    screenshot_path: PathBuf,
    gamescopectl_bin: String,
    gamescope_runtime_dir: String,
    gamescope_wayland_display: String,
    /// Optional `su - <user> -c '...'` wrapper: `armada-rgb run` is a system
    /// service with no graphical session env of its own (see the module doc
    /// on `Effect::ScreenSync`); if setting `XDG_RUNTIME_DIR`/`WAYLAND_DISPLAY`
    /// directly is not enough on some device, this switches to the session
    /// user without a rebuild.
    screen_sync_user: Option<String>,
    last_left: [u8; 3],
    last_right: [u8; 3],
    screen_sync_warned: bool,
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
            backlight_root: std::env::var_os("ARMADA_RGB_BACKLIGHT_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/sys/class/backlight")),
            backlight_name: std::env::var("ARMADA_RGB_BACKLIGHT_NAME").ok(),
            backlight_warned: false,
            screenshot_path: std::env::var_os("ARMADA_RGB_SCREENSHOT_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/run/armada-rgb/screen-sync.png")),
            gamescopectl_bin: std::env::var("ARMADA_RGB_GAMESCOPECTL_BIN")
                .unwrap_or_else(|_| "gamescopectl".into()),
            gamescope_runtime_dir: std::env::var("ARMADA_RGB_GAMESCOPE_XDG_RUNTIME_DIR")
                .unwrap_or_else(|_| "/run/user/1000".into()),
            gamescope_wayland_display: std::env::var("ARMADA_RGB_GAMESCOPE_WAYLAND_DISPLAY")
                .unwrap_or_else(|_| "gamescope-0".into()),
            screen_sync_user: std::env::var("ARMADA_RGB_SCREEN_SYNC_USER").ok(),
            last_left: [0, 0, 0],
            last_right: [0, 0, 0],
            screen_sync_warned: false,
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
            Effect::BacklightSync => {
                // The configured brightness is the ceiling; the screen's own
                // percentage scales it down from there, so dimming the
                // screen dims the LEDs proportionally instead of replacing
                // the user's brightness choice outright.
                let pct: f64 = self.sample_backlight_pct();
                let scaled: u8 = (f64::from(brightness) * pct).round() as u8;
                (Frame::Uniform(base), scaled.min(100))
            }
            Effect::ScreenSync => {
                let colors: Vec<[u8; 3]> = self.sample_screen_colors(count);
                (Frame::PerTarget(colors), brightness)
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

    /// Screen backlight as a `0.0..=1.0` fraction. Falls back to `1.0`
    /// (unscaled — the configured brightness applies as-is) if no backlight
    /// device can be resolved, so a missing/renamed node dims nothing rather
    /// than going dark. Warns once (not every tick) with the exact env vars
    /// to set if this is not a transient boot-time race.
    fn sample_backlight_pct(&mut self) -> f64 {
        match resolve_backlight_dir(&self.backlight_root, self.backlight_name.as_deref()) {
            Some(dir) => {
                let brightness: Option<u32> = read_u32(&dir.join("brightness"));
                let maximum: Option<u32> = read_u32(&dir.join("max_brightness"));
                match (brightness, maximum) {
                    (Some(brightness), Some(maximum)) if maximum > 0 => {
                        self.backlight_warned = false;
                        (f64::from(brightness) / f64::from(maximum)).clamp(0.0, 1.0)
                    }
                    _ => {
                        self.warn_backlight_once(&format!(
                            "could not read brightness/max_brightness under {}",
                            dir.display()
                        ));
                        1.0
                    }
                }
            }
            None => {
                self.warn_backlight_once(&format!(
                    "no backlight device found under {} (set ARMADA_RGB_BACKLIGHT_NAME if there is \
                     more than one and the wrong one is picked, or ARMADA_RGB_BACKLIGHT_ROOT if it \
                     lives elsewhere on this device)",
                    self.backlight_root.display()
                ));
                1.0
            }
        }
    }

    fn warn_backlight_once(&mut self, reason: &str) {
        if !self.backlight_warned {
            eprintln!("armada-rgb: backlight_sync {reason}; brightness left unscaled");
            self.backlight_warned = true;
        }
    }

    /// Per-side (left/right) average color of the current screen contents,
    /// spread across `count` targets (first half = left average, second half
    /// = right average). On any capture failure, keeps returning the last
    /// successfully sampled colors (black before the first successful
    /// capture) instead of flickering — see [`Effect::ScreenSync`].
    fn sample_screen_colors(&mut self, count: usize) -> Vec<[u8; 3]> {
        match self.capture_screen_split() {
            Ok((left, right)) => {
                self.last_left = left;
                self.last_right = right;
                self.screen_sync_warned = false;
            }
            Err(reason) => {
                if !self.screen_sync_warned {
                    eprintln!(
                        "armada-rgb: screen_sync capture failed ({reason}); keeping the last \
                         known colors. If this is not transient (e.g. no graphical session yet), \
                         check ARMADA_RGB_GAMESCOPECTL_BIN, ARMADA_RGB_GAMESCOPE_XDG_RUNTIME_DIR, \
                         ARMADA_RGB_GAMESCOPE_WAYLAND_DISPLAY, and ARMADA_RGB_SCREEN_SYNC_USER."
                    );
                    self.screen_sync_warned = true;
                }
            }
        }
        let left_count: usize = count / 2;
        (0..count)
            .map(|index| if index < left_count { self.last_left } else { self.last_right })
            .collect()
    }

    fn capture_screen_split(&self) -> Result<([u8; 3], [u8; 3]), String> {
        if let Some(parent) = self.screenshot_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
        // Avoid reading a stale screenshot if this capture fails to produce
        // a fresh one (e.g. gamescopectl runs but the compositor is busy).
        let _ = fs::remove_file(&self.screenshot_path);

        run_screenshot_command(
            &self.gamescopectl_bin,
            &self.screenshot_path,
            &self.gamescope_runtime_dir,
            &self.gamescope_wayland_display,
            self.screen_sync_user.as_deref(),
        )?;

        let image = image::open(&self.screenshot_path)
            .map_err(|error| format!("decode {}: {error}", self.screenshot_path.display()))?
            .to_rgb8();
        Ok(average_left_right(&image))
    }
}

/// Resolve which `/sys/class/backlight/*` device is the real screen panel.
/// `preferred` (from `ARMADA_RGB_BACKLIGHT_NAME`) is authoritative when set —
/// an explicit override that does not exist fails outright rather than
/// silently falling back to a guess. Otherwise, among the devices found,
/// prefer one whose name is not the literal `backlight` (the generic
/// `pwm-backlight` wrapper node Linux exposes on some panels alongside the
/// panel's own, more specific node — e.g. the RP6 exposes both
/// `ae94000.dsi.0` and a generic `backlight`; the named one is the panel).
fn resolve_backlight_dir(root: &Path, preferred: Option<&str>) -> Option<PathBuf> {
    if let Some(name) = preferred {
        let candidate: PathBuf = root.join(name);
        return if candidate.join("brightness").is_file() && candidate.join("max_brightness").is_file() {
            Some(candidate)
        } else {
            None
        };
    }

    let mut entries: Vec<PathBuf> = fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.join("brightness").is_file() && path.join("max_brightness").is_file())
        .collect();
    if entries.is_empty() {
        return None;
    }
    entries.sort();
    entries
        .iter()
        .find(|path| path.file_name().and_then(|name| name.to_str()) != Some("backlight"))
        .or_else(|| entries.first())
        .cloned()
}

fn read_u32(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Run `<bin> screenshot <path>` (or, if `su_user` is set, the same command
/// through `su - <user> -c '...'`) with the Wayland session env explicitly
/// set — `armada-rgb run` is a system service with no graphical session env
/// of its own, so it cannot rely on inherited environment variables. Bounded
/// by a hard timeout so a wedged compositor cannot hang the whole daemon
/// (config reload and every other effect share this same loop thread).
fn run_screenshot_command(
    bin: &str,
    path: &Path,
    xdg_runtime_dir: &str,
    wayland_display: &str,
    su_user: Option<&str>,
) -> Result<(), String> {
    const TIMEOUT: Duration = Duration::from_millis(1500);
    const POLL: Duration = Duration::from_millis(50);

    let mut command: Command = match su_user {
        Some(user) => {
            let mut command = Command::new("su");
            command.arg("-").arg(user).arg("-c").arg(format!(
                "env XDG_RUNTIME_DIR={xdg_runtime_dir} WAYLAND_DISPLAY={wayland_display} {bin} screenshot {}",
                path.display()
            ));
            command
        }
        None => {
            let mut command = Command::new(bin);
            command
                .arg("screenshot")
                .arg(path)
                .env("XDG_RUNTIME_DIR", xdg_runtime_dir)
                .env("WAYLAND_DISPLAY", wayland_display);
            command
        }
    };
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());

    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn {bin}: {error}"))?;

    let deadline: Instant = Instant::now() + TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("{bin} screenshot timed out after {TIMEOUT:?}"));
                }
                sleep(POLL);
            }
            Err(error) => return Err(format!("wait for {bin}: {error}")),
        }
    }

    // The compositor writes the file asynchronously; give it a brief moment
    // to appear rather than failing on the exact same tick the process exits.
    let flush_deadline: Instant = Instant::now() + Duration::from_millis(500);
    while !path.exists() {
        if Instant::now() >= flush_deadline {
            return Err(format!("{} never appeared", path.display()));
        }
        sleep(POLL);
    }
    Ok(())
}

/// Average RGB of the left and right halves of the image, sampled on a
/// bounded grid (never more than ~64 samples per axis) so decoding a large
/// screenshot cannot itself become the CPU cost this effect is gated against.
fn average_left_right(image: &image::RgbImage) -> ([u8; 3], [u8; 3]) {
    let (width, height): (u32, u32) = image.dimensions();
    if width == 0 || height == 0 {
        return ([0, 0, 0], [0, 0, 0]);
    }
    let mid: u32 = width / 2;
    let step: u32 = (width.max(height) / 64).max(1);

    let mut left_sum: [u64; 3] = [0; 3];
    let mut left_n: u64 = 0;
    let mut right_sum: [u64; 3] = [0; 3];
    let mut right_n: u64 = 0;

    let mut y: u32 = 0;
    while y < height {
        let mut x: u32 = 0;
        while x < width {
            let pixel = image.get_pixel(x, y);
            let (sum, n): (&mut [u64; 3], &mut u64) =
                if x < mid { (&mut left_sum, &mut left_n) } else { (&mut right_sum, &mut right_n) };
            sum[0] += u64::from(pixel[0]);
            sum[1] += u64::from(pixel[1]);
            sum[2] += u64::from(pixel[2]);
            *n += 1;
            x += step;
        }
        y += step;
    }
    (average(left_sum, left_n), average(right_sum, right_n))
}

fn average(sum: [u64; 3], n: u64) -> [u8; 3] {
    if n == 0 {
        return [0, 0, 0];
    }
    [(sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8]
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

    // -- backlight_sync -----------------------------------------------------

    fn fixture_dir(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir: PathBuf = std::env::temp_dir().join(format!(
            "armada-rgb-effects-test-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn backlight_device(root: &Path, name: &str, brightness: u32, maximum: u32) {
        let dir: PathBuf = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("brightness"), format!("{brightness}\n")).unwrap();
        fs::write(dir.join("max_brightness"), format!("{maximum}\n")).unwrap();
    }

    #[test]
    fn backlight_sync_scales_brightness_by_screen_percent() {
        let root: PathBuf = fixture_dir("backlight-single");
        backlight_device(&root, "panel.dsi.0", 50, 100);

        let mut state: EffectState = EffectState {
            backlight_root: root,
            backlight_name: None,
            ..EffectState::default()
        };
        let (frame, brightness) =
            state.render(Effect::BacklightSync, [255, 255, 255], 80, 100, 0.0, 8);
        assert_eq!(frame, Frame::Uniform([255, 255, 255]));
        assert_eq!(brightness, 40); // 80% configured ceiling * 50% backlight
    }

    #[test]
    fn backlight_sync_prefers_named_panel_over_generic_alias() {
        // Mirrors the RP6, which exposes both a generic "backlight" wrapper
        // and the panel's own named node; the named one must win by default.
        let root: PathBuf = fixture_dir("backlight-ambiguous");
        backlight_device(&root, "backlight", 10, 100);
        backlight_device(&root, "panel.dsi.0", 90, 100);

        let mut state: EffectState = EffectState {
            backlight_root: root,
            backlight_name: None,
            ..EffectState::default()
        };
        let (_frame, brightness) = state.render(Effect::BacklightSync, [1, 1, 1], 100, 100, 0.0, 8);
        assert_eq!(brightness, 90);
    }

    #[test]
    fn backlight_sync_honors_explicit_name_override() {
        let root: PathBuf = fixture_dir("backlight-override");
        backlight_device(&root, "backlight", 10, 100);
        backlight_device(&root, "panel.dsi.0", 90, 100);

        let mut state: EffectState = EffectState {
            backlight_root: root,
            backlight_name: Some("backlight".into()),
            ..EffectState::default()
        };
        let (_frame, brightness) = state.render(Effect::BacklightSync, [1, 1, 1], 100, 100, 0.0, 8);
        assert_eq!(brightness, 10);
    }

    #[test]
    fn backlight_sync_falls_back_to_configured_brightness_when_missing() {
        let root: PathBuf = fixture_dir("backlight-missing"); // left empty
        let mut state: EffectState = EffectState {
            backlight_root: root,
            backlight_name: None,
            ..EffectState::default()
        };
        let (_frame, brightness) = state.render(Effect::BacklightSync, [1, 1, 1], 55, 100, 0.0, 8);
        assert_eq!(brightness, 55);
    }

    // -- screen_sync ----------------------------------------------------------

    fn write_fake_gamescopectl(root: &Path, fixture_png: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script: PathBuf = root.join("fake-gamescopectl.sh");
        fs::write(
            &script,
            format!("#!/bin/sh\ncp '{}' \"$2\"\n", fixture_png.display()),
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();
        script
    }

    fn write_split_fixture(root: &Path, left: [u8; 3], right: [u8; 3]) -> PathBuf {
        let path: PathBuf = root.join("fixture.png");
        let mut fixture = image::RgbImage::new(4, 4);
        for y in 0..4 {
            for x in 0..4 {
                let color = if x < 2 { left } else { right };
                fixture.put_pixel(x, y, image::Rgb(color));
            }
        }
        fixture.save(&path).unwrap();
        path
    }

    #[test]
    fn screen_sync_averages_left_and_right_halves() {
        let root: PathBuf = fixture_dir("screen-sync-ok");
        let fixture_png: PathBuf = write_split_fixture(&root, [255, 0, 0], [0, 0, 255]);
        let script: PathBuf = write_fake_gamescopectl(&root, &fixture_png);

        let mut state: EffectState = EffectState {
            gamescopectl_bin: script.to_string_lossy().into_owned(),
            screenshot_path: root.join("out.png"),
            screen_sync_user: None,
            ..EffectState::default()
        };
        let (frame, _brightness) = state.render(Effect::ScreenSync, [0, 0, 0], 100, 100, 0.0, 8);
        match frame {
            Frame::PerTarget(colors) => {
                assert_eq!(colors.len(), 8);
                assert_eq!(colors[0], [255, 0, 0], "left ring should follow the left half");
                assert_eq!(colors[7], [0, 0, 255], "right ring should follow the right half");
            }
            other => panic!("screen_sync must be per-target, got {other:?}"),
        }
    }

    #[test]
    fn screen_sync_falls_back_to_last_colors_then_recovers() {
        let root: PathBuf = fixture_dir("screen-sync-fallback");
        let fixture_png: PathBuf = write_split_fixture(&root, [10, 20, 30], [40, 50, 60]);
        let script: PathBuf = write_fake_gamescopectl(&root, &fixture_png);

        let mut state: EffectState = EffectState {
            gamescopectl_bin: root.join("does-not-exist").to_string_lossy().into_owned(),
            screenshot_path: root.join("out.png"),
            screen_sync_user: None,
            ..EffectState::default()
        };
        let (frame, _) = state.render(Effect::ScreenSync, [0, 0, 0], 100, 100, 0.0, 4);
        assert_eq!(
            frame.expand(4),
            vec![[0, 0, 0]; 4],
            "before any successful capture, colors default to off"
        );

        state.gamescopectl_bin = script.to_string_lossy().into_owned();
        let (frame, _) = state.render(Effect::ScreenSync, [0, 0, 0], 100, 100, 0.0, 4);
        assert_eq!(frame.expand(4), vec![[10, 20, 30], [10, 20, 30], [40, 50, 60], [40, 50, 60]]);
    }
}
