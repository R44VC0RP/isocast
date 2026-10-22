//! Remote control settings and public identities. Private keys live only in
//! the platform credential store; a pin is canonical Ed25519 SPKI, base64.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const DEFAULT_PORT: u16 = 47470;

pub fn on_link(ip: std::net::IpAddr, prefixes: &[(std::net::IpAddr, u8)]) -> bool {
    use std::net::IpAddr;
    let ip = match ip {
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(ip)),
        ip => ip,
    };
    prefixes.iter().any(|(address, bits)| match (ip, *address) {
        (IpAddr::V4(ip), IpAddr::V4(address)) if *bits > 0 && *bits <= 32 => {
            let mask = u32::MAX << (32 - *bits);
            u32::from(ip) & mask == u32::from(address) & mask
        }
        (IpAddr::V6(ip), IpAddr::V6(address)) if *bits > 0 && *bits <= 128 => {
            let mask = u128::MAX << (128 - *bits);
            u128::from(ip) & mask == u128::from(address) & mask
        }
        _ => false,
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteEnableParams {
    pub port: Option<u16>,
    /// A numeric address of an engine interface (default: 0.0.0.0).
    pub bind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteRevokeParams {
    pub client_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteIdentity {
    pub id: String,
    pub name: String,
    pub spki: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemotePeer {
    pub id: String,
    pub name: String,
    pub spki: String,
    pub paired_at: u64,
    pub last_seen: Option<u64>,
    /// Client-side only: the last explicitly paired engine address.
    pub address: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteSettings {
    #[serde(default)]
    pub enabled: bool,
    pub port: Option<u16>,
    pub bind: Option<String>,
    pub identity: Option<RemoteIdentity>,
    #[serde(default)]
    pub peers: Vec<RemotePeer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteStatus {
    pub enabled: bool,
    pub running: bool,
    pub identity: Option<RemoteIdentity>,
    pub address: Option<String>,
    pub pairing_open: bool,
    pub error: Option<String>,
}

// No Debug: even a local pairing response must never enter an audit log.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemotePairWindow {
    pub code: String,
    pub expires_in_seconds: u32,
    pub identity: RemoteIdentity,
    pub address: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteAuditPeer {
    pub transport: String,
    pub client_id: String,
    pub name: String,
    pub addr: String,
}
