use crate::charging::{self, ChargeIndicator};
use crate::{config, runtime, EffectState, LightingBackend, LightingConfig};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime};

/// Frames per second for animated effects that do not set their own cadence.
const FPS: u32 = 30;

pub struct Controller {
    config_path: PathBuf,
    backend: LightingBackend,
    charge_path: PathBuf,
}

impl Controller {
    pub fn new(config_path: PathBuf, backend: LightingBackend) -> Self {
        // Deterministic per-caller default (a sibling of the config file) so
        // tests that build a `Controller` directly stay isolated without
        // needing to know about `/run`. `from_env()` overrides this with the
        // real (tmpfs) production path below.
        let charge_path: PathBuf = config_path.with_file_name("charge.json");
        Self {
            config_path,
            backend,
            charge_path,
        }
    }

    /// Override where the charging-indicator pin is read/written. Used by
    /// `from_env()` to point at the real (tmpfs) production path.
    pub fn with_charge_path(mut self, charge_path: PathBuf) -> Self {
        self.charge_path = charge_path;
        self
    }

    pub fn from_env() -> Self {
        let (config_path, backend): (PathBuf, LightingBackend) = runtime::from_env();
        let charge_path: PathBuf = runtime::charge_path_from_env();
        Self::new(config_path, backend).with_charge_path(charge_path)
    }

    pub fn get(&self) -> Result<LightingConfig> {
        let mut config: LightingConfig = config::load(&self.config_path)?;
        if config.correction.is_none() {
            config.correction = self.backend.default_correction();
        }
        Ok(config)
    }

    pub fn is_supported(&self) -> bool {
        self.backend.unsupported_reason().is_none()
    }

    pub fn set(&self, config: LightingConfig) -> Result<LightingConfig> {
        let mut config: LightingConfig = config.validate()?;
        if config.correction.is_none() {
            config.correction = self.backend.default_correction();
        }
        self.backend.apply(&config)?;
        config::save(&self.config_path, &config)?;
        Ok(config)
    }

    pub fn off(&self) -> Result<LightingConfig> {
        let mut config: LightingConfig = self.get()?;
        config.enabled = false;
        self.set(config)
    }

    pub fn apply(&self) -> Result<Option<String>> {
        if let Some(reason) = self.backend.unsupported_reason() {
            return Ok(Some(reason.into()));
        }

        let config: LightingConfig = self.get()?;
        self.backend.apply(&config)?;
        Ok(None)
    }

    /// Paint the charging indicator directly (works even if `run` is not
    /// active) and pin it, so a `run` daemon thawed during the same wake
    /// does not repaint the normal effect over it. Meant to be called by a
    /// suspend/wake hook right before the device goes back to sleep.
    pub fn charge_indicator_on(
        &self,
        color: Option<String>,
        brightness: Option<u8>,
    ) -> Result<ChargeIndicator> {
        if let Some(reason) = self.backend.unsupported_reason() {
            bail!("RGB unsupported: {reason}");
        }
        let indicator: ChargeIndicator = ChargeIndicator::new(color, brightness)?;
        charging::apply_directly(&self.backend, &indicator).context("paint charge indicator")?;
        charging::save(&self.charge_path, &indicator)
            .context("persist charge indicator override")?;
        Ok(indicator)
    }

    /// Clear the pin and restore the saved configuration immediately (does
    /// not wait for a `run` daemon's next tick, which may not be running or
    /// may still be thawing).
    pub fn charge_indicator_off(&self) -> Result<LightingConfig> {
        charging::clear(&self.charge_path).context("clear charge indicator override")?;
        if let Some(reason) = self.backend.unsupported_reason() {
            bail!("RGB unsupported: {reason}");
        }
        let config: LightingConfig = self.get()?;
        self.backend.apply(&config)?;
        Ok(config)
    }

    /// Run the lighting daemon: keep the saved configuration painted, animate it
    /// when an effect is selected, and reload live whenever the config file
    /// changes (so a UI that writes `rgb.json` is reflected immediately). The
    /// loop re-asserts the hardware every frame, which also restores the LEDs
    /// after a suspend/resume that clears the controller. Never returns on a
    /// supported device.
    pub fn run(&self) -> Result<()> {
        if let Some(reason) = self.backend.unsupported_reason() {
            bail!("RGB unsupported: {reason}");
        }

        let count: usize = self.backend.target_count().max(1);
        let mut effects: EffectState = EffectState::default();
        let mut config: LightingConfig = self.get()?;
        let mut seen: Option<SystemTime> = config_mtime(&self.config_path);
        let start: Instant = Instant::now();

        loop {
            // A pinned charge indicator (set by `charge-indicator on`, e.g. from
            // a wake-on-charge-attach hook) wins over the normal configuration
            // until cleared: paint only it, at a cheap 1s cadence, and skip the
            // rest of the loop entirely so it is never raced by config reload
            // or animation. See `crate::charging`.
            if let Some(indicator) = charging::load(&self.charge_path) {
                if let Err(error) = self.backend.apply(&indicator.to_config()) {
                    eprintln!("armada-rgb: charge indicator apply failed: {error:#}");
                }
                sleep(Duration::from_secs(1));
                continue;
            }

            let current: Option<SystemTime> = config_mtime(&self.config_path);
            if current != seen {
                seen = current;
                match self.get() {
                    Ok(reloaded) => config = reloaded,
                    Err(error) => eprintln!("armada-rgb: keeping previous config: {error:#}"),
                }
            }

            // Static (and disabled) reuse the exact one-shot path so a saved
            // solid color — including any config-level correction — is honored.
            if config.effect.is_static() {
                if let Err(error) = self.backend.apply(&config) {
                    eprintln!("armada-rgb: apply failed: {error:#}");
                }
                sleep(Duration::from_secs_f64(config.effect.frame_interval(FPS)));
                continue;
            }

            if !config.enabled {
                if let Err(error) = self.backend.apply(&config) {
                    eprintln!("armada-rgb: blank failed: {error:#}");
                }
                sleep(Duration::from_secs(1));
                continue;
            }

            let t: f64 = start.elapsed().as_secs_f64();
            let (frame, brightness) = effects.render(
                config.effect,
                config.rgb(),
                config.brightness,
                config.speed,
                t,
                count,
            );
            if let Err(error) = self.backend.render(&frame.expand(count), brightness) {
                eprintln!("armada-rgb: render failed: {error:#}");
            }
            sleep(Duration::from_secs_f64(config.effect.frame_interval(FPS)));
        }
    }
}

fn config_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|meta| meta.modified()).ok()
}
