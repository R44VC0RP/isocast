//! Start with system FXC. Only a confirmed warm-up device loss may switch
//! this process and its pinned adapter to the verified app-local DXC pair.

use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, anyhow, ensure};
use sha2::{Digest, Sha256};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
use windows::core::{Owned, PCWSTR, w};

const VERSION: &str = "1.8.2502.8";
// x64 file identities in Microsoft's v1.8.2502 release, not the Windows SDK.
const COMPILER: &str = "f79943b02f73b621421b58a569fd6ee120d0023fb6bfa23844ecad6b00d6a295";
const VALIDATOR: &str = "62736e99aa534a942873f92b881a71b2cb6ad62b8d7481ea7a8ad8e9888a8056";

struct Selection {
    compiler: wgpu::Dx12Compiler,
    detail: String,
}

#[derive(Clone, Debug)]
pub enum ShaderCompilerFailure {
    SystemFxc(String),
    DxcFiles(String),
    DxcRetry(String),
}

impl ShaderCompilerFailure {
    pub fn fix(&self) -> &'static str {
        match self {
            Self::SystemFxc(_) => {
                "restart Isocast; if system FXC still won't load, repair the Windows DirectX runtime"
            }
            Self::DxcFiles(_) => "reinstall the Windows package with its matching DXC DLLs",
            Self::DxcRetry(_) => {
                "restart Isocast; renderer warm-up failed on this adapter with both shader compilers"
            }
        }
    }
}

impl std::fmt::Display for ShaderCompilerFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SystemFxc(detail) | Self::DxcFiles(detail) | Self::DxcRetry(detail) => {
                f.write_str(detail)
            }
        }
    }
}

fn open(path: &Path) -> io::Result<File> {
    // FILE_SHARE_READ: do not allow replacement or writes between verifying
    // the release identity and mapping the DLL.
    File::options().read(true).share_mode(1).open(path)
}

fn verify(file: &mut File, path: &Path, expected: &str) -> Result<()> {
    let mut digest = Sha256::new();
    io::copy(file, &mut digest)?;
    ensure!(
        format!("{:x}", digest.finalize()) == expected,
        "{} is not the pinned x64 DXC {VERSION}; reinstall Isocast",
        path.display()
    );
    Ok(())
}

fn load_dxc() -> Result<Selection> {
    let _span = isocast_diagnostics::gpu_startup_span("compiler.dxc.verify_and_load");
    let exe = std::env::current_exe()?;
    let dir = exe.parent().context("executable has no directory")?;
    let compiler = dir.join("dxcompiler.dll");
    let validator = dir.join("dxil.dll");
    let mut c = open(&compiler).with_context(|| format!("cannot open {}", compiler.display()))?;
    let mut v = open(&validator).with_context(|| format!("cannot open {}", validator.display()))?;
    verify(&mut c, &compiler, COMPILER)?;
    verify(&mut v, &validator, VALIDATOR)?;
    let dxc_path = compiler
        .to_str()
        .context("wgpu requires a Unicode DXC path")?
        .to_owned();
    // DXC may request the validator by basename. Load the verified pair by
    // absolute path, dependencies from System32 only, validator first. Keep
    // these two references until process exit (OnceLock, not per device).
    for path in [&validator, &compiler] {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe { LoadLibraryExW(PCWSTR(wide.as_ptr()), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
            .with_context(|| format!("cannot load {}", path.display()))?;
    }
    Ok(Selection {
        compiler: wgpu::Dx12Compiler::DynamicDxc { dxc_path },
        detail: format!("DXC (FXC lost the device on this adapter); verified app-local {VERSION}"),
    })
}

/// wgpu requests FXC by basename. Hold the System32 copy for the lifetime
/// of the consumer: one probe scope for lookup, process lifetime for rendering.
pub(super) fn load_system_fxc() -> windows::core::Result<Owned<HMODULE>> {
    let _span = isocast_diagnostics::gpu_startup_span("compiler.fxc.load");
    unsafe {
        Ok(Owned::new(LoadLibraryExW(
            w!("d3dcompiler_47.dll"),
            None,
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        )?))
    }
}

fn selection() -> &'static Mutex<Result<Selection, ShaderCompilerFailure>> {
    static CHOICE: OnceLock<Mutex<Result<Selection, ShaderCompilerFailure>>> = OnceLock::new();
    CHOICE.get_or_init(|| {
        Mutex::new(
            load_system_fxc()
                .map(|module| {
                    std::mem::forget(module); // process-lifetime selection, as for DXC
                    isocast_diagnostics::diagnostic!("info"; "DX12 shader compiler: FXC (default)");
                    Selection {
                        compiler: wgpu::Dx12Compiler::Fxc,
                        detail: "FXC (default)".into(),
                    }
                })
                .map_err(|e| {
                    ShaderCompilerFailure::SystemFxc(format!("cannot load system FXC: {e}"))
                }),
        )
    })
}

/// Called only by the renderer's serialized, pre-publication warm-up. Adapter
/// selection stays pinned to its original LUID across the context replacement.
pub fn retry_with_dxc(loss: &str) -> Result<()> {
    let mut state = selection().lock().unwrap();
    ensure!(
        matches!(&*state, Ok(s) if matches!(s.compiler, wgpu::Dx12Compiler::Fxc)),
        "FXC warm-up retry already consumed"
    );
    *state = load_dxc().map_err(|e| {
        ShaderCompilerFailure::DxcFiles(format!(
            "FXC warm-up lost its device ({loss}); DXC fallback unavailable: {e:#}"
        ))
    });
    let selected = state.as_ref().map_err(|e| {
        isocast_diagnostics::diagnostic!("error"; "{e}");
        anyhow!("{e}")
    })?;
    isocast_diagnostics::diagnostic!("warn"; "DX12 shader compiler: {}; FXC warm-up: {loss}; retrying once", selected.detail);
    Ok(())
}

pub fn fail_dxc_retry(error: &anyhow::Error) {
    let reason = format!("DXC renderer warm-up retry failed: {error:#}; not retrying");
    isocast_diagnostics::diagnostic!("error"; "{reason}");
    *selection().lock().unwrap() = Err(ShaderCompilerFailure::DxcRetry(reason));
}

pub fn shader_compiler() -> Result<String, ShaderCompilerFailure> {
    selection()
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.detail.clone())
        .map_err(Clone::clone)
}

pub fn dx12_instance() -> Result<wgpu::Instance> {
    instance_and_compiler().map(|(instance, _)| instance)
}

pub(super) fn instance_and_compiler() -> Result<(wgpu::Instance, wgpu::Dx12Compiler)> {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = wgpu::Backends::DX12;
    let compiler = selection()
        .lock()
        .unwrap()
        .as_ref()
        .map_err(|e| anyhow!("{e}"))?
        .compiler
        .clone();
    desc.backend_options.dx12.shader_compiler = compiler.clone();
    Ok((wgpu::Instance::new(desc), compiler))
}
