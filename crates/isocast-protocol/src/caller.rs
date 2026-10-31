//! Who is calling the method table, and what only the engine's own
//! machine may do (docs/remote-engines.md, decisions 4, 14 and 15).
//!
//! A paired client on another computer, and an obs-websocket client, get
//! full control of the show (scenes, sources, recording, streaming), not of
//! the machine: the engine refuses [`local_only_reason`]'s methods from any
//! caller that isn't [`Origin::Local`], with `NOT_PERMITTED` and the
//! engine machine's name. The app hides the same controls from a window on
//! another computer, from the same list.

use std::net::SocketAddr;

use crate::api::Method;

/// Who sent a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub origin: Origin,
}

/// Where a request came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// This machine's own socket or pipe (the app, the CLI, MCP, plugins),
    /// limited to the same user by the operating system.
    Local,
    /// A paired client on another computer, over the network connection:
    /// its identity (the SHA-256 of its public key), the name it paired
    /// with, and where it connected from.
    Remote {
        client_id: String,
        name: String,
        addr: SocketAddr,
    },
    /// A client of the obs-websocket compatibility server. It's a network
    /// caller even on 127.0.0.1: another user's program on this machine
    /// can reach the port too.
    ObsWebsocket { addr: SocketAddr },
}

impl Caller {
    pub fn local() -> Self {
        Self {
            origin: Origin::Local,
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self.origin, Origin::Local)
    }

    /// The name events and replies give for this caller (`by`), so the
    /// other apps can say who changed what; `None` for local callers.
    pub fn by(&self) -> Option<String> {
        match &self.origin {
            Origin::Local => None,
            Origin::Remote { name, .. } => Some(name.clone()),
            Origin::ObsWebsocket { addr } => Some(format!("obs-websocket client {addr}")),
        }
    }

    /// The audit log's `peer` for this caller, if it isn't local (local
    /// connections describe their own peer: process id, transport).
    pub fn peer(&self) -> Option<serde_json::Value> {
        match &self.origin {
            Origin::Local => None,
            Origin::Remote {
                client_id,
                name,
                addr,
            } => Some(serde_json::json!({
                "transport": "tls",
                "client_id": client_id,
                "name": name,
                "addr": addr.to_string(),
            })),
            Origin::ObsWebsocket { addr } => Some(serde_json::json!({
                "transport": "obs-websocket",
                "peer": addr.to_string(),
            })),
        }
    }

    /// Why this caller may not call `method` on the engine machine named
    /// `machine`, as the error message to send; `None` if it may.
    pub fn refusal(&self, method: &Method, machine: &str) -> Option<String> {
        if self.is_local() {
            return None;
        }
        let reason = local_only_reason(method)?;
        let who = match &self.origin {
            Origin::Local => unreachable!("local callers return above"),
            Origin::Remote { name, .. } => format!("another computer ({name})"),
            Origin::ObsWebsocket { .. } => "an obs-websocket client".to_string(),
        };
        Some(format!(
            "{} only works on the engine's own computer, not from {who}: {reason}. Change this \
             on {machine}",
            method_label(method)
        ))
    }
}

/// The reason for every `remote_*` method that changes the remote
/// settings (enable, disable, pair, revoke): only `remote_status` and
/// `remote_clients` are allowed from other computers.
pub const REMOTE_SETTINGS: &str = "who may control the engine's computer is decided on it, so \
     a client can't open pairing, let others in, or lock someone out";

/// Why `method` works only on the engine's own machine, if it does
/// (decision 4): it controls the machine rather than the show. Matched
/// exhaustively, so every new method must be placed here.
pub fn local_only_reason(method: &Method) -> Option<&'static str> {
    match method {
        Method::WhepOffer(_) | Method::WhepClose => None,
        Method::Shutdown | Method::ShutdownIfIdle => Some(
            "it quits the engine for everyone using it, ending any recording or stream, and \
             nothing on the network can start it again",
        ),
        Method::WebsocketEnable(_) | Method::WebsocketDisable => Some(
            "the obs-websocket server is another way in, with its own password, that pairing and \
             revoking don't cover",
        ),
        Method::PluginAdd(_) | Method::PluginRemove(_) | Method::PluginSetEnabled(_) => {
            Some("plugins run programs on the engine's computer")
        }
        Method::DebugSet(_) => Some(
            "developer mode writes verbose logs and traces of everything on the engine's computer",
        ),
        Method::BugReport(params) if params.submit => Some(
            "sending a report publishes it as a GitHub issue; make the report here and send it \
             from the engine's computer",
        ),
        Method::MachineCalibrate => Some(
            "calibration loads the engine's encoders for up to 30 seconds and is started by \
             someone at it",
        ),
        Method::MachineCancel => {
            Some("calibration is started and stopped by someone at the engine's computer")
        }
        Method::FolderRootsSet(_) => {
            Some("which folders other computers can use is decided on the engine's computer")
        }
        Method::SharedFrames(_) => Some(
            "shared GPU surfaces only reach programs on the engine's computer; ask for `frames` \
             (encoded video) instead",
        ),
        // Added on review of the method table (2026-10-22), beyond
        // decision 4's list: these change the operating system or show
        // its prompts on the engine's screen, or overwrite any file.
        Method::VirtualCameraRemove => Some(
            "it removes a system extension and asks for an administrator password on the engine's \
             screen",
        ),
        Method::RequestPermissions(_) | Method::OpenPermissionSettings { .. } => Some(
            "the operating system's permission prompts and settings appear on the engine's own \
             screen, for someone there to answer",
        ),
        Method::ScreenPick(_) => Some(
            "the desktop's sharing dialog appears on the engine's own screen, for someone there to \
             choose a screen or window",
        ),
        Method::TakeExportNle(params) if params.replace => Some(
            "`replace` overwrites whatever file is at the path; leave it out (an earlier Isocast \
             export is still replaced) or choose another path",
        ),
        Method::RemoteStatus | Method::RemoteClients => None,
        Method::RemoteEnable(_)
        | Method::RemoteDisable
        | Method::RemotePairBegin
        | Method::RemotePairFinish
        | Method::RemoteRevoke(_) => Some(REMOTE_SETTINGS),
        // Allowed as methods; what they do is checked as they do it
        // (decision 25): `speed_test`, `optimize` and `tune` only against
        // configured servers, paths inside the folder roots, setup changes
        // without developer settings or files outside the roots.
        Method::BugReport(_)
        | Method::SpeedTest(_)
        | Method::TakeExportNle(_)
        | Method::Status
        | Method::Devices
        | Method::Get(_)
        | Method::Plan(_)
        | Method::Apply(_)
        | Method::Watch(_)
        | Method::Start(_)
        | Method::Stop(_)
        | Method::RequestStop(_)
        | Method::Split
        | Method::Pause
        | Method::Resume
        | Method::Marker { .. }
        | Method::RefreshBrowser { .. }
        | Method::SwitchScene { .. }
        | Method::Preview { .. }
        | Method::Take { .. }
        | Method::Doctor(_)
        | Method::Tune(_)
        | Method::Snapshot(_)
        | Method::Frame(_)
        | Method::Frames(_)
        | Method::Engine { .. }
        | Method::ImportObs(_)
        | Method::InitSetup(_)
        | Method::Permissions
        | Method::StreamStart(_)
        | Method::StreamStop(_)
        | Method::StreamKeySet(_)
        | Method::StreamKeyClear(_)
        | Method::StreamDestinationAdd(_)
        | Method::StreamDestinationRemove { .. }
        | Method::StreamDestinationSet(_)
        | Method::CanvasAdd(_)
        | Method::CanvasSet(_)
        | Method::CanvasRemove { .. }
        | Method::Library
        | Method::LibraryGet(_)
        | Method::LibraryCreate(_)
        | Method::LibraryRename(_)
        | Method::LibraryDelete(_)
        | Method::LibrarySwitch(_)
        | Method::LibraryImport(_)
        | Method::LibraryImportObs(_)
        | Method::PluginList
        | Method::DataGet(_)
        | Method::DataSet(_)
        | Method::DataDelete(_)
        | Method::Logs(_)
        | Method::ReportFile(_)
        | Method::HotkeysGet
        | Method::HotkeysSet(_)
        | Method::HotkeysActions
        | Method::HotkeysInput(_)
        | Method::VirtualCameraStart
        | Method::VirtualCameraStop
        | Method::VirtualCameraStatus
        | Method::EncoderOptions
        | Method::ClearPreview
        | Method::DebugStatus
        | Method::ListProjects(_)
        | Method::RenameProject(_)
        | Method::EstimateCapacity(_)
        | Method::ObsDiscover(_)
        | Method::Optimize(_)
        | Method::ReplayStart(_)
        | Method::ReplayStop(_)
        | Method::ReplaySave(_)
        | Method::ReplayStatus(_)
        | Method::WebsocketStatus
        | Method::ClearCanvasPreview { .. }
        | Method::ProjectorOpen(_)
        | Method::ProjectorClose(_)
        | Method::ProjectorList
        | Method::ProjectorPlace(_)
        | Method::MachineProfile
        | Method::GpuAdapter
        | Method::Hello
        | Method::FolderRoots
        | Method::FilesRoots
        | Method::FilesList(_) => None,
    }
}

/// The method's name as requests spell it (`websocket_enable`), with the
/// part that makes it local-only (`bug_report with submit`).
fn method_label(method: &Method) -> String {
    let name = serde_json::to_value(method)
        .ok()
        .and_then(|value| value.get("method")?.as_str().map(str::to_string))
        .unwrap_or_else(|| "this method".into());
    match method {
        Method::BugReport(_) => format!("{name} with submit"),
        Method::TakeExportNle(_) => format!("{name} with replace"),
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn method(json: &str) -> Method {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn machine_control_is_local_only() {
        let remote = Caller {
            origin: Origin::Remote {
                client_id: "ab".into(),
                name: "vogel-mac".into(),
                addr: "10.0.10.26:50000".parse().unwrap(),
            },
        };
        let obs = Caller {
            origin: Origin::ObsWebsocket {
                addr: "127.0.0.1:50000".parse().unwrap(),
            },
        };
        for json in [
            r#"{"method":"shutdown"}"#,
            r#"{"method":"shutdown_if_idle"}"#,
            r#"{"method":"websocket_disable"}"#,
            r#"{"method":"websocket_enable"}"#,
            r#"{"method":"plugin_remove","params":{"id":"x"}}"#,
            r#"{"method":"bug_report","params":{"submit":true}}"#,
            r#"{"method":"shared_frames","params":{}}"#,
            r#"{"method":"machine_cancel"}"#,
            r#"{"method":"folder_roots_set","params":{}}"#,
            r#"{"method":"machine_calibrate"}"#,
            r#"{"method":"virtual_camera_remove"}"#,
            r#"{"method":"request_permissions"}"#,
        ] {
            let method = method(json);
            assert!(local_only_reason(&method).is_some(), "{json}");
            let message = remote.refusal(&method, "exon-desk").unwrap();
            assert!(message.ends_with("on exon-desk"), "{message}");
            assert!(obs.refusal(&method, "exon-desk").is_some(), "{json}");
            assert!(Caller::local().refusal(&method, "exon-desk").is_none());
        }
        for json in [
            r#"{"method":"status"}"#,
            r#"{"method":"stop"}"#,
            r#"{"method":"bug_report","params":{"preview":true}}"#,
            r#"{"method":"speed_test","params":{}}"#,
            r#"{"method":"websocket_status"}"#,
            r#"{"method":"hello"}"#,
            r#"{"method":"folder_roots"}"#,
        ] {
            assert!(
                remote.refusal(&method(json), "exon-desk").is_none(),
                "{json}"
            );
        }
        let message = remote
            .refusal(
                &method(r#"{"method":"bug_report","params":{"submit":true}}"#),
                "exon-desk",
            )
            .unwrap();
        assert!(
            message.starts_with("bug_report with submit only works"),
            "{message}"
        );
    }
}
