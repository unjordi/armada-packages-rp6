//! Command line interface for RGB lighting.

use anyhow::Result;
use armada_rgb::{ChargeIndicator, ColorCorrection, Controller, Effect, LightingConfig};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check whether this device has a lighting profile.
    Supported,
    /// Show the saved lighting configuration.
    Get,
    /// Set a solid color and brightness.
    Set {
        #[arg(long)]
        color: String,
        #[arg(long)]
        brightness: u8,
        /// RGB correction trigger and channel reductions.
        #[arg(long, value_name = "TRIGGER:RED,GREEN,BLUE")]
        correction: Option<ColorCorrection>,
        /// Animation: static, breathing, color_cycle, rainbow, load, battery,
        /// screen_sync. (Brightness-follows-backlight is NOT an effect — it is
        /// the orthogonal `sync-brightness on/off` toggle, combinable with any
        /// of these.)
        #[arg(long, value_parser = parse_effect)]
        effect: Option<Effect>,
        /// Animation speed as a percentage (100 = default).
        #[arg(long)]
        speed: Option<u16>,
    },
    /// Turn the stick lights off and save that state.
    Off,
    /// Apply the saved configuration.
    Apply,
    /// Run the lighting daemon: keep the saved configuration painted, animate
    /// the selected effect, and reload live when the config changes.
    Run,
    /// Paint (or clear) the fixed indicator used while the device is asleep.
    /// The hardware holds a plain color without CPU help, so this is what a
    /// suspend/wake hook calls right before (and after) the brief
    /// wake-on-charge-attach window during deep sleep — see the `armada-rgb`
    /// README for the full contract.
    ChargeIndicator {
        #[command(subcommand)]
        state: ChargeIndicatorState,
    },
    /// Toggle brightness-follows-screen-backlight as a MODIFIER on top of
    /// whatever color/effect is already configured — not a replacement
    /// effect. Persists (`sync_brightness` in the saved configuration) and
    /// takes effect on `run`'s next tick (at most ~1s later) without
    /// touching color/effect/brightness.
    SyncBrightness {
        #[command(subcommand)]
        state: SyncBrightnessState,
    },
}

#[derive(Subcommand)]
enum ChargeIndicatorState {
    /// Paint the indicator now (works even if `run` is not active) and pin
    /// it so `run`, if thawed during the same window, does not repaint the
    /// normal effect over it.
    On {
        /// Defaults to a dim amber if omitted.
        #[arg(long)]
        color: Option<String>,
        /// Percentage 0-100. Defaults to a low, non-distracting level.
        #[arg(long)]
        brightness: Option<u8>,
    },
    /// Clear the pin and restore the saved configuration immediately.
    Off,
}

#[derive(Subcommand)]
enum SyncBrightnessState {
    /// Scale the painted brightness by the live screen backlight percentage.
    On,
    /// Use the configured brightness as-is (the default).
    Off,
}

fn parse_effect(value: &str) -> Result<Effect, String> {
    value.parse()
}

fn main() -> Result<()> {
    let cli: Cli = Cli::parse();
    let controller: Controller = Controller::from_env();

    match cli.command {
        Command::Supported => {
            if !controller.is_supported() {
                std::process::exit(1);
            }
        }
        Command::Get => {
            let config: LightingConfig = controller.get()?;
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
        Command::Set {
            color,
            brightness,
            correction,
            effect,
            speed,
        } => {
            let mut config: LightingConfig = controller.get()?;
            config.enabled = true;
            config.color = color;
            config.brightness = brightness;
            if let Some(correction) = correction {
                config.correction = Some(correction);
            }
            if let Some(effect) = effect {
                config.effect = effect;
            }
            if let Some(speed) = speed {
                config.speed = speed;
            }
            let config: LightingConfig = controller.set(config)?;
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
        Command::Off => {
            let config: LightingConfig = controller.off()?;
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
        Command::Apply => {
            if let Some(reason) = controller.apply()? {
                eprintln!("RGB unsupported: {reason}");
            }
        }
        Command::Run => {
            controller.run()?;
        }
        Command::ChargeIndicator { state } => match state {
            ChargeIndicatorState::On { color, brightness } => {
                let indicator: ChargeIndicator = controller.charge_indicator_on(color, brightness)?;
                println!("{}", serde_json::to_string_pretty(&indicator)?);
            }
            ChargeIndicatorState::Off => {
                let config: LightingConfig = controller.charge_indicator_off()?;
                println!("{}", serde_json::to_string_pretty(&config)?);
            }
        },
        Command::SyncBrightness { state } => {
            let mut config: LightingConfig = controller.get()?;
            config.sync_brightness = matches!(state, SyncBrightnessState::On);
            let config: LightingConfig = controller.set(config)?;
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
    }
    Ok(())
}
