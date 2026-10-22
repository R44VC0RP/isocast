//! Systems without a DNS-SD backend yet (Linux: Avahi, later).

use anyhow::Result;

use crate::{OnEvent, Txt};

pub const SUPPORTED: bool = false;

pub struct Registration;
pub struct Watch;

impl Registration {
    pub fn name(&self) -> String {
        String::new()
    }
}

fn unsupported() -> anyhow::Error {
    anyhow::anyhow!(
        "finding engines on the network isn't supported on {} yet",
        std::env::consts::OS
    )
}

pub fn register(_name: &str, _port: u16, _txt: &Txt) -> Result<Registration> {
    Err(unsupported())
}

pub fn watch(_on_event: OnEvent) -> Result<Watch> {
    Err(unsupported())
}
