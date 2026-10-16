//! The obs-websocket v5 compatibility server (#71): settings, what
//! `websocket_status` reports and what `websocket_enable` takes. The
//! server speaks OBS's WebSocket protocol so tools written for OBS (Stream
//! Deck, Touch Portal, Streamer.bot, bots, scripts) drive Isocast; it is a
//! client of the same method table as the CLI and MCP.
//!
//! The settings belong to this Mac's engine, not to a profile (as in OBS):
//! they are kept beside the live setup (`setup.websocket.json`), and the
//! password in the Keychain.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The port obs-websocket listens on unless told otherwise.
pub const DEFAULT_PORT: u16 = 4455;
/// Where it listens unless told otherwise: this Mac only.
pub const DEFAULT_BIND: &str = "127.0.0.1";

/// The server's settings, as kept between runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebsocketSettings {
    /// Whether the server runs (off until turned on).
    #[serde(default)]
    pub enabled: bool,
    /// The TCP port (4455, OBS's default, if absent).
    #[serde(default = "default_port")]
    pub port: u16,
    /// The address it listens on: 127.0.0.1 (this Mac only, the default),
    /// or an address of this Mac, or 0.0.0.0 for every network.
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Whether clients must give the password (kept in the Keychain). On
    /// unless turned off with `no_password`; while on and no password can
    /// be read, the server doesn't start.
    #[serde(default = "default_true")]
    pub authentication: bool,
}

fn default_true() -> bool {
    true
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn default_bind() -> String {
    DEFAULT_BIND.into()
}

impl Default for WebsocketSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            port: DEFAULT_PORT,
            bind: DEFAULT_BIND.into(),
            authentication: true,
        }
    }
}

/// What `websocket_enable` changes; anything left out stays as it was.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebsocketEnableParams {
    /// The TCP port, 1–65535.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// The address to listen on (an IPv4 or IPv6 address of this Mac;
    /// 0.0.0.0 for every network).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
    /// The password clients authenticate with, kept in the Keychain. Left
    /// out, the stored one stays; when none is stored one is generated and
    /// returned once as `generated_password`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Remove the password: any program on this Mac that reaches the port
    /// may connect, except web pages (connections with an Origin header
    /// are refused). Only with a loopback bind (127.0.0.1 or ::1).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_password: bool,
}

/// What `websocket_status`, `websocket_enable` and `websocket_disable`
/// return.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WebsocketStatus {
    /// Whether it is turned on.
    pub enabled: bool,
    /// Whether it is listening now.
    pub running: bool,
    pub port: u16,
    pub bind: String,
    /// Where clients connect (`ws://ADDRESS:PORT`).
    pub url: String,
    /// Whether clients must give the password.
    pub authentication: bool,
    /// Clients connected now.
    pub clients: u32,
    /// The obs-websocket version it speaks.
    pub obs_websocket_version: String,
    /// The rpcVersion it speaks.
    pub rpc_version: u32,
    /// Why it isn't running though turned on (the port is taken, the
    /// password can't be read).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// The password `websocket_enable` just generated, shown this once;
    /// enter it in the OBS client. Never in `websocket_status`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_password: Option<String>,
}

impl WebsocketSettings {
    /// What is wrong with these settings, naming the field.
    pub fn problem(&self) -> Option<String> {
        if self.port == 0 {
            return Some("port: 0 is not a port; give 1-65535 (4455 is OBS's)".into());
        }
        let Ok(ip) = self.bind.parse::<std::net::IpAddr>() else {
            return Some(format!(
                "bind: {:?} is not an IP address; give 127.0.0.1 (this Mac only), an address \
                 of this Mac, or 0.0.0.0 (every network)",
                self.bind
            ));
        };
        if !self.authentication && !ip.is_loopback() {
            return Some(format!(
                "no_password: {} reaches other computers, so it needs a password; give \
                 bind 127.0.0.1 (this Mac only) to go without one, or a password",
                self.bind
            ));
        }
        None
    }

    /// Where clients connect.
    pub fn url(&self) -> String {
        match self.bind.parse::<std::net::IpAddr>() {
            Ok(std::net::IpAddr::V6(ip)) => format!("ws://[{ip}]:{}", self.port),
            _ => format!("ws://{}:{}", self.bind, self.port),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_problems() {
        let settings: WebsocketSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings, WebsocketSettings::default());
        assert_eq!(settings.url(), "ws://127.0.0.1:4455");
        assert!(settings.problem().is_none());
        let bad = WebsocketSettings {
            bind: "localhost".into(),
            ..settings.clone()
        };
        assert!(bad.problem().unwrap().starts_with("bind:"));
        let zero = WebsocketSettings {
            port: 0,
            ..settings.clone()
        };
        assert!(zero.problem().unwrap().starts_with("port:"));
        // No password only on this Mac (#74).
        let open = WebsocketSettings {
            authentication: false,
            ..settings.clone()
        };
        assert!(open.problem().is_none());
        for bind in ["0.0.0.0", "192.168.1.5", "::"] {
            let wide = WebsocketSettings {
                bind: bind.into(),
                ..open.clone()
            };
            assert!(
                wide.problem().unwrap().starts_with("no_password:"),
                "{bind}"
            );
        }
        let local6 = WebsocketSettings {
            bind: "::1".into(),
            ..open
        };
        assert!(local6.problem().is_none());
    }
}
