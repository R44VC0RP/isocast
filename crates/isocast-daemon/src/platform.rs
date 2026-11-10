//! Select the engine platform once, rather than at every daemon call site.

#[cfg(target_os = "linux")]
pub use isocast_platform_linux::LinuxPlatform as NativePlatform;
#[cfg(target_os = "macos")]
pub use isocast_platform_macos::MacPlatform as NativePlatform;
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub use isocast_platform_stub::StubPlatform as NativePlatform;
#[cfg(windows)]
pub use isocast_platform_windows::WindowsPlatform as NativePlatform;

pub type Frame = <NativePlatform as isocast_engine::Platform>::Frame;

/// Windows services share one lazily opened graphics device. The other
/// backends use stateless platform values.
#[cfg(windows)]
pub fn native() -> &'static NativePlatform {
    static PLATFORM: std::sync::OnceLock<NativePlatform> = std::sync::OnceLock::new();
    PLATFORM.get_or_init(|| {
        // A preview can open the first renderer before there has been a take.
        isocast_surfaces::generation::registry().manage_idle_replacement();
        NativePlatform::default()
    })
}

#[cfg(not(windows))]
pub fn native() -> &'static NativePlatform {
    &NativePlatform
}

impl crate::Daemon {
    #[cfg(windows)]
    pub(super) fn configure_adapter(&self) -> anyhow::Result<()> {
        use std::io::Read;
        let path = isocast_protocol::api::daemon_file("gpu-adapter");
        let read = || -> anyhow::Result<String> {
            let file = match std::fs::File::open(&path) {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok("auto".into()),
                Err(e) => return Err(e.into()),
            };
            let mut text = String::new();
            file.take(4097).read_to_string(&mut text)?;
            let id = text.trim();
            anyhow::ensure!(
                text.len() <= 4096 && !id.is_empty() && !id.chars().any(char::is_control),
                "expected a single stable adapter ID or auto"
            );
            Ok(id.into())
        };
        let (id, warning) = match read() {
            Ok(id) => (id, None),
            Err(e) => (
                "auto".into(),
                Some(format!(
                    "could not read windows.gpu_adapter: {e}; using auto"
                )),
            ),
        };
        isocast_platform_windows::adapter_choice::configure(
            id,
            warning,
            self.state.read().setup.as_ref(),
        )
    }

    pub(super) fn gpu_preference(
        &self,
        adapter: Option<String>,
    ) -> Result<isocast_protocol::api::WindowsGpuSelection, isocast_protocol::api::RpcError> {
        #[cfg(windows)]
        {
            use isocast_platform_windows::adapter_choice;
            let Some(id) = adapter else {
                return adapter_choice::preference().map_err(crate::failed);
            };
            let _gate = self.starting.write();
            let state = self.state.read();
            if state.session.is_some()
                || state.tuning
                || state.starting
                || state.live_gpu_client().is_some()
            {
                return Err(isocast_protocol::api::RpcError{code:isocast_protocol::api::RpcError::WRONG_STATE,message:"params.adapter: stop recording, previews, streams, replay, virtual camera and tuning, and release any engine hold before saving the adapter preference".into()});
            }
            adapter_choice::save(&id, |valid| {
                crate::store::write_atomically(
                    &isocast_protocol::api::daemon_file("gpu-adapter"),
                    format!("{valid}\n").as_bytes(),
                )
            })
            .map_err(crate::failed)
        }
        #[cfg(not(windows))]
        {
            let _ = adapter;
            Err(isocast_protocol::api::RpcError {
                code: isocast_protocol::api::RpcError::UNSUPPORTED,
                message: "gpu_preference: only supported on Windows engines".into(),
            })
        }
    }
}
