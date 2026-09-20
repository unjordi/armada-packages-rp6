//! Animated lighting effects rendered on top of the base color.
//!
//! Effects are opt-in: [`Effect::Static`] (the default) reproduces the original
//! solid-color behaviour exactly, so a device that never selects an animation
//! sees no change. The animated variants compute a per-frame [`Frame`] that the
//! hardware backend paints through its existing, validated write path.

use serde::{Deserialize, Serialize};
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::thread::sleep;
use std::time::{Duration, Instant};

// -- screen_sync (ambilight) efficiency knobs ------------------------------
//
// gamescope's screenshot IPC (`gamescope-control.xml`, request
// `take_screenshot(path, type, flags)`) offers NO region, crop, or
// resolution/downscale argument: `type` only picks base_plane_only /
// all_real_layers / full_composition / screen_buffer, `flags` is a single
// dummy bit, and the capture is always the full output resolution
// (upstream feature request ValveSoftware/gamescope#284 is still open).
// The written formats are png / avif / raw nv12.bin only -- no bmp.
//
// So the compositor always reads back the whole frame; the levers we DO have
// are (a) not paying for a PNG encode+decode round-trip, and (b) only reading
// the outer edge bands we actually need:
//
//   1. RAW NV12 capture -- request `<path>.nv12.bin` instead of `.png`. The
//      extension is what tells gamescope which encoder to use, and NV12 is
//      the compositor's native pixel format, so it writes the buffer with NO
//      PNG encode (measured on the RP6: NV12 ~261ms wall vs PNG ~646ms), and
//      WE never decode a 2 MP PNG -- we read luma/chroma straight out of the
//      raw planes. This is the fix for Jordi's "1.6 MB per live-sync is too
//      much": the cost was the PNG encode + the `image` 2 MP decode, both now
//      gone (the `image` crate dependency was dropped entirely).
//   2. EDGE-ONLY sampling -- we only touch the outer edge band on each side,
//      on a bounded grid, which is the correct ambilight source for the
//      left/right stick LEDs and keeps per-tick CPU trivial.
//   3. CADENCE -- capture at a slow cadence so even the compositor-side
//      readback stays a negligible background cost.
//
// NV12 layout (measured in Game Mode, 2026-09-20, RP6 gamescope output
// 1920x1080): the file is the raw GPU buffer. The Y (luma) plane is
// `width` bytes per row (no per-row padding was observed) for `height` rows;
// the interleaved U/V (chroma, 2x2-subsampled) plane follows at a
// PAGE-ALIGNED offset. For 1920x1080: Y = 1920*1080 = 2_073_600 rounded up to
// a 4096-byte page = 2_076_672; UV = 1920*540 = 1_036_800 rounded up =
// 1_040_384; total = 3_117_056 bytes (the exact size gamescope writes). The
// resolution can change (res switch / external display), so the geometry is
// validated against the actual file size before sampling and, on any
// mismatch, the caller falls back to the base color (QG-2) rather than
// sampling garbage -- see `Nv12Layout::resolve` and `capture_screen_split`.

/// screen_sync capture cadence, in seconds. Now that each tick avoids the PNG
/// encode+decode, the per-tick cost is dominated by the compositor's frame
/// readback; 3s keeps the effect a negligible background cost. It is now cheap
/// enough to lower for a snappier ambilight -- the visual feel is a
/// physical-QA call, so this stays a tunable constant rather than a config
/// field.
const SCREEN_SYNC_INTERVAL_SECS: f64 = 3.0;

/// Fraction of the frame width sampled at EACH side for the ambilight
/// average. gamescope cannot capture a region (see the module note), so the
/// full frame is written; this bounds how much of it we *read* -- the outer
/// ~8% of each side (approximately the panel border, ~154px on a 1920-wide
/// frame). Tunable; the feel is a physical-QA call.
const SCREEN_SYNC_EDGE_FRACTION: f64 = 0.08;

/// Default composited output width / height in pixels (gamescope's Game Mode
/// output on the RP6, measured 2026-09-20). Overridable via
/// `ARMADA_RGB_SCREEN_WIDTH` / `ARMADA_RGB_SCREEN_HEIGHT` if the resolution
/// differs on some device/dock, without a rebuild. Only used to interpret the
/// raw NV12 buffer; a wrong value makes the size check fail -> base-color
/// fallback, never garbage LEDs.
const SCREEN_WIDTH_DEFAULT: u32 = 1920;
const SCREEN_HEIGHT_DEFAULT: u32 = 1080;

/// Byte alignment of each NV12 plane in the buffer gamescope hands back
/// (page-aligned = 4096 on the RP6). Overridable via
/// `ARMADA_RGB_NV12_PLANE_ALIGN`. `resolve` also accepts a tightly-packed
/// buffer, so this only needs to be right for the page-aligned case.
const NV12_PLANE_ALIGN_DEFAULT: usize = 4096;

/// Hard timeout for a single screenshot capture. Kept comfortably under
/// `SCREEN_SYNC_INTERVAL_SECS` so a slow capture just delays the next tick
/// instead of two ever running back-to-back with zero gap.
const SCREEN_SYNC_CAPTURE_TIMEOUT: Duration = Duration::from_millis(2500);

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
            // Sampling + decoding a screenshot is real CPU/IO work compared to
            // a sysfs read; keep this effect's own cadence slow on purpose so
            // it cannot become a background thermal/CPU drain (see the module
            // note on gamescope's capture limits).
            Effect::ScreenSync => SCREEN_SYNC_INTERVAL_SECS,
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
            "screen_sync" => Ok(Effect::ScreenSync),
            other => Err(format!(
                "unknown effect '{other}' (expected static, breathing, color_cycle, rainbow, \
                 load, battery, screen_sync)"
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

/// Mutable state carried between frames (CPU delta, smoothed load, backlight
/// resolution for the `sync_brightness` modifier, and the last successfully
/// sampled screen colors so a transient capture failure degrades to "keep
/// showing the last good value" instead of flickering to black).
pub struct EffectState {
    cpu: Option<CpuSample>,
    smooth_load: f32,
    battery_path: PathBuf,
    stat_path: PathBuf,
    backlight_root: PathBuf,
    backlight_name: Option<String>,
    backlight_warned: bool,
    /// Separate from `backlight_warned` on purpose: ambiguity (more than one
    /// candidate device) is a STATIC fact about the device, so it must warn
    /// exactly once ever, not "once per failure streak" — a successful
    /// brightness read (which resets `backlight_warned`) does not mean the
    /// ambiguity went away.
    backlight_ambiguity_warned: bool,
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
    /// Composited output geometry, used to interpret the raw NV12 capture.
    /// A wrong value fails the size check in `Nv12Layout::resolve` and falls
    /// back to the base color rather than sampling garbage.
    screen_width: u32,
    screen_height: u32,
    nv12_plane_align: usize,
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
            backlight_ambiguity_warned: false,
            screenshot_path: std::env::var_os("ARMADA_RGB_SCREENSHOT_PATH")
                .map(PathBuf::from)
                // `.nv12.bin` is what tells gamescope to write the raw NV12
                // buffer (no PNG encode). tmpfs to avoid wearing flash.
                .unwrap_or_else(|| PathBuf::from("/run/armada-rgb/screen-sync.nv12.bin")),
            gamescopectl_bin: std::env::var("ARMADA_RGB_GAMESCOPECTL_BIN")
                .unwrap_or_else(|_| "gamescopectl".into()),
            gamescope_runtime_dir: std::env::var("ARMADA_RGB_GAMESCOPE_XDG_RUNTIME_DIR")
                .unwrap_or_else(|_| "/run/user/1000".into()),
            gamescope_wayland_display: std::env::var("ARMADA_RGB_GAMESCOPE_WAYLAND_DISPLAY")
                .unwrap_or_else(|_| "gamescope-0".into()),
            screen_sync_user: std::env::var("ARMADA_RGB_SCREEN_SYNC_USER").ok(),
            screen_width: env_u32("ARMADA_RGB_SCREEN_WIDTH", SCREEN_WIDTH_DEFAULT),
            screen_height: env_u32("ARMADA_RGB_SCREEN_HEIGHT", SCREEN_HEIGHT_DEFAULT),
            nv12_plane_align: std::env::var("ARMADA_RGB_NV12_PLANE_ALIGN")
                .ok()
                .and_then(|value| value.trim().parse::<usize>().ok())
                .filter(|value| *value >= 1)
                .unwrap_or(NV12_PLANE_ALIGN_DEFAULT),
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
                let (pct, status): (f64, Option<String>) = match self.read_battery() {
                    Some(capacity) => {
                        let status: Option<String> = self.read_battery_status();
                        (capacity as f64 / 100.0, status)
                    }
                    None => (1.0, None),
                };
                let color: [u8; 3] = match status.as_deref() {
                    Some("Charging") => hsv_to_rgb(0.11, 1.0, 1.0), // amber: charging
                    Some("Full") => hsv_to_rgb(0.33, 1.0, 1.0),     // green: full
                    _ => {
                        // Discharging / unknown: red (0.0) empty → green (0.33) full.
                        hsv_to_rgb(pct * 0.33, 1.0, 1.0)
                    }
                };
                (Frame::Uniform(color), brightness)
            }
            Effect::ScreenSync => {
                let colors: Vec<[u8; 3]> = self.sample_screen_colors(count, base);
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

    /// Battery charge status (`Charging` / `Full` / `Discharging` / …) from
    /// the same power_supply node, if readable. QG-3: the daemon disarms the
    /// kernel charging trigger on every write (see `reclaim_led` in
    /// `backend.rs`), so the LED cannot rely on the kernel to paint the
    /// charge state — the effect reads `status` itself and paints amber
    /// (charging), green (full), or the capacity gradient (discharging).
    fn read_battery_status(&self) -> Option<String> {
        let entries = fs::read_dir(&self.battery_path).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            let kind = fs::read_to_string(path.join("type")).ok()?;
            if kind.trim() != "Battery" {
                continue;
            }
            if let Ok(status) = fs::read_to_string(path.join("status")) {
                return Some(status.trim().to_string());
            }
        }
        None
    }

    /// Scale `brightness` (whatever the active effect already computed —
    /// this runs in the shared write path, not inside any one effect) by the
    /// live screen backlight percentage when `sync_brightness` is enabled.
    /// This is a MODIFIER on top of the current color/effect, not a
    /// replacement for either — see `LightingConfig::sync_brightness`.
    /// A no-op (returns `brightness` unchanged) when the modifier is off.
    pub(crate) fn scale_for_sync(&mut self, brightness: u8, sync_brightness: bool) -> u8 {
        if !sync_brightness {
            return brightness;
        }
        let pct: f64 = self.sample_backlight_pct();
        // QG-5: the LED panel reads ~12% brighter than the screen panel at
        // the same backlight percentage, so scale the LED brightness down by
        // 12% (×0.88) to match the perceived screen brightness.
        (f64::from(brightness) * pct * 0.88).round().min(100.0) as u8
    }

    /// Screen backlight as a `0.0..=1.0` fraction. Falls back to `1.0`
    /// (unscaled — the configured brightness applies as-is) if no backlight
    /// device can be resolved, so a missing/renamed node dims nothing rather
    /// than going dark. Warns once (not every tick) with the exact env vars
    /// to set if this is not a transient boot-time race.
    fn sample_backlight_pct(&mut self) -> f64 {
        let (resolved, ambiguity): (Option<PathBuf>, Option<String>) =
            resolve_backlight_dir(&self.backlight_root, self.backlight_name.as_deref());
        if let Some(reason) = ambiguity {
            if !self.backlight_ambiguity_warned {
                eprintln!("armada-rgb: sync_brightness {reason}");
                self.backlight_ambiguity_warned = true;
            }
        }
        match resolved {
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
            eprintln!("armada-rgb: sync_brightness {reason}; brightness left unscaled");
            self.backlight_warned = true;
        }
    }

    /// Per-side (left/right) average color of the current screen EDGES,
    /// spread across `count` targets (first half = left edge average, second
    /// half = right edge average). On any capture failure, keeps returning the last
    /// successfully sampled colors (or the configured `base` color before the
    /// first successful capture) instead of flickering to black — see
    /// [`Effect::ScreenSync`]. QG-2: a persistent capture failure must never
    /// leave the LEDs dark; falling back to `base` keeps them visible.
    fn sample_screen_colors(&mut self, count: usize, base: [u8; 3]) -> Vec<[u8; 3]> {
        match self.capture_screen_split() {
            Ok((left, right)) => {
                self.last_left = left;
                self.last_right = right;
                self.screen_sync_warned = false;
            }
            Err(reason) => {
                if !self.screen_sync_warned {
                    eprintln!(
                        "armada-rgb: screen_sync capture failed ({reason}); falling back to the \
                         configured base color. If this is not transient (e.g. no graphical \
                         session yet), check ARMADA_RGB_GAMESCOPECTL_BIN, \
                         ARMADA_RGB_GAMESCOPE_XDG_RUNTIME_DIR, ARMADA_RGB_GAMESCOPE_WAYLAND_DISPLAY, \
                         and ARMADA_RGB_SCREEN_SYNC_USER."
                    );
                    self.screen_sync_warned = true;
                }
                // QG-2: never paint black on capture failure — use the base color
                // so the LEDs stay visible.
                self.last_left = base;
                self.last_right = base;
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

        // Read the raw NV12 buffer -- no PNG decode. `/run` is tmpfs, so this
        // read is cheap; the win over the old path is skipping the 2 MP PNG
        // decode (and the compositor's PNG encode).
        let raw: Vec<u8> = fs::read(&self.screenshot_path)
            .map_err(|error| format!("read {}: {error}", self.screenshot_path.display()))?;
        let layout: Nv12Layout =
            Nv12Layout::resolve(raw.len(), self.screen_width, self.screen_height, self.nv12_plane_align)
                .ok_or_else(|| {
                    format!(
                        "NV12 size {} does not match {}x{} (plane align {}); set \
                         ARMADA_RGB_SCREEN_WIDTH/HEIGHT if the output resolution changed",
                        raw.len(),
                        self.screen_width,
                        self.screen_height,
                        self.nv12_plane_align,
                    )
                })?;
        Ok(average_edges_nv12(&raw, &layout, SCREEN_SYNC_EDGE_FRACTION))
    }

    /// One instrumented capture for the `screen-sync-probe` diagnostic /
    /// benchmark. Mirrors [`capture_screen_split`] but times the compositor
    /// capture and the read+sample separately, and reports the resolved
    /// geometry and sampled colors, so the effect's cost and correctness can
    /// be measured on the device without guessing.
    pub fn probe_screen_sync(&self) -> Result<ScreenSyncProbe, String> {
        if let Some(parent) = self.screenshot_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
        let _ = fs::remove_file(&self.screenshot_path);

        let capture_start: Instant = Instant::now();
        run_screenshot_command(
            &self.gamescopectl_bin,
            &self.screenshot_path,
            &self.gamescope_runtime_dir,
            &self.gamescope_wayland_display,
            self.screen_sync_user.as_deref(),
        )?;
        let capture_ms: f64 = capture_start.elapsed().as_secs_f64() * 1000.0;

        let process_start: Instant = Instant::now();
        let raw: Vec<u8> = fs::read(&self.screenshot_path)
            .map_err(|error| format!("read {}: {error}", self.screenshot_path.display()))?;
        let layout: Nv12Layout =
            Nv12Layout::resolve(raw.len(), self.screen_width, self.screen_height, self.nv12_plane_align)
                .ok_or_else(|| {
                    format!(
                        "NV12 size {} does not match {}x{} (plane align {})",
                        raw.len(),
                        self.screen_width,
                        self.screen_height,
                        self.nv12_plane_align,
                    )
                })?;
        let (left, right): ([u8; 3], [u8; 3]) =
            average_edges_nv12(&raw, &layout, SCREEN_SYNC_EDGE_FRACTION);
        let process_ms: f64 = process_start.elapsed().as_secs_f64() * 1000.0;

        Ok(ScreenSyncProbe {
            capture_ms,
            process_ms,
            bytes: raw.len(),
            width: layout.width,
            height: layout.height,
            uv_offset: layout.uv_offset,
            left,
            right,
        })
    }
}

/// Timing + result of a single instrumented `screen_sync` capture, for the
/// `screen-sync-probe` diagnostic subcommand.
#[derive(Clone, Copy, Debug)]
pub struct ScreenSyncProbe {
    /// Wall time of the compositor capture (`gamescopectl screenshot`), ms.
    pub capture_ms: f64,
    /// Wall time to read the raw buffer and sample the edges, ms (no decode).
    pub process_ms: f64,
    /// Size of the raw NV12 buffer read.
    pub bytes: usize,
    pub width: u32,
    pub height: u32,
    pub uv_offset: usize,
    pub left: [u8; 3],
    pub right: [u8; 3],
}

/// Read a `u32` env var, falling back to `default` if unset or unparseable.
fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .unwrap_or(default)
}

/// Round `value` up to the next multiple of `align` (`align <= 1` is a no-op).
fn align_up(value: usize, align: usize) -> usize {
    if align <= 1 {
        return value;
    }
    // Avoid overflow on the `+ align - 1` for pathological inputs.
    match value.checked_add(align - 1) {
        Some(sum) => (sum / align) * align,
        None => value,
    }
}

/// Geometry needed to read edge samples out of a raw NV12 buffer without
/// decoding it. `stride` is the Y-plane bytes-per-row (== width; no per-row
/// padding was observed on the RP6), and `uv_offset` is where the interleaved
/// U/V plane begins. See the module note for the measured 1920x1080 layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Nv12Layout {
    width: u32,
    height: u32,
    stride: usize,
    uv_offset: usize,
}

impl Nv12Layout {
    /// Interpret a raw NV12 buffer of `len` bytes as `width`x`height` with the
    /// given plane `align`. Returns `None` (caller falls back to the base
    /// color -- QG-2) unless the length matches one of the plausible packings:
    /// page-aligned planes (what gamescope hands back on the RP6), page-aligned
    /// Y with a tight UV tail, or fully tight. This is the guard that keeps a
    /// resolution change (or a wrong width/height) from making us sample
    /// garbage instead of admitting the frame is unreadable.
    fn resolve(len: usize, width: u32, height: u32, align: usize) -> Option<Self> {
        // NV12 needs even dimensions (2x2 chroma subsampling).
        if width == 0 || height == 0 || (width & 1) == 1 || (height & 1) == 1 {
            return None;
        }
        let w: usize = width as usize;
        let h: usize = height as usize;
        let stride: usize = w; // measured: no per-row padding
        let y_size: usize = w * h;
        let uv_size: usize = w * (h / 2); // interleaved: one U and one V per 2x2 block
        let uv_off_aligned: usize = align_up(y_size, align);

        let total_aligned: usize = uv_off_aligned + align_up(uv_size, align);
        let total_ypad: usize = uv_off_aligned + uv_size;
        let total_tight: usize = y_size + uv_size;

        let uv_offset: usize = if len == total_aligned || len == total_ypad {
            uv_off_aligned
        } else if len == total_tight {
            y_size
        } else {
            return None;
        };
        // Defensive: the sampler must never read past the buffer.
        if uv_offset.checked_add(uv_size)? > len {
            return None;
        }
        Some(Self { width, height, stride, uv_offset })
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
///
/// Returns `(chosen device, ambiguity warning)`. The second element is
/// `Some` only when more than one *non-generic* candidate remains after that
/// preference — e.g. a hypothetical dual-panel device with two named nodes —
/// so the caller can surface that the pick was a guess instead of staying
/// silent about it (the RP6's actual case, one named node + the generic
/// alias, is NOT ambiguous: there is exactly one non-generic candidate).
fn resolve_backlight_dir(root: &Path, preferred: Option<&str>) -> (Option<PathBuf>, Option<String>) {
    if let Some(name) = preferred {
        let candidate: PathBuf = root.join(name);
        let found: bool =
            candidate.join("brightness").is_file() && candidate.join("max_brightness").is_file();
        return (if found { Some(candidate) } else { None }, None);
    }

    let Some(read_dir) = fs::read_dir(root).ok() else {
        return (None, None);
    };
    let mut entries: Vec<PathBuf> = read_dir
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.join("brightness").is_file() && path.join("max_brightness").is_file())
        .collect();
    if entries.is_empty() {
        return (None, None);
    }
    entries.sort();

    let is_generic = |path: &PathBuf| path.file_name().and_then(|name| name.to_str()) == Some("backlight");
    let named: Vec<&PathBuf> = entries.iter().filter(|path| !is_generic(path)).collect();

    let chosen: Option<PathBuf> = named.first().copied().or_else(|| entries.first()).cloned();
    let warning: Option<String> = (named.len() > 1).then(|| {
        let names: Vec<&str> = named
            .iter()
            .filter_map(|path| path.file_name().and_then(|name| name.to_str()))
            .collect();
        let picked: &str = chosen
            .as_ref()
            .and_then(|path| path.file_name())
            .and_then(|name| name.to_str())
            .unwrap_or("?");
        format!(
            "found {} candidate backlight devices under {} ({}); picked '{picked}' — set \
             ARMADA_RGB_BACKLIGHT_NAME to pin the right one if this is wrong",
            named.len(),
            root.display(),
            names.join(", "),
        )
    });
    (chosen, warning)
}

fn read_u32(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Run `<bin> screenshot <path>` (or, if `su_user` is set, the same command
/// through `su - <user> -c '...'`) with the Wayland session env explicitly
/// set — `armada-rgb run` is a system service with no graphical session env
/// of its own, so it cannot rely on inherited environment variables. Bounded
/// by a hard timeout so a wedged compositor cannot hang the whole daemon
/// (config reload and every other effect share this same loop thread): on
/// timeout the whole child PROCESS GROUP is killed and reaped, never left
/// running or awaited indefinitely, so this never blocks `run` past the
/// timeout either way.
///
/// The child runs in its own process group (`process_group(0)`) so the
/// timeout can reap the entire tree, not just the direct child (A-M3): the
/// `su` path forks a login shell that in turn forks `gamescopectl`, so a
/// plain `child.kill()` would signal only `su` and leave `gamescopectl`
/// running — a real leak once per wedged capture. Every value interpolated
/// into the `su -c` shell string is single-quoted (B1) so a path or env
/// value containing a space or shell metacharacter can neither word-split
/// nor be interpreted by the login shell.
fn run_screenshot_command(
    bin: &str,
    path: &Path,
    xdg_runtime_dir: &str,
    wayland_display: &str,
    su_user: Option<&str>,
) -> Result<(), String> {
    const POLL: Duration = Duration::from_millis(50);
    let timeout: Duration = SCREEN_SYNC_CAPTURE_TIMEOUT;

    let mut command: Command = match su_user {
        Some(user) => {
            let mut command = Command::new("su");
            command.arg("-").arg(user).arg("-c").arg(su_screenshot_script(
                bin,
                path,
                xdg_runtime_dir,
                wayland_display,
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
    // A-M3: own process group so a timeout can kill the whole tree.
    command.process_group(0);

    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn {bin}: {error}"))?;
    // With `process_group(0)` the child's PID is also its process-group id,
    // so `-pgid` in `kill(2)` targets the child and every descendant.
    let pgid: i32 = child.id() as i32;

    let deadline: Instant = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    kill_process_group(pgid);
                    let _ = child.wait();
                    return Err(format!("{bin} screenshot timed out after {timeout:?}"));
                }
                sleep(POLL);
            }
            Err(error) => return Err(format!("wait for {bin}: {error}")),
        }
    }

    // `gamescopectl screenshot` returns almost immediately; the compositor
    // then writes (and GROWS) the file asynchronously, after the child has
    // already exited. Reading as soon as the file merely EXISTS would catch a
    // half-written buffer (observed: a 2_822_144-byte partial vs the full
    // 3_117_056) -- harmless for a PNG (decode fails -> fallback) but for a
    // raw NV12 buffer a short read is just a wrong size. So wait until the
    // size is non-zero and has STOPPED changing before returning. This is
    // format-agnostic and bounded by its own deadline.
    const STABILIZE: Duration = Duration::from_millis(60);
    let flush_deadline: Instant = Instant::now() + Duration::from_millis(2000);
    let mut last_size: u64 = 0;
    let mut stable_since: Option<Instant> = None;
    loop {
        match fs::metadata(path).map(|meta| meta.len()) {
            Ok(size) if size > 0 && size == last_size => {
                if stable_since.is_some_and(|since| since.elapsed() >= STABILIZE) {
                    return Ok(());
                }
            }
            Ok(size) if size > 0 => {
                last_size = size;
                stable_since = Some(Instant::now());
            }
            _ => {}
        }
        if Instant::now() >= flush_deadline {
            return Err(if last_size == 0 {
                format!("{} never appeared", path.display())
            } else {
                format!("{} did not finish writing (stalled at {last_size} bytes)", path.display())
            });
        }
        sleep(POLL);
    }
}

/// Build the argument passed to `su - <user> -c` that runs the screenshot
/// with the Wayland session env. Every interpolated value is single-quoted
/// (B1) so a path or env value with a space or shell metacharacter neither
/// word-splits nor is interpreted by the login shell.
fn su_screenshot_script(
    bin: &str,
    path: &Path,
    xdg_runtime_dir: &str,
    wayland_display: &str,
) -> String {
    format!(
        "env XDG_RUNTIME_DIR={} WAYLAND_DISPLAY={} {} screenshot {}",
        sh_single_quote(xdg_runtime_dir),
        sh_single_quote(wayland_display),
        sh_single_quote(bin),
        sh_single_quote(&path.display().to_string()),
    )
}

/// POSIX single-quote a value for a `/bin/sh` command line: wrap it in
/// `'...'` and turn each embedded `'` into `'\''` (close quote, escaped
/// quote, reopen). Safe for any byte sequence a path/env value can hold.
fn sh_single_quote(value: &str) -> String {
    let mut quoted: String = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

/// SIGKILL an entire process group (A-M3). `pgid` is the group id (== the
/// spawned child's PID, because it was started with `process_group(0)`), so
/// the negative pid passed to `kill(2)` reaches the child and every
/// descendant — including a `gamescopectl` forked by the `su` login shell,
/// which a plain `child.kill()` on `su` alone would orphan. Best-effort:
/// any error is ignored (the process may already have exited).
fn kill_process_group(pgid: i32) {
    // SAFETY: `kill(2)` with a negative pid is a plain libc call with no
    // memory effects; a stale/exited group just returns ESRCH.
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}

/// Average RGB of the outer LEFT and RIGHT edge bands of a raw NV12 buffer —
/// the ambilight source for the left/right stick LEDs. gamescope cannot
/// capture a region (see the module note), so the full frame is written, but
/// only the outer `edge_fraction` of the width on each side is *read*, on a
/// bounded grid (never more than ~64 samples per axis) so sampling a large
/// frame cannot itself become the CPU cost this effect is gated against.
/// `edge_fraction` is clamped so the two bands never overlap even on a
/// pathologically narrow frame. No PNG decode happens: we read luma from the Y
/// plane and chroma from the interleaved U/V plane directly.
fn average_edges_nv12(buf: &[u8], layout: &Nv12Layout, edge_fraction: f64) -> ([u8; 3], [u8; 3]) {
    let width: u32 = layout.width;
    let height: u32 = layout.height;
    if width == 0 || height == 0 {
        return ([0, 0, 0], [0, 0, 0]);
    }
    let fraction: f64 = edge_fraction.clamp(0.0, 0.5);
    // At least one column per side; never let the two bands overlap.
    let edge: u32 = ((f64::from(width) * fraction).round() as u32).clamp(1, (width / 2).max(1));
    let step: u32 = (width.max(height) / 64).max(1);

    let left: [u8; 3] = average_band_nv12(buf, layout, 0, edge, step);
    let right: [u8; 3] = average_band_nv12(buf, layout, width - edge, width, step);
    (left, right)
}

/// Average RGB over the columns `[x0, x1)` across the full height, on a grid
/// of stride `step`, reading the raw NV12 planes. For each sampled pixel the
/// luma comes from `Y[y*stride + x]` and the chroma from the 2x2-subsampled
/// interleaved plane at `uv_offset + (y/2)*stride + (x/2)*2` (U then V). Any
/// out-of-range index (only possible under a bad geometry) is skipped, so this
/// can never panic in the daemon loop.
fn average_band_nv12(buf: &[u8], layout: &Nv12Layout, x0: u32, x1: u32, step: u32) -> [u8; 3] {
    let mut sum: [u64; 3] = [0; 3];
    let mut n: u64 = 0;
    let mut y: u32 = 0;
    while y < layout.height {
        let y_row: usize = (y as usize) * layout.stride;
        let uv_row: usize = layout.uv_offset + (y as usize / 2) * layout.stride;
        let mut x: u32 = x0;
        while x < x1 {
            let cx: usize = (x as usize / 2) * 2; // even chroma column, U at cx, V at cx+1
            if let (Some(&luma), Some(&u), Some(&v)) = (
                buf.get(y_row + x as usize),
                buf.get(uv_row + cx),
                buf.get(uv_row + cx + 1),
            ) {
                let [r, g, b]: [u8; 3] = yuv_to_rgb(luma, u, v);
                sum[0] += u64::from(r);
                sum[1] += u64::from(g);
                sum[2] += u64::from(b);
                n += 1;
            }
            x += step;
        }
        y += step;
    }
    average(sum, n)
}

/// Full-range (JPEG/"full swing") BT.601 YUV -> RGB. The exact matrix and
/// range barely affect an ambient LED color (hue is preserved either way);
/// full-range BT.601 is the common choice and matches what a desktop-style
/// screenshot buffer carries. Clamped to `0..=255`.
fn yuv_to_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
    let yf: f64 = f64::from(y);
    let uf: f64 = f64::from(u) - 128.0;
    let vf: f64 = f64::from(v) - 128.0;
    let r: f64 = yf + 1.402 * vf;
    let g: f64 = yf - 0.344_136 * uf - 0.714_136 * vf;
    let b: f64 = yf + 1.772 * uf;
    [clamp_u8(r), clamp_u8(g), clamp_u8(b)]
}

fn clamp_u8(value: f64) -> u8 {
    value.round().clamp(0.0, 255.0) as u8
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

    // -- sync_brightness (modifier, not an effect) ---------------------------

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
    fn scale_for_sync_is_a_noop_when_the_modifier_is_off() {
        let root: PathBuf = fixture_dir("sync-off");
        backlight_device(&root, "panel.dsi.0", 1, 100); // near-zero, to prove it's ignored
        let mut state: EffectState = EffectState {
            backlight_root: root,
            backlight_name: None,
            ..EffectState::default()
        };
        assert_eq!(state.scale_for_sync(80, false), 80);
    }

    #[test]
    fn scale_for_sync_applies_on_top_of_any_brightness() {
        let root: PathBuf = fixture_dir("sync-single");
        backlight_device(&root, "panel.dsi.0", 50, 100);

        let mut state: EffectState = EffectState {
            backlight_root: root,
            backlight_name: None,
            ..EffectState::default()
        };
        // Same math whether the 80 came from a plain static config or from an
        // effect's own computed brightness (e.g. mid-breath) — scale_for_sync
        // does not know or care which.
        assert_eq!(state.scale_for_sync(80, true), 35); // 80 ceiling * 50% backlight * 0.88 (QG-5)
    }

    #[test]
    fn scale_for_sync_prefers_named_panel_over_generic_alias() {
        // Mirrors the RP6, which exposes both a generic "backlight" wrapper
        // and the panel's own named node; the named one must win by default.
        let root: PathBuf = fixture_dir("sync-ambiguous");
        backlight_device(&root, "backlight", 10, 100);
        backlight_device(&root, "panel.dsi.0", 90, 100);

        let mut state: EffectState = EffectState {
            backlight_root: root,
            backlight_name: None,
            ..EffectState::default()
        };
        assert_eq!(state.scale_for_sync(100, true), 79); // 90% backlight * 0.88 (QG-5)
    }

    #[test]
    fn scale_for_sync_honors_explicit_name_override() {
        let root: PathBuf = fixture_dir("sync-override");
        backlight_device(&root, "backlight", 10, 100);
        backlight_device(&root, "panel.dsi.0", 90, 100);

        let mut state: EffectState = EffectState {
            backlight_root: root,
            backlight_name: Some("backlight".into()),
            ..EffectState::default()
        };
        assert_eq!(state.scale_for_sync(100, true), 9); // 10% backlight * 0.88 (QG-5)
    }

    #[test]
    fn scale_for_sync_falls_back_to_configured_brightness_when_missing() {
        let root: PathBuf = fixture_dir("sync-missing"); // left empty
        let mut state: EffectState = EffectState {
            backlight_root: root,
            backlight_name: None,
            ..EffectState::default()
        };
        assert_eq!(state.scale_for_sync(55, true), 48); // 55 * 1.0 (no backlight) * 0.88 (QG-5)
    }

    #[test]
    fn resolve_backlight_dir_flags_ambiguity_between_multiple_named_candidates() {
        // A hypothetical dual-panel device: two candidates survive the
        // "prefer non-generic name" heuristic, so the pick is a genuine
        // guess and must be surfaced, not silent.
        let root: PathBuf = fixture_dir("resolve-ambiguous-named");
        backlight_device(&root, "ae94000.dsi.0", 10, 100);
        backlight_device(&root, "ae94000.dsi.1", 90, 100);

        let (chosen, warning): (Option<PathBuf>, Option<String>) = resolve_backlight_dir(&root, None);
        assert_eq!(chosen, Some(root.join("ae94000.dsi.0")), "deterministic (alphabetical) pick");
        let warning: String = warning.expect("must flag ambiguity with >1 non-generic candidate");
        assert!(warning.contains("ae94000.dsi.0"));
        assert!(warning.contains("ae94000.dsi.1"));
        assert!(warning.contains("ARMADA_RGB_BACKLIGHT_NAME"));
    }

    #[test]
    fn resolve_backlight_dir_does_not_flag_the_rp6s_actual_case() {
        // One named node + the generic alias is NOT ambiguous — exactly one
        // non-generic candidate exists, so no guess is being made.
        let root: PathBuf = fixture_dir("resolve-not-ambiguous");
        backlight_device(&root, "backlight", 10, 100);
        backlight_device(&root, "panel.dsi.0", 90, 100);

        let (chosen, warning): (Option<PathBuf>, Option<String>) = resolve_backlight_dir(&root, None);
        assert_eq!(chosen, Some(root.join("panel.dsi.0")));
        assert!(warning.is_none());
    }

    #[test]
    fn resolve_backlight_dir_explicit_override_never_warns() {
        let root: PathBuf = fixture_dir("resolve-override-no-warn");
        backlight_device(&root, "a", 1, 100);
        backlight_device(&root, "b", 1, 100);

        let (chosen, warning): (Option<PathBuf>, Option<String>) = resolve_backlight_dir(&root, Some("a"));
        assert_eq!(chosen, Some(root.join("a")));
        assert!(warning.is_none());
    }

    // -- NV12 helpers for tests ----------------------------------------------

    /// Forward of `yuv_to_rgb` (full-range BT.601 RGB -> YUV) so the test
    /// fixtures encode a known color that the sampler reads back to ~the same
    /// RGB (within rounding).
    fn rgb_to_yuv(rgb: [u8; 3]) -> (u8, u8, u8) {
        let r: f64 = f64::from(rgb[0]);
        let g: f64 = f64::from(rgb[1]);
        let b: f64 = f64::from(rgb[2]);
        let y: f64 = 0.299 * r + 0.587 * g + 0.114 * b;
        let u: f64 = -0.168_736 * r - 0.331_264 * g + 0.5 * b + 128.0;
        let v: f64 = 0.5 * r - 0.418_688 * g - 0.081_312 * b + 128.0;
        (clamp_u8(y), clamp_u8(u), clamp_u8(v))
    }

    /// Build a raw NV12 buffer (page-aligned planes, like gamescope's) whose
    /// per-pixel color is `color_at(x, y)`. `align` sizes each plane's padding.
    fn make_nv12(
        width: u32,
        height: u32,
        align: usize,
        color_at: impl Fn(u32, u32) -> [u8; 3],
    ) -> Vec<u8> {
        let w: usize = width as usize;
        let h: usize = height as usize;
        let y_size: usize = w * h;
        let uv_size: usize = w * (h / 2);
        let uv_off: usize = align_up(y_size, align);
        let total: usize = uv_off + align_up(uv_size, align);
        let mut buf: Vec<u8> = vec![0u8; total];
        for y in 0..height {
            for x in 0..width {
                let (luma, _, _) = rgb_to_yuv(color_at(x, y));
                buf[y as usize * w + x as usize] = luma;
            }
        }
        // Chroma sampled from the top-left of each 2x2 block.
        for cy in 0..(height / 2) {
            for cx in 0..(width / 2) {
                let (_, u, v) = rgb_to_yuv(color_at(cx * 2, cy * 2));
                let off: usize = uv_off + cy as usize * w + cx as usize * 2;
                buf[off] = u;
                buf[off + 1] = v;
            }
        }
        buf
    }

    /// A fake `gamescopectl` that writes a fixed NV12 buffer to `$2` (the path
    /// arg), so the whole capture path (spawn, poll, read, sample) is exercised
    /// without a real compositor.
    fn write_fake_gamescopectl(root: &Path, fixture_nv12: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script: PathBuf = root.join("fake-gamescopectl.sh");
        fs::write(
            &script,
            format!("#!/bin/sh\ncp '{}' \"$2\"\n", fixture_nv12.display()),
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();
        script
    }

    fn write_split_fixture(root: &Path, left: [u8; 3], right: [u8; 3]) -> PathBuf {
        let path: PathBuf = root.join("fixture.nv12.bin");
        let buf: Vec<u8> = make_nv12(8, 4, 64, |x, _| if x < 4 { left } else { right });
        fs::write(&path, &buf).unwrap();
        path
    }

    fn approx(actual: [u8; 3], expected: [u8; 3], label: &str) {
        for i in 0..3 {
            let diff: i32 = i32::from(actual[i]) - i32::from(expected[i]);
            assert!(
                diff.abs() <= 4,
                "{label}: channel {i} {} not ~= {} (YUV round-trip)",
                actual[i],
                expected[i]
            );
        }
    }

    // -- Nv12Layout::resolve --------------------------------------------------

    #[test]
    fn nv12_layout_matches_the_measured_rp6_geometry() {
        // Game Mode, 2026-09-20: gamescope wrote exactly 3_117_056 bytes for a
        // 1920x1080 output with 4096-byte page-aligned planes.
        let layout = Nv12Layout::resolve(3_117_056, 1920, 1080, 4096)
            .expect("the measured RP6 size must be accepted");
        assert_eq!(layout.width, 1920);
        assert_eq!(layout.height, 1080);
        assert_eq!(layout.stride, 1920, "no per-row padding was observed");
        assert_eq!(layout.uv_offset, 2_076_672, "UV plane starts at align_up(1920*1080, 4096)");
    }

    #[test]
    fn nv12_layout_accepts_tightly_packed_and_rejects_wrong_size() {
        // Tightly packed (no plane padding): align 1 => uv right after Y.
        let tight = Nv12Layout::resolve(1920 * 1080 * 3 / 2, 1920, 1080, 4096).unwrap();
        assert_eq!(tight.uv_offset, 1920 * 1080);
        // A size that matches no packing (a resolution change) is rejected so
        // the caller falls back to base instead of sampling garbage (QG-2).
        assert!(Nv12Layout::resolve(1_234_567, 1920, 1080, 4096).is_none());
        // Odd dimensions are not valid NV12.
        assert!(Nv12Layout::resolve(100, 11, 10, 64).is_none());
    }

    // -- screen_sync ----------------------------------------------------------

    #[test]
    fn screen_sync_averages_left_and_right_halves() {
        let root: PathBuf = fixture_dir("screen-sync-ok");
        let fixture: PathBuf = write_split_fixture(&root, [255, 0, 0], [0, 0, 255]);
        let script: PathBuf = write_fake_gamescopectl(&root, &fixture);

        let mut state: EffectState = EffectState {
            gamescopectl_bin: script.to_string_lossy().into_owned(),
            screenshot_path: root.join("out.nv12.bin"),
            screen_sync_user: None,
            screen_width: 8,
            screen_height: 4,
            nv12_plane_align: 64,
            ..EffectState::default()
        };
        let (frame, _brightness) = state.render(Effect::ScreenSync, [0, 0, 0], 100, 100, 0.0, 8);
        match frame {
            Frame::PerTarget(colors) => {
                assert_eq!(colors.len(), 8);
                approx(colors[0], [255, 0, 0], "left ring should follow the left half");
                approx(colors[7], [0, 0, 255], "right ring should follow the right half");
            }
            other => panic!("screen_sync must be per-target, got {other:?}"),
        }
    }

    #[test]
    fn screen_sync_falls_back_to_last_colors_then_recovers() {
        let root: PathBuf = fixture_dir("screen-sync-fallback");
        let fixture: PathBuf = write_split_fixture(&root, [10, 20, 30], [40, 50, 60]);
        let script: PathBuf = write_fake_gamescopectl(&root, &fixture);

        let mut state: EffectState = EffectState {
            gamescopectl_bin: root.join("does-not-exist").to_string_lossy().into_owned(),
            screenshot_path: root.join("out.nv12.bin"),
            screen_sync_user: None,
            screen_width: 8,
            screen_height: 4,
            nv12_plane_align: 64,
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
        let colors: Vec<[u8; 3]> = frame.expand(4);
        approx(colors[0], [10, 20, 30], "left recovered");
        approx(colors[1], [10, 20, 30], "left recovered");
        approx(colors[2], [40, 50, 60], "right recovered");
        approx(colors[3], [40, 50, 60], "right recovered");
    }

    #[test]
    fn screen_sync_falls_back_to_base_when_size_mismatches() {
        // A buffer whose length matches no packing for the configured geometry
        // (as if the resolution changed) must NOT paint garbage: QG-2 falls
        // back to the base color.
        let root: PathBuf = fixture_dir("screen-sync-size-mismatch");
        let fixture: PathBuf = root.join("fixture.nv12.bin");
        fs::write(&fixture, vec![200u8; 4096]).unwrap(); // wrong size for 8x4
        let script: PathBuf = write_fake_gamescopectl(&root, &fixture);
        let mut state: EffectState = EffectState {
            gamescopectl_bin: script.to_string_lossy().into_owned(),
            screenshot_path: root.join("out.nv12.bin"),
            screen_sync_user: None,
            screen_width: 8,
            screen_height: 4,
            nv12_plane_align: 64,
            ..EffectState::default()
        };
        let base: [u8; 3] = [7, 8, 9];
        let (frame, _) = state.render(Effect::ScreenSync, base, 100, 100, 0.0, 4);
        assert_eq!(frame.expand(4), vec![base; 4], "size mismatch must fall back to base, not garbage");
    }

    // -- efficient edge sampling (armada#27) ---------------------------------

    /// Build a WxH NV12 buffer with distinct left-edge / center / right-edge colors.
    fn banded_nv12(
        width: u32,
        height: u32,
        edge: u32,
        left: [u8; 3],
        center: [u8; 3],
        right: [u8; 3],
    ) -> (Vec<u8>, Nv12Layout) {
        let buf: Vec<u8> = make_nv12(width, height, 64, |x, _| {
            if x < edge {
                left
            } else if x >= width - edge {
                right
            } else {
                center
            }
        });
        let layout: Nv12Layout = Nv12Layout::resolve(buf.len(), width, height, 64).unwrap();
        (buf, layout)
    }

    #[test]
    fn average_edges_reads_only_the_borders_not_the_center() {
        // A wide frame whose center is a loud color the ambilight must ignore.
        let (buf, layout) = banded_nv12(10, 4, 2, [255, 0, 0], [0, 255, 0], [0, 0, 255]);
        let (left, right) = average_edges_nv12(&buf, &layout, 0.2); // 0.2 * 10 = 2 cols per side
        approx(left, [255, 0, 0], "left ring follows the left border, not the green center");
        approx(right, [0, 0, 255], "right ring follows the right border, not the green center");
    }

    #[test]
    fn average_edges_always_samples_at_least_one_column_per_side() {
        // Fraction rounds to zero, but each side must still yield a sample.
        let (buf, layout) = banded_nv12(20, 4, 2, [10, 10, 10], [99, 99, 99], [20, 20, 20]);
        let (left, right) = average_edges_nv12(&buf, &layout, 0.0);
        approx(left, [10, 10, 10], "left");
        approx(right, [20, 20, 20], "right");
    }

    #[test]
    fn average_edges_does_not_panic_on_a_two_pixel_frame() {
        // Smallest valid NV12 (even dims). A bad geometry can't panic either:
        // out-of-range samples are skipped.
        let (buf, layout) = banded_nv12(2, 2, 1, [7, 8, 9], [7, 8, 9], [7, 8, 9]);
        let (left, right) = average_edges_nv12(&buf, &layout, 0.08);
        approx(left, [7, 8, 9], "left");
        approx(right, [7, 8, 9], "right");
    }

    #[test]
    fn yuv_to_rgb_round_trips_primaries() {
        for color in [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255], [0, 0, 0]] {
            let (y, u, v) = rgb_to_yuv(color);
            approx(yuv_to_rgb(y, u, v), color, "primary round-trip");
        }
    }

    // -- su -c shell quoting (B1) --------------------------------------------

    #[test]
    fn sh_single_quote_wraps_and_escapes() {
        assert_eq!(sh_single_quote("abc"), "'abc'");
        assert_eq!(sh_single_quote(""), "''");
        assert_eq!(sh_single_quote("a b"), "'a b'"); // space stays inside one quote
        assert_eq!(sh_single_quote("it's"), "'it'\\''s'"); // embedded quote escaped
        assert_eq!(sh_single_quote("a;b|c$(x)"), "'a;b|c$(x)'"); // metachars inert
    }

    #[test]
    fn su_screenshot_script_quotes_every_interpolated_value() {
        let script = su_screenshot_script(
            "gamescopectl",
            Path::new("/run/armada-rgb/screen sync.png"),
            "/run/user/1000",
            "gamescope-0",
        );
        assert_eq!(
            script,
            "env XDG_RUNTIME_DIR='/run/user/1000' WAYLAND_DISPLAY='gamescope-0' \
             'gamescopectl' screenshot '/run/armada-rgb/screen sync.png'",
            "the path's space must stay a single argument, not word-split"
        );
    }
}
