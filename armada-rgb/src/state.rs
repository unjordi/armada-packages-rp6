use crate::effects::Effect;
use crate::ColorCorrection;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

const CONFIG_VERSION: u32 = 1;
const DEFAULT_SPEED: u16 = 100;
const MAX_SPEED: u16 = 1000;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LightingConfig {
    pub version: u32,
    pub enabled: bool,
    pub brightness: u8,
    pub color: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correction: Option<ColorCorrection>,
    /// Animation applied on top of the base color. `static` (the default)
    /// keeps the original solid-color behaviour and is omitted when saved.
    #[serde(default, skip_serializing_if = "Effect::is_static")]
    pub effect: Effect,
    /// Animation speed as a percentage (100 = default). Ignored by `static`.
    #[serde(default = "default_speed", skip_serializing_if = "is_default_speed")]
    pub speed: u16,
    /// Modifier, ORTHOGONAL to `effect`/`color`: when true, the brightness
    /// that actually gets painted is scaled by the live screen backlight
    /// percentage, on top of whatever effect/color is active (breathing,
    /// rainbow, a plain static color, ...). This is a toggle on top of the
    /// user's existing choice, not a replacement effect — see
    /// `Controller::run`, which applies the scaling in the one shared write
    /// path rather than any single effect.
    #[serde(default, skip_serializing_if = "is_false")]
    pub sync_brightness: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn default_speed() -> u16 {
    DEFAULT_SPEED
}

fn is_default_speed(speed: &u16) -> bool {
    *speed == DEFAULT_SPEED
}

impl Default for LightingConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            enabled: false,
            brightness: 25,
            color: "FFFFFF".into(),
            correction: None,
            effect: Effect::Static,
            speed: DEFAULT_SPEED,
            sync_brightness: false,
        }
    }
}

impl LightingConfig {
    pub fn validate(mut self) -> Result<Self> {
        if self.version != CONFIG_VERSION {
            bail!("unsupported config version {}", self.version);
        }
        if self.brightness > 100 {
            bail!("brightness must be between 0 and 100");
        }
        if self.color.len() != 6 || !self.color.bytes().all(|c| c.is_ascii_hexdigit()) {
            bail!("color must be six hexadecimal RGB digits");
        }
        if let Some(correction) = &self.correction {
            correction.validate()?;
        }
        if self.speed == 0 || self.speed > MAX_SPEED {
            bail!("speed must be between 1 and {MAX_SPEED}");
        }

        self.color.make_ascii_uppercase();
        Ok(self)
    }

    pub(crate) fn rgb(&self) -> [u8; 3] {
        let red: u8 = u8::from_str_radix(&self.color[0..2], 16).expect("validated color");
        let green: u8 = u8::from_str_radix(&self.color[2..4], 16).expect("validated color");
        let blue: u8 = u8::from_str_radix(&self.color[4..6], 16).expect("validated color");
        [red, green, blue]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_and_normalizes_config() {
        let old_config: LightingConfig = serde_json::from_str(
            r#"{"version":1,"enabled":true,"brightness":25,"color":"FFFFFF"}"#,
        )
        .unwrap();
        assert!(old_config.correction.is_none());
        assert!(!old_config.sync_brightness, "must default to off for old saves");

        let config: LightingConfig = LightingConfig {
            color: "a1b2c3".into(),
            ..LightingConfig::default()
        }
        .validate()
        .unwrap();
        assert_eq!(config.color, "A1B2C3");

        for color in ["fff", "GG0000", "0000000"] {
            let config: LightingConfig = LightingConfig {
                color: color.into(),
                ..LightingConfig::default()
            };
            assert!(config.validate().is_err());
        }

        let brightness: LightingConfig = LightingConfig {
            brightness: 101,
            ..LightingConfig::default()
        };
        assert!(brightness.validate().is_err());

        let version: LightingConfig = LightingConfig {
            version: 2,
            ..LightingConfig::default()
        };
        assert!(version.validate().is_err());
    }

    #[test]
    fn sync_brightness_is_a_modifier_independent_of_effect_and_omitted_when_off() {
        let synced: LightingConfig = LightingConfig {
            effect: Effect::Rainbow,
            sync_brightness: true,
            ..LightingConfig::default()
        }
        .validate()
        .unwrap();
        assert!(synced.sync_brightness);
        assert_eq!(synced.effect, Effect::Rainbow, "orthogonal: does not replace the effect");
        assert!(serde_json::to_string(&synced).unwrap().contains("\"sync_brightness\":true"));

        // Off is the default and stays omitted, same as `effect: static`.
        let plain: LightingConfig = LightingConfig::default().validate().unwrap();
        assert!(!serde_json::to_string(&plain).unwrap().contains("sync_brightness"));

        let round_tripped: LightingConfig =
            serde_json::from_str(&serde_json::to_string(&synced).unwrap()).unwrap();
        assert_eq!(round_tripped, synced);
    }
}
