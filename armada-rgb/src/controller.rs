use crate::{config, runtime, EffectState, LightingBackend, LightingConfig};
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime};

/// Frames per second for animated effects that do not set their own cadence.
const FPS: u32 = 30;

pub struct Controller {
    config_path: PathBuf,
    backend: LightingBackend,
}

impl Controller {
    pub fn new(config_path: PathBuf, backend: LightingBackend) -> Self {
        Self {
            config_path,
            backend,
        }
    }

    pub fn from_env() -> Self {
        let (config_path, backend): (PathBuf, LightingBackend) = runtime::from_env();
        Self::new(config_path, backend)
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
            let current: Option<SystemTime> = config_mtime(&self.config_path);
            if current != seen {
                seen = current;
                match self.get() {
                    Ok(reloaded) => config = reloaded,
                    Err(error) => eprintln!("armada-rgb: keeping previous config: {error:#}"),
                }
            }

            // Static (and disabled) reuse the exact one-shot path so a saved
            // solid color — including any config-level correction — is
            // honored. `sync_brightness` is applied here too: it is a
            // modifier over the FINAL painted brightness, not part of any
            // one effect (see `LightingConfig::sync_brightness`).
            if config.effect.is_static() {
                let mut painted: LightingConfig = config.clone();
                painted.brightness = effects.scale_for_sync(config.brightness, config.sync_brightness);
                if let Err(error) = self.backend.apply(&painted) {
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
            // `sync_brightness` scales whatever brightness the effect just
            // computed (a plain color, a breath, rainbow, ...) — it composes
            // with any effect rather than being one itself.
            let brightness: u8 = effects.scale_for_sync(brightness, config.sync_brightness);
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
