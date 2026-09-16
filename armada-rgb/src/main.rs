//! Command line interface for RGB lighting.

use anyhow::Result;
use armada_rgb::{ColorCorrection, Controller, Effect, LightingConfig};
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
        /// Animation: static, breathing, color_cycle, rainbow, load, battery.
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
    }
    Ok(())
}
