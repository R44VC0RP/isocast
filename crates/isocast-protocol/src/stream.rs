//! Live streaming: the `[stream]` section of a setup, the RTMP server
//! address, and what `status` reports about a stream.
//!
//! The stream key is a secret and is never part of a setup, a status or an
//! error: it lives in the Keychain (`isocast stream key set`), filed under
//! [`StreamSettings::key_account`].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::Issues;

/// Streaming the program (the canvas and the program mix) to a service
/// over RTMP or RTMPS, as H.264 and AAC.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StreamSettings {
    /// Where to stream. A service fills in its ingest server; `custom`
    /// needs `server`.
    pub service: StreamService,
    /// The ingest server without the stream key: `rtmp://host[:port]/app`
    /// or `rtmps://host[:port]/app`. Required for `custom` and `kick` (each
    /// Kick channel has its own); for the other services it replaces their
    /// default server (for example a nearer Twitch ingest).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// Video bitrate in kb/s, held near constant as services ask.
    #[serde(default = "default_video_bitrate")]
    pub video_bitrate_kbps: u32,
    /// How the encoder keeps to `video_bitrate_kbps`: `constant` (the
    /// default: the full rate every second, padding still scenes, as
    /// YouTube's and Twitch's guidelines ask) or `average` (fewer bits when
    /// the picture is still, overshooting very busy scenes a little less).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_control: Option<RateControl>,
    /// Seconds between keyframes. Twitch and YouTube ask for 2.
    #[serde(default = "default_keyframe_interval")]
    pub keyframe_interval_seconds: u32,
    /// AAC audio bitrate in kb/s, held constant (silence too, as services
    /// expect): 64, 96, 128, 160, 192, 256 or 320.
    #[serde(default = "default_audio_bitrate")]
    pub audio_bitrate_kbps: u32,
    /// The size sent, if not the canvas's: the encoder scales the program
    /// to it. Without it, a canvas larger than 1920×1080 (1080×1920
    /// upright) is scaled down to fit, as most services take 1080p at most.
    /// Each side is an even number from 16 to 4096.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Connect again after the connection drops.
    #[serde(default, skip_serializing_if = "Reconnect::is_default")]
    pub reconnect: Reconnect,
    /// Whether `stream start` (without `only`) streams to this section's
    /// own service, the main destination (id `main`). Its encode settings
    /// stay the other destinations' defaults while it is off.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub enabled: bool,
    /// More places to stream the same program to at once, each with its
    /// own key, state and reconnects. A destination with the same encode
    /// settings as another shares its encoder; one with its own
    /// `video_bitrate_kbps` or size gets an encoder of its own if this
    /// Mac's encoders have room.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub destinations: Vec<StreamDestination>,
}

/// The id of the destination the `[stream]` section itself describes.
pub const MAIN_DESTINATION: &str = "main";

/// The most destinations a setup may have, the main one included.
pub const MAX_DESTINATIONS: usize = 8;

/// Another place to stream the program to (see
/// [`StreamSettings::destinations`]). What it doesn't set comes from the
/// `[stream]` section: rate control, keyframes, audio and reconnecting
/// always do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StreamDestination {
    /// Its id (lowercase letters, digits and dashes; not `main`), as
    /// `isocast stream start --only` and the key commands name it.
    pub id: String,
    pub service: StreamService,
    /// The ingest server without the stream key, as for `[stream]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// Whether `stream start` (without `only`) streams here.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub enabled: bool,
    /// Its own video bitrate; `[stream]`'s if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_bitrate_kbps: Option<u32>,
    /// Its own size sent; `[stream]`'s if absent. Give both or neither.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

/// A destination with everything it streams with filled in (see
/// [`StreamSettings::resolve`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Destination {
    pub id: String,
    pub enabled: bool,
    /// Its settings as a single stream: `destinations` empty.
    pub settings: StreamSettings,
    /// The Keychain account its key is filed under.
    pub key_account: String,
}

/// One H.264 encode of the program and the destinations it feeds (see
/// [`StreamSettings::encodes`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamEncode {
    pub ids: Vec<String>,
    pub width: u32,
    pub height: u32,
    pub video_bitrate_kbps: u32,
}

fn is_true(value: &bool) -> bool {
    *value
}

fn default_video_bitrate() -> u32 {
    6000
}

fn default_keyframe_interval() -> u32 {
    2
}

fn default_audio_bitrate() -> u32 {
    160
}

/// The largest size sent unless the setup asks for more: 1080p (or
/// 1080×1920 upright).
const MAX_DEFAULT_SIZE: (u32, u32) = (1920, 1080);

/// The AAC bitrates a stream can use, in kb/s.
pub const AUDIO_BITRATES: [u32; 7] = [64, 96, 128, 160, 192, 256, 320];

/// Reconnecting after the connection to the server drops.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Reconnect {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Attempts before giving up (the stream then shows `failed`).
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Seconds to wait before each attempt.
    #[serde(default = "default_retry_delay")]
    pub delay_seconds: u32,
}

fn yes() -> bool {
    true
}

fn default_max_retries() -> u32 {
    25
}

fn default_retry_delay() -> u32 {
    2
}

impl Default for Reconnect {
    fn default() -> Self {
        Self {
            enabled: true,
            max_retries: default_max_retries(),
            delay_seconds: default_retry_delay(),
        }
    }
}

impl Reconnect {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// How a stream's encoder keeps to its bitrate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RateControl {
    /// About the bitrate on average: fewer bits for still pictures.
    Average,
    /// The bitrate every second, still pictures padded.
    Constant,
}

/// A streaming service. Presets use the service's documented RTMPS ingest
/// (see `docs/streaming.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StreamService {
    /// Any RTMP or RTMPS server, given as `server`.
    Custom,
    Twitch,
    Youtube,
    /// Kick: give your channel's Stream URL (Creator Dashboard → Channel →
    /// Stream URL and Key) as `server`.
    Kick,
    Facebook,
}

/// Every service, for messages that list the options.
pub const STREAM_SERVICES: [StreamService; 5] = [
    StreamService::Custom,
    StreamService::Twitch,
    StreamService::Youtube,
    StreamService::Kick,
    StreamService::Facebook,
];

impl StreamService {
    /// The name used in setups and on the command line.
    pub fn id(self) -> &'static str {
        match self {
            Self::Custom => "custom",
            Self::Twitch => "twitch",
            Self::Youtube => "youtube",
            Self::Kick => "kick",
            Self::Facebook => "facebook",
        }
    }

    /// Parse a service name, listing the valid ones if it isn't one.
    pub fn from_id(id: &str) -> Result<Self, String> {
        STREAM_SERVICES
            .into_iter()
            .find(|service| service.id() == id.trim().to_ascii_lowercase())
            .ok_or_else(|| {
                let ids: Vec<&str> = STREAM_SERVICES.iter().map(|s| s.id()).collect();
                format!("unknown service {id:?}; services: {}", ids.join(", "))
            })
    }

    /// The service's default ingest server, if it has one for everybody.
    pub fn default_server(self) -> Option<&'static str> {
        match self {
            // Twitch's "Default" (global) ingest, from
            // https://ingest.twitch.tv/ingests (url_template_secure).
            Self::Twitch => Some("rtmps://ingest.global-contribute.live-video.net/app"),
            // YouTube Live's primary RTMPS ingest (port 443).
            Self::Youtube => Some("rtmps://a.rtmps.youtube.com/live2"),
            // Facebook's Live Video API: RTMPS only.
            Self::Facebook => Some("rtmps://rtmp-api.facebook.com/rtmp"),
            // Each Kick channel has its own ingest host.
            Self::Kick | Self::Custom => None,
        }
    }

    /// The service only accepts RTMPS.
    pub fn requires_tls(self) -> bool {
        matches!(self, Self::Facebook | Self::Kick)
    }

    /// The application every ingest server of the service uses (the path
    /// before the stream key in the URLs the services show).
    fn app(self) -> Option<&'static str> {
        match self {
            Self::Twitch | Self::Kick => Some("app"),
            Self::Youtube => Some("live2"),
            Self::Facebook => Some("rtmp"),
            Self::Custom => None,
        }
    }
}

impl StreamSettings {
    /// Every destination, the main one first, as the single streams they
    /// are.
    pub fn resolve(&self) -> Vec<Destination> {
        let base = StreamSettings {
            destinations: Vec::new(),
            enabled: true,
            ..self.clone()
        };
        let mut out = vec![Destination {
            id: MAIN_DESTINATION.into(),
            enabled: self.enabled,
            key_account: base.key_account(),
            settings: base.clone(),
        }];
        for extra in &self.destinations {
            let mut settings = StreamSettings {
                service: extra.service,
                server: extra.server.clone(),
                video_bitrate_kbps: extra.video_bitrate_kbps.unwrap_or(base.video_bitrate_kbps),
                ..base.clone()
            };
            if extra.width.is_some() || extra.height.is_some() {
                settings.width = extra.width;
                settings.height = extra.height;
            }
            out.push(Destination {
                id: extra.id.clone(),
                enabled: extra.enabled,
                key_account: destination_key_account(
                    &extra.id,
                    extra.service,
                    extra.server.as_deref(),
                ),
                settings,
            });
        }
        out
    }

    /// The H.264 encodes the enabled destinations need: those sending the
    /// same size at the same video bitrate share one (everything else
    /// they encode with is the main destination's), the main one's
    /// first.
    pub fn encodes(&self, canvas: (u32, u32)) -> Vec<StreamEncode> {
        let mut out: Vec<StreamEncode> = Vec::new();
        for destination in self.resolve().into_iter().filter(|d| d.enabled) {
            let (width, height) = destination.settings.output_size(canvas);
            let kbps = destination.settings.video_bitrate_kbps;
            match out
                .iter_mut()
                .find(|e| (e.width, e.height, e.video_bitrate_kbps) == (width, height, kbps))
            {
                Some(encode) => encode.ids.push(destination.id),
                None => out.push(StreamEncode {
                    ids: vec![destination.id],
                    width,
                    height,
                    video_bitrate_kbps: kbps,
                }),
            }
        }
        out
    }

    /// The destination `id`, if there is one.
    pub fn destination(&self, id: &str) -> Option<Destination> {
        self.resolve().into_iter().find(|d| d.id == id)
    }

    /// Every destination's id, the main one first.
    pub fn destination_ids(&self) -> Vec<String> {
        std::iter::once(MAIN_DESTINATION.to_string())
            .chain(self.destinations.iter().map(|d| d.id.clone()))
            .collect()
    }

    /// The rate control in effect: the setup's, else `constant`.
    pub fn rate_control(&self) -> RateControl {
        self.rate_control.unwrap_or(RateControl::Constant)
    }

    /// The server to connect to: `server`, else the service's default.
    pub fn server_url(&self) -> Option<&str> {
        self.server.as_deref().or(self.service.default_server())
    }

    /// The parsed server, or why it can't be used.
    pub fn server(&self) -> Result<ServerUrl, String> {
        let Some(url) = self.server_url() else {
            return Err(format!(
                "stream.server: {} needs a server{}",
                self.service.id(),
                match self.service {
                    StreamService::Kick =>
                        ": your channel's Stream URL from Kick's Creator \
                                            Dashboard (Channel → Stream URL and Key)",
                    _ => ", like rtmp://host/app or rtmps://host/app",
                }
            ));
        };
        let mut server = ServerUrl::parse(url).map_err(|err| format!("stream.server: {err}"))?;
        // Kick shows its Stream URL with or without the `app` path.
        if self.service == StreamService::Kick && server.app.is_empty() {
            server.app = "app".into();
        }
        Ok(server)
    }

    /// The size the stream sends for a `canvas` of this size: `width` and
    /// `height` if set, else the canvas, scaled down (keeping its shape, to
    /// even sizes) to fit 1920×1080 (1080×1920 if upright).
    pub fn output_size(&self, canvas: (u32, u32)) -> (u32, u32) {
        if let (Some(width), Some(height)) = (self.width, self.height) {
            return (width, height);
        }
        let (width, height) = canvas;
        let (max_w, max_h) = if height > width {
            (MAX_DEFAULT_SIZE.1, MAX_DEFAULT_SIZE.0)
        } else {
            MAX_DEFAULT_SIZE
        };
        let scale = (f64::from(max_w) / f64::from(width.max(1)))
            .min(f64::from(max_h) / f64::from(height.max(1)))
            .min(1.0);
        let even = |n: u32| ((f64::from(n) * scale / 2.0).round() as u32 * 2).max(2);
        if scale >= 1.0 {
            (width, height)
        } else {
            (even(width), even(height))
        }
    }

    /// The Keychain account the stream key is filed under: the service, or
    /// for `custom` the server (so each server keeps its own key).
    pub fn key_account(&self) -> String {
        key_account(self.service, self.server.as_deref())
    }

    /// What follows a preset's application in `server` (`…/app/live_123`):
    /// most likely the stream key, pasted with the server. `check` refuses
    /// it, and anything showing the server must hide it.
    pub fn pasted_key(&self) -> Option<String> {
        let app = self.service.app()?;
        let server = self.server().ok()?;
        server
            .app
            .strip_prefix(&format!("{app}/"))
            .filter(|key| !key.is_empty())
            .map(str::to_string)
    }

    pub(crate) fn check(&self, issues: &mut Issues) {
        self.check_server("stream", issues);
        check_size("stream", self.width, self.height, issues);
        check_video_bitrate("stream", self.video_bitrate_kbps, issues);
        self.check_rest(issues);
        let mut ids = std::collections::HashSet::from([MAIN_DESTINATION]);
        if self.destinations.len() + 1 > MAX_DESTINATIONS {
            issues.add(
                "stream.destinations",
                format!(
                    "{} destinations with the main one; at most {MAX_DESTINATIONS}",
                    self.destinations.len() + 1
                ),
            );
        }
        let resolved = self.resolve();
        for (i, (extra, destination)) in self.destinations.iter().zip(&resolved[1..]).enumerate() {
            let path = format!("stream.destinations[{i}]");
            if extra.id == MAIN_DESTINATION {
                issues.add(
                    format!("{path}.id"),
                    "\"main\" is the [stream] section's own destination; pick another id",
                );
            } else {
                issues.check_id(format!("{path}.id"), &extra.id, &mut ids);
            }
            destination.settings.check_server(&path, issues);
            if extra.width.is_some() || extra.height.is_some() {
                check_size(&path, extra.width, extra.height, issues);
            }
            if let Some(kbps) = extra.video_bitrate_kbps {
                check_video_bitrate(&path, kbps, issues);
            }
        }
    }

    /// The server's problems, under `{prefix}.server`.
    fn check_server(&self, prefix: &str, issues: &mut Issues) {
        let field = format!("{prefix}.server");
        match self.server() {
            Ok(server) => {
                if self.service.requires_tls() && !server.secure {
                    issues.add(
                        &field,
                        format!(
                            "{} only accepts RTMPS; use an rtmps:// address",
                            self.service.id()
                        ),
                    );
                }
                // A key pasted with the server (the services show them
                // together) would end up in the setup, status and logs. The
                // message mustn't repeat it.
                if let Some(app) = self.service.app()
                    && self.pasted_key().is_some()
                {
                    let alone = ServerUrl {
                        app: app.into(),
                        ..server.clone()
                    };
                    issues.add(
                        &field,
                        format!(
                            "this looks like it has the stream key after /{app}/; give the server \
                             alone ({alone}) and keep the key in the Keychain with `isocast \
                             stream key set`"
                        ),
                    );
                }
                if server.app.is_empty() {
                    issues.add(
                        &field,
                        "give the application after the host, like rtmp://host/app (the stream \
                         key is set separately with `isocast stream key set`)",
                    );
                }
            }
            Err(err) => {
                let message = err.strip_prefix("stream.server: ").unwrap_or(&err);
                issues.add(&field, message);
            }
        }
    }

    /// Keyframes, audio and reconnecting: the main destination's alone.
    fn check_rest(&self, issues: &mut Issues) {
        if !(1..=10).contains(&self.keyframe_interval_seconds) {
            issues.add(
                "stream.keyframe_interval_seconds",
                format!(
                    "{} is outside 1 to 10 seconds; services ask for 2",
                    self.keyframe_interval_seconds
                ),
            );
        }
        if !AUDIO_BITRATES.contains(&self.audio_bitrate_kbps) {
            let options: Vec<String> = AUDIO_BITRATES.iter().map(u32::to_string).collect();
            issues.add(
                "stream.audio_bitrate_kbps",
                format!(
                    "{} is not an AAC bitrate this encoder offers; use one of {}",
                    self.audio_bitrate_kbps,
                    options.join(", ")
                ),
            );
        }
        if !(1..=300).contains(&self.reconnect.delay_seconds) {
            issues.add(
                "stream.reconnect.delay_seconds",
                format!(
                    "{} is outside 1 to 300 seconds",
                    self.reconnect.delay_seconds
                ),
            );
        }
        if self.reconnect.max_retries > 1000 {
            issues.add(
                "stream.reconnect.max_retries",
                format!("{} is more than 1000", self.reconnect.max_retries),
            );
        }
    }
}

/// The Keychain account for `service`'s key (see
/// [`StreamSettings::key_account`]).
pub fn key_account(service: StreamService, server: Option<&str>) -> String {
    match (service, server.map(ServerUrl::parse)) {
        (StreamService::Custom, Some(Ok(server))) => server.to_string(),
        (StreamService::Custom, _) => server.unwrap_or_default().trim().to_string(),
        (service, _) => service.id().to_string(),
    }
}

/// The Keychain account for destination `id`'s key: the service's (or
/// custom server's) account with the id, so two Twitch channels keep two
/// keys. The main destination keeps the plain account (see
/// [`key_account`]), as it did before there were more.
pub fn destination_key_account(id: &str, service: StreamService, server: Option<&str>) -> String {
    let base = key_account(service, server);
    if id == MAIN_DESTINATION {
        base
    } else {
        format!("{base} [{id}]")
    }
}

/// A size sent, given as `{prefix}.width` and `.height`.
fn check_size(prefix: &str, width: Option<u32>, height: Option<u32>, issues: &mut Issues) {
    match (width, height) {
        (None, None) => {}
        (Some(width), Some(height)) => {
            for (field, value) in [("width", width), ("height", height)] {
                // VideoToolbox's H.264 encoder takes at most 4096 a side.
                if !(16..=4096).contains(&value) || value % 2 != 0 {
                    issues.add(
                        format!("{prefix}.{field}"),
                        format!(
                            "{value} must be an even number from 16 to 4096 (the H.264 \
                             encoder's largest side)"
                        ),
                    );
                }
            }
        }
        _ => issues.add(
            format!("{prefix}.width"),
            "give width and height together, or neither to send the canvas (at most 1080p)",
        ),
    }
}

fn check_video_bitrate(prefix: &str, kbps: u32, issues: &mut Issues) {
    if !(200..=50_000).contains(&kbps) {
        issues.add(
            format!("{prefix}.video_bitrate_kbps"),
            format!("{kbps} is outside 200 to 50000 kb/s; services suggest 3000 to 9000 for 1080p"),
        );
    }
}

/// An RTMP server address: `rtmp://host[:port]/app` or `rtmps://…`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerUrl {
    /// RTMPS: RTMP over TLS.
    pub secure: bool,
    pub host: String,
    /// 1935 for RTMP and 443 for RTMPS unless the address gives one.
    pub port: u16,
    /// Everything after the host, like `app` or `live2`, without the
    /// slashes around it.
    pub app: String,
}

impl ServerUrl {
    pub fn parse(url: &str) -> Result<Self, String> {
        let url = url.trim();
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| format!("{url:?} is not an rtmp:// or rtmps:// address"))?;
        let secure = match scheme.to_ascii_lowercase().as_str() {
            "rtmp" => false,
            "rtmps" => true,
            _ => {
                return Err(format!(
                    "{url:?} uses {scheme}://; streaming needs rtmp:// or rtmps://"
                ));
            }
        };
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        if authority.contains('@') {
            return Err("the address must not contain a user name or password".into());
        }
        let default_port = if secure { 443 } else { 1935 };
        let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
            // An IPv6 address: [::1]:1935.
            let (host, after) = bracketed
                .split_once(']')
                .ok_or_else(|| format!("{url:?} has an unclosed [ in its host"))?;
            let port = match after.strip_prefix(':') {
                Some(port) => Some(port),
                None if after.is_empty() => None,
                None => return Err(format!("{url:?} has something after its host: {after:?}")),
            };
            (host.to_string(), port)
        } else {
            match authority.rsplit_once(':') {
                Some((host, port)) => (host.to_string(), Some(port)),
                None => (authority.to_string(), None),
            }
        };
        if host.is_empty() {
            return Err(format!("{url:?} has no host"));
        }
        let port = match port {
            None => default_port,
            Some(port) => port
                .parse::<u16>()
                .ok()
                .filter(|port| *port > 0)
                .ok_or_else(|| format!("{url:?} has a bad port {port:?}"))?,
        };
        Ok(Self {
            secure,
            host: host.to_ascii_lowercase(),
            port,
            app: path.trim_matches('/').to_string(),
        })
    }

    fn default_port(&self) -> u16 {
        if self.secure { 443 } else { 1935 }
    }
}

impl std::fmt::Display for ServerUrl {
    /// The address as sent in RTMP's `connect` (its tcUrl): never with a
    /// stream key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let scheme = if self.secure { "rtmps" } else { "rtmp" };
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        write!(f, "{scheme}://{host}")?;
        if self.port != self.default_port() {
            write!(f, ":{}", self.port)?;
        }
        write!(f, "/{}", self.app)
    }
}

/// Where a stream is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StreamState {
    /// Not streaming.
    Offline,
    /// Connecting to the server for the first time.
    Connecting,
    /// Sending the program.
    Live,
    /// The connection dropped; connecting again.
    Reconnecting,
    /// Stopped by an error (`last_error`) or after the last reconnect
    /// attempt.
    Failed,
}

/// A stream's state and health. Never holds the stream key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StreamStatus {
    /// Which destination: `main` for the `[stream]` section's own, else
    /// the id of one of its `destinations`.
    #[serde(default = "main_id")]
    pub id: String,
    /// Whether `stream start` (without `only`) streams to it.
    #[serde(default = "yes")]
    pub enabled: bool,
    pub state: StreamState,
    pub service: StreamService,
    /// The server, without the stream key (empty if the setup has none).
    pub server: String,
    /// Whether a stream key is stored for this service (or server).
    pub key_set: bool,
    /// When the state last changed (local time, RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// Seconds since the stream started.
    #[serde(default)]
    pub elapsed_seconds: f64,
    /// What was actually sent over the last second, in kb/s.
    #[serde(default)]
    pub video_kbps: f64,
    #[serde(default)]
    pub audio_kbps: f64,
    /// Everything sent, protocol included, over the last second.
    #[serde(default)]
    pub total_kbps: f64,
    /// The picture sent, while streaming: its size and frame rate (0 when
    /// offline).
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub fps: u32,
    /// The bitrates the setup asks for.
    #[serde(default)]
    pub target_video_kbps: u32,
    #[serde(default)]
    pub target_audio_kbps: u32,
    #[serde(default)]
    pub frames_sent: u64,
    /// Video frames that were never sent: dropped because the connection
    /// couldn't keep up, while reconnecting, or because the encoder fell
    /// behind. The program and recordings don't lose them.
    #[serde(default)]
    pub dropped_frames: u64,
    /// Program mix chunks lost before encoding plus encoded AAC packets
    /// discarded during congestion/reconnects. These are stream-only losses.
    #[serde(default)]
    pub audio_dropouts: u64,
    /// Sent video has exceeded 1.3 times its target for at least five seconds.
    /// Clears once the rate recovers or the connection stops being live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate_warning: Option<String>,
    /// How backed up sending is: 0 when the connection keeps up, 1 when
    /// frames are being dropped.
    #[serde(default)]
    pub congestion: f64,
    /// Times the stream connected again after its connection dropped.
    #[serde(default)]
    pub reconnects: u32,
    #[serde(default)]
    pub bytes_sent: u64,
    /// The most recent error, kept after a reconnect succeeds (see
    /// `reconnects`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Why the stored key can't be used, though `key_set` (it was stored by
    /// another build of Isocast, which this one may not read without
    /// asking): store it again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_problem: Option<String>,
}

fn main_id() -> String {
    MAIN_DESTINATION.into()
}

impl StreamStatus {
    /// An offline stream of `destination`.
    pub fn offline_destination(destination: &Destination, key_set: bool) -> Self {
        Self {
            id: destination.id.clone(),
            enabled: destination.enabled,
            ..Self::offline(&destination.settings, key_set)
        }
    }

    /// An offline stream of `settings` (the main destination).
    pub fn offline(settings: &StreamSettings, key_set: bool) -> Self {
        Self {
            id: main_id(),
            enabled: settings.enabled,
            state: StreamState::Offline,
            service: settings.service,
            server: settings
                .server()
                .map(|server| server.to_string())
                .unwrap_or_default(),
            key_set,
            since: None,
            elapsed_seconds: 0.0,
            video_kbps: 0.0,
            audio_kbps: 0.0,
            total_kbps: 0.0,
            width: 0,
            height: 0,
            fps: 0,
            target_video_kbps: settings.video_bitrate_kbps,
            target_audio_kbps: settings.audio_bitrate_kbps,
            frames_sent: 0,
            dropped_frames: 0,
            audio_dropouts: 0,
            bitrate_warning: None,
            congestion: 0.0,
            reconnects: 0,
            bytes_sent: 0,
            last_error: None,
            key_problem: None,
        }
    }

    /// The stream is sending or trying to.
    pub fn is_active(&self) -> bool {
        matches!(
            self.state,
            StreamState::Connecting | StreamState::Live | StreamState::Reconnecting
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_addresses_parse_with_default_ports() {
        let twitch =
            ServerUrl::parse("rtmps://ingest.global-contribute.live-video.net/app/").unwrap();
        assert_eq!(
            twitch,
            ServerUrl {
                secure: true,
                host: "ingest.global-contribute.live-video.net".into(),
                port: 443,
                app: "app".into(),
            }
        );
        assert_eq!(
            twitch.to_string(),
            "rtmps://ingest.global-contribute.live-video.net/app"
        );
        let local = ServerUrl::parse("RTMP://127.0.0.1:19350/live").unwrap();
        assert_eq!((local.secure, local.port), (false, 19350));
        assert_eq!(local.to_string(), "rtmp://127.0.0.1:19350/live");
        let deep = ServerUrl::parse("rtmp://example.com/live/sub?backup=1").unwrap();
        assert_eq!((deep.port, deep.app.as_str()), (1935, "live/sub?backup=1"));
        let v6 = ServerUrl::parse("rtmp://[::1]:1936/live").unwrap();
        assert_eq!((v6.host.as_str(), v6.port), ("::1", 1936));
        assert_eq!(v6.to_string(), "rtmp://[::1]:1936/live");
        for bad in [
            "http://example.com/app",
            "rtmp://",
            "rtmp://host:x/app",
            "rtmp://user:pw@host/app",
            "example.com/app",
        ] {
            assert!(ServerUrl::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn destinations_inherit_main_and_share_encodes() {
        let stream: StreamSettings = serde_json::from_value(serde_json::json!({
            "service": "twitch",
            "video_bitrate_kbps": 6000,
            "destinations": [
                { "id": "yt", "service": "youtube" },
                { "id": "backup", "service": "custom", "server": "rtmp://host/live",
                  "video_bitrate_kbps": 3000, "width": 1280, "height": 720 },
                { "id": "off", "service": "twitch", "enabled": false }
            ]
        }))
        .unwrap();
        let all = stream.resolve();
        assert_eq!(stream.destination_ids(), ["main", "yt", "backup", "off"]);
        // The main destination keeps the account it always had.
        assert_eq!(all[0].key_account, "twitch");
        assert_eq!(all[1].key_account, "youtube [yt]");
        assert_eq!(all[2].key_account, "rtmp://host/live [backup]");
        assert_eq!(all[1].settings.video_bitrate_kbps, 6000);
        assert_eq!(all[2].settings.output_size((1920, 1080)), (1280, 720));
        let encodes = stream.encodes((1920, 1080));
        assert_eq!(encodes.len(), 2);
        assert_eq!(encodes[0].ids, ["main", "yt"]);
        assert_eq!(encodes[1].ids, ["backup"]);
        let mut issues = Issues::default();
        stream.check(&mut issues);
        assert!(issues.0.is_empty(), "{:?}", issues.0);
        // A plain [stream] round-trips without the new fields.
        let plain: StreamSettings =
            serde_json::from_value(serde_json::json!({ "service": "twitch" })).unwrap();
        let value = serde_json::to_value(&plain).unwrap();
        assert!(value.get("enabled").is_none() && value.get("destinations").is_none());
        // Ids are checked, and each destination's server.
        let bad: StreamSettings = serde_json::from_value(serde_json::json!({
            "service": "twitch",
            "destinations": [
                { "id": "main", "service": "youtube" },
                { "id": "Two", "service": "kick" },
                { "id": "a", "service": "custom", "width": 1280 }
            ]
        }))
        .unwrap();
        let mut issues = Issues::default();
        bad.check(&mut issues);
        let paths: Vec<&str> = issues.0.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "stream.destinations[0].id",
                "stream.destinations[1].id",
                "stream.destinations[1].server",
                "stream.destinations[2].server",
                "stream.destinations[2].width"
            ],
            "{:?}",
            issues.0
        );
    }

    #[test]
    fn presets_fill_the_server_and_file_keys_by_service() {
        let settings = |service, server: Option<&str>| StreamSettings {
            service,
            server: server.map(String::from),
            video_bitrate_kbps: 6000,
            rate_control: None,
            keyframe_interval_seconds: 2,
            audio_bitrate_kbps: 160,
            width: None,
            height: None,
            reconnect: Reconnect::default(),
            enabled: true,
            destinations: Vec::new(),
        };
        let youtube = settings(StreamService::Youtube, None);
        // Constant unless asked, as the services' guidelines ask.
        assert_eq!(youtube.rate_control(), RateControl::Constant);
        assert_eq!(
            youtube.server().unwrap().to_string(),
            "rtmps://a.rtmps.youtube.com/live2"
        );
        assert_eq!(youtube.key_account(), "youtube");
        let custom = settings(StreamService::Custom, Some("rtmp://Host:1935/live/"));
        assert_eq!(custom.key_account(), "rtmp://host/live");
        assert!(settings(StreamService::Custom, None).server().is_err());
        let kick = settings(
            StreamService::Kick,
            Some("rtmps://abc.global-contribute.live-video.net"),
        );
        assert_eq!(kick.server().unwrap().app, "app");
        // The canvas, unless larger than 1080p.
        assert_eq!(youtube.output_size((1280, 720)), (1280, 720));
        assert_eq!(youtube.output_size((3840, 2160)), (1920, 1080));
        assert_eq!(youtube.output_size((1206, 2622)), (884, 1920));
        assert_eq!(youtube.output_size((2560, 1600)), (1728, 1080));
        let mut issues = Issues::default();
        settings(StreamService::Facebook, Some("rtmp://example.com/rtmp")).check(&mut issues);
        assert_eq!(issues.0.len(), 1, "{:?}", issues.0);
        assert_eq!(issues.0[0].path, "stream.server");
        // A key pasted with a preset's server is caught, without repeating it.
        let mut issues = Issues::default();
        settings(
            StreamService::Twitch,
            Some("rtmp://live.twitch.tv/app/live_123_abc"),
        )
        .check(&mut issues);
        assert_eq!(issues.0.len(), 1, "{:?}", issues.0);
        assert!(!issues.0[0].message.contains("live_123_abc"));
        assert!(issues.0[0].message.contains("rtmp://live.twitch.tv/app"));
        assert_eq!(
            settings(
                StreamService::Twitch,
                Some("rtmp://live.twitch.tv/app/live_123_abc")
            )
            .pasted_key()
            .as_deref(),
            Some("live_123_abc")
        );
        // A custom server's app may have slashes.
        let mut issues = Issues::default();
        settings(StreamService::Custom, Some("rtmp://host/live/sub")).check(&mut issues);
        assert!(issues.0.is_empty(), "{:?}", issues.0);
    }
}
