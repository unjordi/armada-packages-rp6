//! RGB lighting support for Armada devices.

mod backend;
mod config;
mod controller;
mod correction;
mod effects;
mod runtime;
mod state;

pub use backend::{ChannelBackend, LightingBackend, MulticolorBackend};
pub use controller::Controller;
pub use correction::ColorCorrection;
pub use effects::{Effect, EffectState, Frame};
pub use state::LightingConfig;
