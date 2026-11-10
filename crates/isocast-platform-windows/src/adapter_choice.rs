//! Daemon-only startup policy. #260 retains the sole LUID pin and owns its
//! lock. Never retain candidate adapters, devices, textures or fence handles.
use anyhow::{Context, Result, bail, ensure};
use isocast_engine::{VideoTrackSpec, encoder_calibration};
use isocast_protocol::{
    Setup, SourceKind,
    api::{WindowsGpuCandidate as Gpu, WindowsGpuSelection as Plan},
};
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::sync::OnceLock;
use windows::Win32::{
    Devices::Display::*,
    Foundation::LUID,
    Graphics::{Direct3D::D3D_FEATURE_LEVEL_11_0, Direct3D12::*},
};

use crate::{
    Frame, GraphicsDevice, NvencBudgetOutput, NvencBudgetProfile, PixelFormat, RecordingQuality,
    platform,
};

static BOOT: OnceLock<(String, Option<String>)> = OnceLock::new();
static SETUP: Mutex<Option<Setup>> = Mutex::new(None);
static PLAN: Mutex<Option<Plan>> = Mutex::new(None);

pub fn configure(preference: String, warning: Option<String>, setup: Option<&Setup>) -> Result<()> {
    BOOT.set((preference, warning))
        .map_err(|_| anyhow::anyhow!("GPU preference already configured"))?;
    workload(setup);
    isocast_surfaces::install_adapter_planner(select)
}

/// No native enumeration while applying a setup. Once pinned, later scene
/// and profile edits do not move the machine's devices.
pub fn workload(setup: Option<&Setup>) {
    if PLAN.lock().is_none() {
        *SETUP.lock() = setup.cloned();
    }
}

pub fn observed() -> Option<Plan> {
    PLAN.lock().clone()
}

pub fn preference() -> Result<Plan> {
    isocast_surfaces::capture_adapter_luid()?;
    observed().context("the Windows engine's GPU plan is unavailable")
}

pub fn save(id: &str, persist: impl FnOnce(&str) -> Result<()>) -> Result<Plan> {
    preference()?;
    let mut plan = PLAN.lock();
    let plan = plan.as_mut().context("GPU preference not initialized")?;
    if id != "auto" && plan.candidates.iter().filter(|c| c.id == id).count() != 1 {
        bail!(
            "params.adapter: missing or ambiguous adapter {id:?}; valid: {}",
            choices(&plan.candidates)
        );
    }
    persist(id)?;
    plan.preference = id.into();
    plan.restart_required = BOOT.get().is_some_and(|(boot, _)| boot != id);
    Ok(plan.clone())
}

fn choices(devices: &[Gpu]) -> String {
    std::iter::once("auto".into())
        .chain(
            devices
                .iter()
                .filter(|d| devices.iter().filter(|c| c.id == d.id).count() == 1)
                .map(|d| format!("{:?} ({})", d.id, d.name)),
        )
        .collect::<Vec<_>>()
        .join(", ")
}

struct Candidate {
    gpu: Gpu,
    capacity: Capacity,
    bytes: u64,
    performance: u8,
    index: usize,
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Capacity {
    Refused,
    Unknown,
    Fits,
}

fn choose(devices: &[Candidate], requested: &str) -> Result<(usize, String, Option<String>)> {
    let matched: Vec<_> = devices
        .iter()
        .enumerate()
        .filter(|(_, c)| c.gpu.id == requested)
        .collect();
    if let [(n, _)] = matched.as_slice() {
        return Ok((*n, "explicit stable adapter preference".into(), None));
    }
    let (n, best) = devices
        .iter()
        .enumerate()
        .max_by_key(|(_, c)| {
            (
                !c.gpu.software,
                c.capacity == Capacity::Fits,
                c.performance,
                c.bytes,
                std::cmp::Reverse(c.index),
            )
        })
        .context("no compatible Windows D3D11/DX12 adapter")?;
    let reason = if best.gpu.software {
        "compatible software adapter (WARP)".into()
    } else if best.capacity == Capacity::Fits {
        "driver-matched calibration for the configured recording workload".into()
    } else if best.capacity == Capacity::Refused {
        "high-performance compatible hardware; the cached plan does not fit the configured workload"
            .into()
    } else if best.bytes != 0 {
        format!(
            "high-performance device class; captured displays owned by this adapter: {} BGRA bytes/s",
            best.bytes
        )
    } else {
        "high-performance compatible hardware; no stronger workload evidence".into()
    };
    let warning = (requested != "auto").then(|| {
        format!(
            "windows.gpu_adapter: saved ID {requested:?} is {}. Using auto; valid: {}",
            if matched.is_empty() {
                "missing or incompatible"
            } else {
                "ambiguous"
            },
            choices(&devices.iter().map(|c| c.gpu.clone()).collect::<Vec<_>>())
        )
    });
    Ok((n, reason, warning))
}

fn stable_id(desc: &windows::Win32::Graphics::Dxgi::DXGI_ADAPTER_DESC2, name: &str) -> String {
    let mut query = DISPLAYCONFIG_ADAPTER_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME,
            size: size_of::<DISPLAYCONFIG_ADAPTER_NAME>() as u32,
            adapterId: desc.AdapterLuid,
            id: 0,
        },
        ..Default::default()
    };
    if unsafe { DisplayConfigGetDeviceInfo(&mut query.header) } == 0
        && query.adapterDevicePath[0] != 0
    {
        let n = query
            .adapterDevicePath
            .iter()
            .position(|u| *u == 0)
            .unwrap_or(query.adapterDevicePath.len());
        return format!(
            "pnp:{}",
            String::from_utf16_lossy(&query.adapterDevicePath[..n]).to_lowercase()
        );
    }
    // Session 0/WARP may expose no CCD interface path. Only accept this
    // fingerprint as an override if it resolves to precisely one device.
    format!(
        "dxgi:{:04x}:{:04x}:{:08x}:{:02x}:{}",
        desc.VendorId,
        desc.DeviceId,
        desc.SubSysId,
        desc.Revision,
        name.trim().to_lowercase()
    )
}

fn calibrated(
    d: &GraphicsDevice,
    setup: &Setup,
    cache: &encoder_calibration::PresetCalibration,
) -> Option<Capacity> {
    // Never guess source formats, concurrent live workload or unknown ISO
    // sizes. The normal recording preflight is authoritative in all cases.
    if !setup.extra_canvas_ids().is_empty()
        || setup.stream.is_some()
        || !setup.recording.replay.is_default()
    {
        return None;
    }
    let mut outputs = Vec::new();
    let mut add =
        |id: &str, iso, size: (u32, u32), fps, encoder: &isocast_protocol::Encoder| -> Option<()> {
            let qp = RecordingQuality::new(encoder.quality).ok()?.nvenc_qp;
            if platform::calibration_mismatch(
                &cache.policy,
                Some(qp),
                PixelFormat::Bgra8,
                fps,
                encoder.effective_bit_depth(),
            )
            .is_some()
            {
                return None;
            }
            outputs.push(NvencBudgetOutput {
                id: id.into(),
                iso,
                video: VideoTrackSpec {
                    width: size.0,
                    height: size.1,
                    fps,
                    encoder: encoder.clone(),
                },
            });
            Some(())
        };
    let p = &setup.recording.program;
    if p.enabled {
        add(
            "program",
            false,
            p.size(&setup.canvas),
            setup.canvas.fps,
            &p.encoder,
        )?;
    }
    let iso = &setup.recording.iso;
    if iso.enabled {
        for id in &iso.sources {
            let s = setup.source(id)?;
            let SourceKind::TestPattern(p) = &s.kind else {
                return None;
            };
            if iso.filtered && !s.filters.is_empty() {
                return None;
            }
            add(id, true, (p.width, p.height), p.fps, &iso.encoder_for(s))?;
        }
    }
    if outputs.is_empty() {
        return None;
    }
    let profile = NvencBudgetProfile::reference(d)
        .ok()?
        .calibrated(&cache.key, cache.h264, cache.hevc)
        .ok()?;
    Some(if profile.plan(d, &outputs, false).is_ok() {
        Capacity::Fits
    } else {
        Capacity::Refused
    })
}

fn candidate(
    adapter: &wgpu::Adapter,
    index: usize,
    probe: bool,
    affinity: &BTreeMap<(u32, i32), u64>,
    workload: Option<(&Setup, &encoder_calibration::PresetCalibration)>,
) -> Result<Candidate> {
    let info = adapter.get_info();
    let hal = unsafe { adapter.as_hal::<wgpu::hal::api::Dx12>() }.context("not DX12")?;
    let raw = hal.raw_adapter();
    let desc = unsafe { raw.GetDesc2() }?;
    let LUID {
        LowPart: low,
        HighPart: high,
    } = desc.AdapterLuid;
    let luid = (low, high);
    let software = info.device_type == wgpu::DeviceType::Cpu
        || (desc.VendorId, desc.DeviceId) == (0x1414, 0x008c);
    let mut capacity = Capacity::Unknown;
    if probe {
        let d = GraphicsDevice::open(
            luid,
            isocast_surfaces::generation::registry().register("adapter compatibility probe")?,
        )?;
        let f = Frame::new_output(d.clone(), 16, 16)?;
        let mut dx12: Option<ID3D12Device> = None;
        unsafe { D3D12CreateDevice(&**raw, D3D_FEATURE_LEVEL_11_0, &mut dx12) }?;
        let dx12 = dx12.context("no DX12 device")?;
        let mut texture: Option<ID3D12Resource> = None;
        let mut fence: Option<ID3D12Fence> = None;
        unsafe {
            dx12.OpenSharedHandle(f.shared_texture(), &mut texture)?;
            dx12.OpenSharedHandle(f.shared_fence(), &mut fence)?;
        }
        ensure!(
            texture.is_some() && fence.is_some(),
            "incomplete native sharing probe"
        );
        capacity = workload
            .and_then(|(s, c)| calibrated(&d, s, c))
            .unwrap_or(Capacity::Unknown);
    }
    use wgpu::DeviceType::*;
    let performance = match info.device_type {
        DiscreteGpu => 4,
        IntegratedGpu => 3,
        Other => 2,
        VirtualGpu => 1,
        Cpu => 0,
    };
    Ok(Candidate {
        gpu: Gpu {
            id: stable_id(&desc, &info.name),
            name: info.name,
            luid,
            software,
            driver: info.driver,
        },
        index,
        capacity,
        bytes: affinity.get(&luid).copied().unwrap_or(0),
        performance,
    })
}

fn finish_plan<T>(
    devices: Vec<Candidate>,
    requested: &str,
    ranked: impl FnOnce(usize) -> T,
    default: impl FnOnce() -> Result<(T, Gpu)>,
) -> Result<(T, Plan)> {
    let (adapter, chosen, reason, warning) = if devices.is_empty() {
        // The extra checks are advisory: don't make a previously working
        // machine unstartable. Record the adapter request_adapter returned,
        // not a guessed index; do not advertise a failed probe as a valid ID.
        let (adapter, chosen) = default()?;
        (
            adapter,
            chosen,
            "wgpu high-performance default".into(),
            Some(format!(
                "No adapter passed the compatibility checks for preference {requested:?}; using wgpu's high-performance default. Valid override: auto"
            )),
        )
    } else {
        let (n, reason, warning) = choose(&devices, requested)?;
        (
            ranked(devices[n].index),
            devices[n].gpu.clone(),
            reason,
            warning,
        )
    };
    Ok((
        adapter,
        Plan {
            preference: requested.into(),
            restart_required: false,
            chosen,
            reason,
            warning,
            candidates: devices.into_iter().map(|c| c.gpu).collect(),
        },
    ))
}

fn select(
    mut adapters: Vec<wgpu::Adapter>,
    default: &dyn Fn() -> Result<wgpu::Adapter>,
) -> Result<wgpu::Adapter> {
    let (requested, load_warning) = BOOT.get().context("adapter planner not configured")?;
    let setup = SETUP.lock().clone();
    let multi = adapters.len() > 1;
    let affinity = setup
        .as_ref()
        .filter(|_| multi)
        .map(crate::devices::capture_affinity)
        .unwrap_or_default();
    let cache = multi
        .then(|| {
            encoder_calibration::load(&platform::budget_cache_path())
                .ok()
                .flatten()
        })
        .flatten()
        .filter(|c| platform::check_calibration_policy(c).is_ok());
    let mut devices = Vec::new();
    for (i, a) in adapters.iter().enumerate() {
        match candidate(a, i, multi, &affinity, setup.as_ref().zip(cache.as_ref())) {
            Ok(c) => devices.push(c),
            Err(e) => {
                isocast_diagnostics::diagnostic!("warn"; "Windows adapter {:?} not selectable: {e:#}", a.get_info().name)
            }
        }
    }
    // Don't retain failed candidates while wgpu requests its normal default.
    if devices.is_empty() {
        adapters.clear();
    }
    let (adapter, mut plan) = finish_plan(
        devices,
        requested,
        |i| adapters.swap_remove(i),
        || {
            let a = default()?;
            let info = candidate(&a, 0, false, &affinity, None)?.gpu;
            Ok((a, info))
        },
    )?;
    if let Some(load) = load_warning {
        plan.warning = Some(match plan.warning {
            Some(w) => format!("{load}; {w}"),
            None => load.clone(),
        });
    }
    isocast_diagnostics::diagnostic!("info"; "engine adapter {} {:?}: {}", plan.chosen.name,plan.chosen.luid,plan.reason);
    if let Some(msg) = &plan.warning {
        isocast_diagnostics::diagnostic!("warn"; "{msg}");
    }
    *PLAN.lock() = Some(plan);
    SETUP.lock().take();
    Ok(adapter)
}

pub(super) fn doctor_warning(plan: &Plan) -> Option<isocast_protocol::api::Check> {
    use isocast_protocol::api::{Check, CheckStatus};
    let mut details = Vec::new();
    if let Some(warning) = &plan.warning {
        details.push(warning.clone());
    }
    if plan.restart_required {
        details.push(format!(
            "Saved preference {:?} is pending a daemon restart",
            plan.preference
        ));
    }
    if details.is_empty() {
        return None;
    }
    let fix = if plan.restart_required {
        "Restart the idle daemon to apply the saved preference."
    } else {
        "Inspect `isocast gpu-adapter --choices` and save a valid ID or auto with --set while idle."
    };
    Some(Check {
        name: "Windows GPU selection".into(),
        status: CheckStatus::Warning,
        detail: format!(
            "{} ({}). {}",
            plan.chosen.name,
            plan.reason,
            details.join(". ")
        ),
        fix: Some(fix.into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn gpu(id: &str, low: u32, performance: u8, bytes: u64) -> Candidate {
        Candidate {
            gpu: Gpu {
                id: id.into(),
                name: id.into(),
                luid: (low, 0),
                software: performance == 0,
                driver: String::new(),
            },
            index: low as usize,
            performance,
            bytes,
            capacity: Capacity::Unknown,
        }
    }
    #[test]
    fn uncalibrated_hybrid_keeps_discrete_gpu_even_for_an_integrated_panel() {
        let mut candidates = [
            gpu("warp", 1, 0, u64::MAX),
            gpu("panel", 2, 3, 4000),
            gpu("discrete", 3, 4, 0),
        ];
        assert_eq!(choose(&candidates, "auto").unwrap().0, 2);
        candidates[2].capacity = Capacity::Refused;
        assert_eq!(choose(&candidates, "auto").unwrap().0, 2);
        candidates[1].capacity = Capacity::Fits;
        assert_eq!(choose(&candidates, "auto").unwrap().0, 1);
        candidates[1].capacity = Capacity::Unknown;
        candidates[1].performance = candidates[2].performance;
        assert_eq!(choose(&candidates, "auto").unwrap().0, 1);
    }
    #[test]
    fn failed_probes_use_the_actual_default_and_keep_the_preference() {
        let (actual, plan) = finish_plan(
            vec![],
            "missing",
            |_| panic!("no validated index"),
            || Ok((99, gpu("default", 99, 4, 0).gpu)),
        )
        .unwrap();
        assert_eq!(actual, 99);
        assert_eq!(plan.chosen.luid, (99, 0));
        assert!(plan.candidates.is_empty());
        assert_eq!(plan.preference, "missing");
        assert!(
            doctor_warning(&plan)
                .unwrap()
                .detail
                .contains("high-performance default")
        );
    }
    #[test]
    fn stable_override_survives_new_luids_and_reorder_but_not_an_ambiguous_match() {
        let old = [gpu("pnp:intel", 1, 3, 1), gpu("pnp:egpu", 2, 4, 0)];
        let id = &old[1].gpu.id;
        let new = [gpu(id, 99, 4, 0), gpu("pnp:intel", 48, 3, 1)];
        assert_eq!(new[choose(&new, id).unwrap().0].gpu.luid, (99, 0));
        let (index, _, warn) = choose(&new, "gone").unwrap();
        assert_eq!(index, 0);
        assert!(warn.unwrap().contains("missing"));
        let twins = [gpu("fallback", 3, 4, 0), gpu("fallback", 5, 4, 0)];
        assert!(
            choose(&twins, "fallback")
                .unwrap()
                .2
                .unwrap()
                .contains("ambiguous")
        );
    }
}
