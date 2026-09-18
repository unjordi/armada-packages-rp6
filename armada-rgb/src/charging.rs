//! One-shot "charging indicator" painted while the device is asleep.
//!
//! During true deep suspend the CPU is powered off, so nothing can drive an
//! animation loop — but the LED controller hardware itself keeps holding
//! whatever value was last written, with no help from software (this is
//! verified in the field: a stray `echo` to sysfs weeks earlier was still
//! lit on the next boot). A brief wake-on-charge-attach can therefore call
//! `armada-rgb charge-indicator on` to paint a fixed, non-animated color
//! directly to hardware before the system goes back to sleep; the hardware
//! then holds it through the rest of the suspend on its own.
//!
//! This module also persists that intent to a small state file so the `run`
//! daemon, if it happens to be thawed during the same brief wake window,
//! defers to the indicator instead of racing to repaint the normal
//! configuration over it. `charge-indicator off` clears that pin and
//! restores the saved configuration immediately.

use crate::{LightingBackend, LightingConfig};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Amber: visually distinct from any "on" effect a user is likely to pick,
/// and readable at a glance as a status light rather than a light show.
pub(crate) const DEFAULT_COLOR: &str = "FFA500";
/// Dim: this lights up while the device is meant to be asleep in a pocket or
/// on a nightstand, not putting on a display.
pub(crate) const DEFAULT_BRIGHTNESS: u8 = 15;

/// The fixed, non-animated state painted while the device is asleep.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ChargeIndicator {
    pub color: String,
    pub brightness: u8,
}

impl ChargeIndicator {
    pub fn new(color: Option<String>, brightness: Option<u8>) -> Result<Self> {
        let indicator: Self = Self {
            color: color.unwrap_or_else(|| DEFAULT_COLOR.to_string()),
            brightness: brightness.unwrap_or(DEFAULT_BRIGHTNESS),
        };
        indicator.validate()
    }

    fn validate(self) -> Result<Self> {
        if self.color.len() != 6 || !self.color.bytes().all(|c| c.is_ascii_hexdigit()) {
            bail!(
                "charge indicator color must be six hexadecimal RGB digits, got '{}'",
                self.color
            );
        }
        if self.brightness > 100 {
            bail!(
                "charge indicator brightness must be between 0 and 100, got {}",
                self.brightness
            );
        }
        Ok(Self {
            color: self.color.to_ascii_uppercase(),
            ..self
        })
    }

    /// The indicator is always a plain solid color: nothing can animate it
    /// once the CPU stops, so `LightingConfig::effect` stays `static` by
    /// construction (there is no field for anything else here).
    pub(crate) fn to_config(&self) -> LightingConfig {
        LightingConfig {
            enabled: true,
            brightness: self.brightness,
            color: self.color.clone(),
            ..LightingConfig::default()
        }
    }
}

/// Read the pinned indicator, if one is set. Any parse/read failure is
/// treated as "not pinned" (the daemon should never get stuck because of a
/// corrupt override file — the worst case is it falls back to normal
/// rendering, not that it freezes on the charge color forever).
pub(crate) fn load(path: &Path) -> Option<ChargeIndicator> {
    let input: String = fs::read_to_string(path).ok()?;
    serde_json::from_str(&input).ok()
}

pub(crate) fn save(path: &Path, indicator: &ChargeIndicator) -> Result<()> {
    let directory: &Path = path
        .parent()
        .context("charge indicator path has no parent")?;
    let temporary: PathBuf = path.with_extension("tmp");
    let mut contents: Vec<u8> = serde_json::to_vec_pretty(indicator)?;
    contents.push(b'\n');

    fs::create_dir_all(directory).context("create charge indicator directory")?;
    fs::write(&temporary, contents).context("write temporary charge indicator")?;
    fs::rename(temporary, path).context("replace charge indicator")
}

pub(crate) fn clear(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("remove charge indicator"),
    }
}

/// Write the indicator straight to hardware, once, bypassing `/etc/armada/rgb.json`
/// (the persisted user configuration is left untouched so it can be restored
/// as-is on `charge-indicator off`).
pub(crate) fn apply_directly(backend: &LightingBackend, indicator: &ChargeIndicator) -> Result<()> {
    backend.apply(&indicator.to_config())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        let indicator: ChargeIndicator = ChargeIndicator::new(None, None).unwrap();
        assert_eq!(indicator.color, DEFAULT_COLOR);
        assert_eq!(indicator.brightness, DEFAULT_BRIGHTNESS);
    }

    #[test]
    fn normalizes_and_validates_color() {
        let indicator: ChargeIndicator =
            ChargeIndicator::new(Some("a1b2c3".into()), Some(50)).unwrap();
        assert_eq!(indicator.color, "A1B2C3");

        assert!(ChargeIndicator::new(Some("zzzzzz".into()), None).is_err());
        assert!(ChargeIndicator::new(Some("FFF".into()), None).is_err());
        assert!(ChargeIndicator::new(None, Some(101)).is_err());
    }

    #[test]
    fn to_config_is_always_static_and_enabled() {
        let indicator: ChargeIndicator =
            ChargeIndicator::new(Some("00FF00".into()), Some(10)).unwrap();
        let config: LightingConfig = indicator.to_config();
        assert!(config.enabled);
        assert_eq!(config.brightness, 10);
        assert_eq!(config.color, "00FF00");
    }

    #[test]
    fn round_trips_through_save_and_load() {
        let root: PathBuf =
            std::env::temp_dir().join(format!("armada-rgb-charging-test-{}", std::process::id()));
        let path: PathBuf = root.join("charge.json");
        let indicator: ChargeIndicator = ChargeIndicator::new(Some("112233".into()), Some(20)).unwrap();

        assert!(load(&path).is_none());
        save(&path, &indicator).unwrap();
        assert_eq!(load(&path), Some(indicator));
        assert!(!path.with_extension("tmp").exists());

        clear(&path).unwrap();
        assert!(load(&path).is_none());
        // Clearing twice (no file present) must not error.
        clear(&path).unwrap();

        let _ = fs::remove_dir_all(&root);
    }
}
