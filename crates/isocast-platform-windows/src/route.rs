//! Read-only observations. No adapter selection, encoder probing or retained
//! GPU resources. Weak registrations expire with the capture/encoder itself.
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use isocast_protocol::api::{
    WindowsAdapter, WindowsCaptureRoute, WindowsDisplayOwner, WindowsEncoderRoute, WindowsGpuRoute,
    WindowsSessionType,
};
use parking_lot::Mutex;
use windows::Win32::Devices::DeviceAndDriverInstallation::*;
use windows::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTSClientProtocolType, WTSFreeMemory, WTSQuerySessionInformationW,
};
use windows::core::{Owned, PCWSTR, PWSTR};

use crate::{EncoderBackend, GraphicsDevice, encoder::NvencOwner};

pub(crate) fn adapter(device: &GraphicsDevice) -> WindowsAdapter {
    WindowsAdapter {
        name: device.name().into(),
        luid: device.adapter_luid(),
        warp: device.is_warp(),
    }
}

struct Live<T>(Mutex<Vec<Weak<T>>>);
impl<T> Live<T> {
    const fn new() -> Self {
        Self(Mutex::new(Vec::new()))
    }
    fn register(&self, value: T) -> Arc<T> {
        let value = Arc::new(value);
        let mut entries = self.0.lock();
        entries.retain(|v| v.strong_count() != 0);
        entries.push(Arc::downgrade(&value));
        value
    }
    fn current(&self) -> Vec<Arc<T>> {
        self.0.lock().iter().filter_map(Weak::upgrade).collect()
    }
}
pub(crate) struct CaptureObservation {
    source: String,
    owner: Option<WindowsDisplayOwner>,
    device: WindowsAdapter,
    first_picture: Box<dyn Fn() -> bool + Send + Sync>,
}
impl CaptureObservation {
    fn read(&self) -> WindowsCaptureRoute {
        WindowsCaptureRoute {
            source: self.source.clone(),
            display_owner: self.owner.clone(),
            capture_adapter: self.device.clone(),
            first_frame_received: (self.first_picture)(),
        }
    }
}
static CAPTURES: Live<CaptureObservation> = Live::new();
static ENCODERS: Live<WindowsEncoderRoute> = Live::new();

pub(crate) fn capture(
    source: &str,
    owner: Option<WindowsDisplayOwner>,
    device: &GraphicsDevice,
    first_picture: impl Fn() -> bool + Send + Sync + 'static,
) -> Arc<CaptureObservation> {
    // The parent only returns the capture after validating the helper's Ready LUID.
    CAPTURES.register(CaptureObservation {
        source: source.into(),
        owner,
        device: adapter(device),
        first_picture: Box::new(first_picture),
    })
}
fn output_route(
    owner: &NvencOwner,
    adapter: WindowsAdapter,
    provider: &EncoderBackend,
    reasons: &[String],
) -> WindowsEncoderRoute {
    WindowsEncoderRoute {
        output: owner.output.clone(),
        take: owner.take,
        provider: provider.to_string(),
        encoder_adapter: (!matches!(provider, EncoderBackend::MediaFoundationSoftware(_)))
            .then_some(adapter),
        fallback_reasons: reasons.to_vec(),
    }
}
pub(crate) fn encoder(
    owner: &NvencOwner,
    device: &GraphicsDevice,
    provider: &EncoderBackend,
    reasons: &[String],
) -> Arc<WindowsEncoderRoute> {
    ENCODERS.register(output_route(owner, adapter(device), provider, reasons))
}

fn classify(session: Option<u32>, protocol: Option<u16>) -> WindowsSessionType {
    match (session, protocol) {
        (Some(0), _) => WindowsSessionType::Service,
        (Some(_), Some(0)) => WindowsSessionType::Console,
        (Some(_), Some(2)) => WindowsSessionType::Rdp,
        _ => WindowsSessionType::Unknown,
    }
}
fn session() -> WindowsSessionType {
    // Status is polled more often than a desktop session changes. Refresh
    // even unknown results, so reconnects and failed WTS queries recover.
    static CACHE: Mutex<Option<(Instant, WindowsSessionType)>> = Mutex::new(None);
    let mut cache = CACHE.lock();
    if let Some((at, kind)) = *cache
        && at.elapsed() < Duration::from_secs(1)
    {
        return kind;
    }
    let kind = query_session();
    *cache = Some((Instant::now(), kind));
    kind
}

fn query_session() -> WindowsSessionType {
    let mut id = 0;
    if unsafe { ProcessIdToSessionId(std::process::id(), &mut id) }.is_err() {
        return WindowsSessionType::Unknown;
    }
    if id == 0 {
        return WindowsSessionType::Service;
    }
    let mut buffer = PWSTR::null();
    let mut len = 0;
    // WTSClientProtocolType returns a USHORT (not a UTF-16 string).
    let protocol = unsafe {
        if WTSQuerySessionInformationW(None, id, WTSClientProtocolType, &mut buffer, &mut len)
            .is_ok()
        {
            let value = (!buffer.is_null() && len as usize >= size_of::<u16>())
                .then(|| buffer.0.read_unaligned());
            WTSFreeMemory(buffer.0.cast());
            value
        } else {
            None
        }
    };
    classify(Some(id), protocol)
}

/// Unlike desktop enumeration, the PnP display class is available in session
/// zero. These names are inventory, never used to infer monitor ownership.
fn display_devices() -> Option<Vec<String>> {
    // SAFETY: Owned releases this device info set; the fixed property buffer
    // is aligned and live for both calls and decoding is bounded by its size.
    unsafe {
        let set = Owned::new(
            SetupDiGetClassDevsW(
                Some(&GUID_DEVCLASS_DISPLAY),
                PCWSTR::null(),
                None,
                DIGCF_PRESENT,
            )
            .ok()?,
        );
        let mut names = Vec::new();
        for i in 0..256 {
            let mut data = SP_DEVINFO_DATA {
                cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            if let Err(error) = SetupDiEnumDeviceInfo(*set, i, &mut data) {
                return (error.code()
                    == windows::Win32::Foundation::ERROR_NO_MORE_ITEMS.to_hresult())
                .then_some(names);
            }
            let mut chars = [0u16; 512];
            for property in [SPDRP_FRIENDLYNAME, SPDRP_DEVICEDESC] {
                let bytes =
                    std::slice::from_raw_parts_mut(chars.as_mut_ptr().cast(), size_of_val(&chars));
                if SetupDiGetDeviceRegistryPropertyW(*set, &data, property, None, Some(bytes), None)
                    .is_ok()
                {
                    let n = chars.iter().position(|&c| c == 0).unwrap_or(chars.len());
                    names.push(String::from_utf16_lossy(&chars[..n]));
                    break;
                }
            }
        }
        Some(names)
    }
}

pub(crate) fn snapshot(
    graphics_adapter: Option<WindowsAdapter>,
    inventory: bool,
) -> WindowsGpuRoute {
    let render_adapter = isocast_engine::gpu::Gpu::opened_adapter();
    let shader_compiler = render_adapter
        .as_ref()
        .map(|_| isocast_surfaces::shader_compiler().unwrap_or_else(|e| e.to_string()));
    WindowsGpuRoute {
        session_type: session(),
        selection: crate::adapter_choice::observed(),
        idle_replacement_block: isocast_surfaces::generation::registry()
            .idle_replacement_block(std::time::Instant::now()),
        graphics_adapter,
        render_adapter,
        shader_compiler,
        display_devices: inventory.then(display_devices).flatten(),
        captures: CAPTURES.current().iter().map(|c| c.read()).collect(),
        encoders: ENCODERS
            .current()
            .iter()
            .map(|e| e.as_ref().clone())
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PixelFormat;
    #[test]
    fn session_uses_the_process_not_the_active_console() {
        use WindowsSessionType::*;
        for (id, protocol, expected) in [
            (Some(0), Some(0), Service),
            (Some(1), Some(0), Console),
            (Some(9), Some(2), Rdp),
            (Some(1), None, Unknown),
            (Some(2), Some(1), Unknown),
            (None, Some(0), Unknown),
        ] {
            assert_eq!(classify(id, protocol), expected);
        }
    }
    #[test]
    fn actual_provider_and_live_picture_not_admission_or_ready() {
        let gpu = WindowsAdapter {
            name: "A".into(),
            luid: (2, -1),
            warp: false,
        };
        let owner = NvencOwner {
            recording: 17,
            take: 3,
            output: "iso/a".into(),
            size: (320, 180, 30),
            input: PixelFormat::Bgra8,
            planned: false,
        };
        let reasons = vec!["NVENC unavailable".to_string()];
        for (backend, expected) in [
            (EncoderBackend::MediaFoundationSoftware("CPU".into()), None),
            (
                EncoderBackend::MediaFoundationHardware("hw".into()),
                Some(gpu.clone()),
            ),
        ] {
            let e = output_route(&owner, gpu.clone(), &backend, &reasons);
            assert_eq!(e.encoder_adapter, expected);
            assert_eq!(e.fallback_reasons, reasons);
        }
        let live = Live::new();
        let first = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let c = live.register(CaptureObservation {
            source: "panel".into(),
            device: gpu,
            owner: Some(WindowsDisplayOwner::Unknown),
            first_picture: Box::new({
                let first = first.clone();
                move || first.load(std::sync::atomic::Ordering::Relaxed)
            }),
        });
        assert!(!c.read().first_frame_received);
        first.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(live.current()[0].read().first_frame_received);
        drop(c);
        assert!(live.current().is_empty());
    }
}
