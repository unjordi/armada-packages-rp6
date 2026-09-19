//! Hardware backends for RGB lighting.

use crate::{ColorCorrection, LightingConfig};
use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub enum LightingBackend {
    Channels(ChannelBackend),
    Multicolor(MulticolorBackend),
    Unsupported(String),
}

impl LightingBackend {
    pub fn apply(&self, config: &LightingConfig) -> Result<()> {
        match self {
            Self::Channels(backend) => backend.apply(config),
            Self::Multicolor(backend) => backend.apply(config),
            Self::Unsupported(reason) => bail!("{reason}"),
        }
    }

    pub fn unsupported_reason(&self) -> Option<&str> {
        match self {
            Self::Channels(_) | Self::Multicolor(_) => None,
            Self::Unsupported(reason) => Some(reason),
        }
    }

    pub(crate) fn default_correction(&self) -> Option<ColorCorrection> {
        match self {
            Self::Channels(backend) => backend.correction.clone(),
            Self::Multicolor(backend) => backend.correction.clone(),
            Self::Unsupported(_) => None,
        }
    }

    /// Number of LED targets the active backend drives (0 if unsupported).
    pub fn target_count(&self) -> usize {
        match self {
            Self::Channels(backend) => backend.targets.len(),
            Self::Multicolor(backend) => backend.targets.len(),
            Self::Unsupported(_) => 0,
        }
    }

    /// Paint an explicit color per target at a shared brightness percentage.
    /// Used by the animation loop: multicolor devices honour per-target colors
    /// (for spatial effects like rainbow); other backends fall back to a
    /// uniform color (the first entry) through their validated `apply` path.
    pub fn render(&self, colors: &[[u8; 3]], brightness: u8) -> Result<()> {
        match self {
            Self::Multicolor(backend) => backend.render(colors, brightness),
            Self::Channels(backend) => {
                let color: [u8; 3] = colors.first().copied().unwrap_or([0, 0, 0]);
                backend.apply(&uniform_config(color, brightness))
            }
            Self::Unsupported(reason) => bail!("{reason}"),
        }
    }
}

fn uniform_config(rgb: [u8; 3], brightness: u8) -> LightingConfig {
    LightingConfig {
        enabled: true,
        brightness,
        color: format!("{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2]),
        ..LightingConfig::default()
    }
}

pub struct ChannelBackend {
    root: PathBuf,
    targets: Vec<String>,
    correction: Option<ColorCorrection>,
}

impl ChannelBackend {
    pub fn new(root: PathBuf, targets: Vec<String>) -> Self {
        Self {
            root,
            targets,
            correction: None,
        }
    }

    pub(crate) fn with_correction(mut self, correction: Option<ColorCorrection>) -> Self {
        self.correction = correction;
        self
    }

    fn apply(&self, config: &LightingConfig) -> Result<()> {
        let mut targets: Vec<PreparedChannel> = self.prepare(config)?;

        if let Err(error) = write_channels(&mut targets) {
            blank_channels_best_effort(&targets);
            return Err(error);
        }
        Ok(())
    }

    fn prepare(&self, config: &LightingConfig) -> Result<Vec<PreparedChannel>> {
        let [red, green, blue]: [u8; 3] = corrected_rgb(config, self.correction.as_ref());
        let mut channels: Vec<(String, u8)> = Vec::new();

        for target in &self.targets {
            let (channel, name): (&str, &str) = target
                .split_once('=')
                .with_context(|| format!("invalid RGB channel target '{target}'"))?;
            let value: u8 = match channel {
                "red" => red,
                "green" => green,
                "blue" => blue,
                _ => bail!("invalid RGB channel '{channel}'"),
            };
            channels.push((name.into(), value));
        }

        let names: Vec<String> = channels.iter().map(|(name, _)| name.clone()).collect();
        validate_names(&names)?;

        let mut targets: Vec<PreparedChannel> = Vec::new();

        for (name, channel) in channels {
            let path: PathBuf = self.root.join(&name);
            reclaim_led(&path);
            let brightness_path: PathBuf = path.join("brightness");
            let brightness: File = OpenOptions::new()
                .write(true)
                .open(&brightness_path)
                .with_context(|| format!("open {name} brightness"))?;
            let value: u32 = if config.enabled {
                let maximum: u32 = read_maximum(&path.join("max_brightness"))?;
                scale(config.brightness, gamma(channel, maximum))
            } else {
                0
            };

            targets.push(PreparedChannel {
                name,
                brightness_path,
                brightness,
                value: value.to_string(),
            });
        }
        Ok(targets)
    }
}

pub struct MulticolorBackend {
    root: PathBuf,
    targets: Vec<String>,
    correction: Option<ColorCorrection>,
}

impl MulticolorBackend {
    pub fn new(root: PathBuf, targets: Vec<String>) -> Self {
        Self {
            root,
            targets,
            correction: None,
        }
    }

    pub(crate) fn with_correction(mut self, correction: Option<ColorCorrection>) -> Self {
        self.correction = correction;
        self
    }

    fn apply(&self, config: &LightingConfig) -> Result<()> {
        let mut targets: Vec<PreparedTarget> = self.prepare(config)?;

        if !config.enabled {
            return blank(&mut targets);
        }

        if let Err(error) = write_colors(&mut targets) {
            blank_best_effort(&mut targets);
            return Err(error);
        }
        if let Err(error) = write_brightness(&mut targets) {
            blank_best_effort(&mut targets);
            return Err(error);
        }
        Ok(())
    }

    fn prepare(&self, config: &LightingConfig) -> Result<Vec<PreparedTarget>> {
        validate_names(&self.targets)?;
        let mut targets: Vec<PreparedTarget> = Vec::new();
        let rgb: [u8; 3] = corrected_rgb(config, self.correction.as_ref());

        for name in &self.targets {
            let path: PathBuf = self.root.join(name);
            reclaim_led(&path);
            let brightness_path: PathBuf = path.join("brightness");
            let blank: File = OpenOptions::new()
                .write(true)
                .open(&brightness_path)
                .with_context(|| format!("open {name} brightness"))?;

            if !config.enabled {
                targets.push(PreparedTarget {
                    name: name.clone(),
                    brightness_path,
                    blank,
                    brightness: None,
                    color: None,
                });
                continue;
            }

            let order: Vec<String> = read_order(&path.join("multi_index"))?;
            let maximum: u32 = read_maximum(&path.join("max_brightness"))?;
            let values: Vec<String> = order
                .iter()
                .map(|channel| channel_value(channel, rgb, maximum).to_string())
                .collect();
            let intensity: File = OpenOptions::new()
                .write(true)
                .open(path.join("multi_intensity"))
                .with_context(|| format!("open {name} multi_intensity"))?;
            let brightness: File = OpenOptions::new()
                .write(true)
                .open(&brightness_path)
                .with_context(|| format!("open {name} brightness"))?;
            let brightness_value: String = scale(config.brightness, maximum).to_string();

            targets.push(PreparedTarget {
                name: name.clone(),
                brightness_path,
                blank,
                brightness: Some((brightness, brightness_value)),
                color: Some((intensity, values.join(" "))),
            });
        }
        Ok(targets)
    }

    /// Paint one already-computed color per target at `brightness` percent,
    /// reusing the profile correction, `multi_index` order, gamma curve and
    /// brightness scaling of the `apply` path. Missing colors default to off.
    fn render(&self, colors: &[[u8; 3]], brightness: u8) -> Result<()> {
        validate_names(&self.targets)?;
        for (index, name) in self.targets.iter().enumerate() {
            let rgb: [u8; 3] = colors.get(index).copied().unwrap_or([0, 0, 0]);
            let rgb: [u8; 3] = match &self.correction {
                Some(correction) => correction.apply(rgb),
                None => rgb,
            };
            let path: PathBuf = self.root.join(name);
            reclaim_led(&path);
            let order: Vec<String> = read_order(&path.join("multi_index"))?;
            let maximum: u32 = read_maximum(&path.join("max_brightness"))?;
            let values: Vec<String> = order
                .iter()
                .map(|channel| channel_value(channel, rgb, maximum).to_string())
                .collect();
            let mut intensity: File = OpenOptions::new()
                .write(true)
                .open(path.join("multi_intensity"))
                .with_context(|| format!("open {name} multi_intensity"))?;
            write_attr(&mut intensity, &values.join(" "))
                .with_context(|| format!("write {name} color"))?;
            let mut brightness_file: File = OpenOptions::new()
                .write(true)
                .open(path.join("brightness"))
                .with_context(|| format!("open {name} brightness"))?;
            write_attr(&mut brightness_file, &scale(brightness, maximum).to_string())
                .with_context(|| format!("write {name} brightness"))?;
        }
        Ok(())
    }
}

struct PreparedTarget {
    name: String,
    brightness_path: PathBuf,
    blank: File,
    brightness: Option<(File, String)>,
    color: Option<(File, String)>,
}

struct PreparedChannel {
    name: String,
    brightness_path: PathBuf,
    brightness: File,
    value: String,
}

fn blank(targets: &mut [PreparedTarget]) -> Result<()> {
    for target in targets {
        write_attr(&mut target.blank, "0")
            .with_context(|| format!("write {} brightness", target.name))?;
    }
    Ok(())
}

fn blank_best_effort(targets: &mut [PreparedTarget]) {
    for target in targets {
        let _ = fs::write(&target.brightness_path, b"0\n");
    }
}

fn blank_channels_best_effort(targets: &[PreparedChannel]) {
    for target in targets {
        let _ = fs::write(&target.brightness_path, b"0\n");
    }
}

fn write_channels(targets: &mut [PreparedChannel]) -> Result<()> {
    for target in targets {
        write_attr(&mut target.brightness, &target.value)
            .with_context(|| format!("write {} brightness", target.name))?;
    }
    Ok(())
}

fn write_colors(targets: &mut [PreparedTarget]) -> Result<()> {
    for target in targets {
        // Invariant: `prepare` only calls these helpers when `config.enabled`,
        // in which case it always sets `color`. A missing value would be a
        // programming error, not a runtime I/O failure — surface it as a clear
        // error instead of an opaque panic.
        let Some((file, value)) = target.color.as_mut() else {
            bail!("internal: target '{}' has no prepared color (expected when enabled)", target.name);
        };
        write_attr(file, value).with_context(|| format!("write {} color", target.name))?;
    }
    Ok(())
}

fn write_brightness(targets: &mut [PreparedTarget]) -> Result<()> {
    for target in targets {
        let Some((file, value)) = target.brightness.as_mut() else {
            bail!("internal: target '{}' has no prepared brightness (expected when enabled)", target.name);
        };
        write_attr(file, value).with_context(|| format!("write {} brightness", target.name))?;
    }
    Ok(())
}

fn write_attr(file: &mut File, value: &str) -> std::io::Result<()> {
    let output: String = format!("{value}\n");
    file.write_all(output.as_bytes())?;
    file.flush()
}

/// Reclaim an LED from a kernel trigger before the daemon drives it.
///
/// The suspend charging hand-off (armada#26) arms a kernel
/// `<psy>-charging-orange-full-green` trigger on these nodes while the device
/// sleeps -- so the charge state keeps painting through deep sleep with no CPU
/// -- and the resume hook disarms it (writes `none`) before the daemon runs.
/// A LED whose trigger is still armed is treated as "not ours": the only time
/// the daemon should meet one is leftover from a crash that skipped the resume
/// hook, so we disarm it here before writing, otherwise the kernel trigger
/// would keep overriding our brightness. Best-effort: nodes with no `trigger`
/// attribute, or one already `none`, are left untouched, and any error is
/// ignored (lighting is cosmetic).
fn reclaim_led(led_dir: &Path) {
    let trigger_path: PathBuf = led_dir.join("trigger");
    let Ok(contents) = fs::read_to_string(&trigger_path) else {
        return;
    };
    // The active trigger is the token wrapped in brackets, e.g.
    // "none rc-feedback [battery-charging-orange-full-green] timer".
    let active: Option<&str> = contents
        .split_whitespace()
        .find_map(|token| token.strip_prefix('[').and_then(|t| t.strip_suffix(']')));
    if matches!(active, Some(name) if name != "none") {
        let _ = fs::write(&trigger_path, b"none\n");
    }
}

fn read_order(path: &Path) -> Result<Vec<String>> {
    let input: String =
        fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let order: Vec<String> = input.split_whitespace().map(str::to_lowercase).collect();
    let channels: HashSet<&str> = order.iter().map(String::as_str).collect();

    if order.len() != 3 || channels != HashSet::from(["red", "green", "blue"]) {
        bail!(
            "{} is not an RGB multi_index: '{}'",
            path.display(),
            input.trim()
        );
    }
    Ok(order)
}

fn read_maximum(path: &Path) -> Result<u32> {
    let input: String =
        fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let maximum: u32 = input
        .trim()
        .parse()
        .with_context(|| format!("parse {}", path.display()))?;

    if maximum == 0 {
        bail!("{} is zero", path.display());
    }
    Ok(maximum)
}

fn validate_names(targets: &[String]) -> Result<()> {
    let mut seen: HashSet<&String> = HashSet::new();

    if targets.is_empty() {
        bail!("RGB target list is empty");
    }
    for target in targets {
        let valid: bool = !target.is_empty()
            && target
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b":_.-".contains(&c))
            && target != "."
            && target != "..";
        if !valid {
            bail!("invalid target name '{target}'");
        }
        if !seen.insert(target) {
            bail!("duplicate target '{target}'");
        }
    }
    Ok(())
}

fn channel_value(channel: &str, [red, green, blue]: [u8; 3], maximum: u32) -> u32 {
    match channel {
        "red" => gamma(red, maximum),
        "green" => gamma(green, maximum),
        "blue" => gamma(blue, maximum),
        _ => unreachable!("validated channel"),
    }
}

fn corrected_rgb(config: &LightingConfig, profile: Option<&ColorCorrection>) -> [u8; 3] {
    let rgb: [u8; 3] = config.rgb();
    let correction: Option<&ColorCorrection> = config.correction.as_ref().or(profile);
    let Some(correction) = correction else {
        return rgb;
    };
    correction.apply(rgb)
}

fn gamma(channel: u8, maximum: u32) -> u32 {
    let value: f64 = f64::from(channel) / 255.0;
    let linear: f64 = if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    };
    (linear * f64::from(maximum)).round() as u32
}

fn scale(percent: u8, maximum: u32) -> u32 {
    ((u64::from(percent) * u64::from(maximum) + 50) / 100) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_led(trigger: Option<&str>) -> PathBuf {
        let nonce: u128 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir: PathBuf = std::env::temp_dir().join(format!(
            "rgb-reclaim-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        if let Some(contents) = trigger {
            fs::write(dir.join("trigger"), contents).unwrap();
        }
        dir
    }

    #[test]
    fn scales_channels_and_brightness() {
        assert_eq!(gamma(0, 255), 0);
        assert_eq!(gamma(128, 100), 22);
        assert_eq!(gamma(255, 255), 255);
        assert_eq!(scale(25, 255), 64);
    }

    #[test]
    fn reclaims_an_armed_trigger() {
        let dir: PathBuf = temp_led(Some(
            "none rc-feedback [battery-charging-orange-full-green] timer\n",
        ));
        reclaim_led(&dir);
        assert_eq!(fs::read_to_string(dir.join("trigger")).unwrap(), "none\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn leaves_a_disarmed_trigger_untouched() {
        let contents: &str = "[none] rc-feedback battery-charging-orange-full-green timer\n";
        let dir: PathBuf = temp_led(Some(contents));
        reclaim_led(&dir);
        assert_eq!(fs::read_to_string(dir.join("trigger")).unwrap(), contents);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ignores_a_node_without_a_trigger_attribute() {
        let dir: PathBuf = temp_led(None);
        reclaim_led(&dir); // must not panic or create the file
        assert!(!dir.join("trigger").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
