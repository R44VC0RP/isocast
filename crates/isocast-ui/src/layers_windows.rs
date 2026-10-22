//! DirectComposition underlays, the Windows counterpart of layers.rs.
//!
//! Slint draws into the top, premultiplied composition visual only when its
//! interface changes. Pictures, selection and the master meter live below it.
//! Shared pictures need one GPU copy into a transactional composition surface;
//! the daemon's ordinary shared allocations are not composition surfaces.

#[path = "layers_windows/content.rs"]
mod content;
#[path = "layers_windows/visuals.rs"]
mod visuals;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::time::Instant;

use anyhow::{Context, Result};
use isocast_client::shared::SharedFrame;
use slint::ComponentHandle;
use slint::wgpu_30::wgpu;
use slint::winit_030::{WinitWindowAccessor, winit::raw_window_handle::RawWindowHandle};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::DirectComposition::*;
use windows::core::{IUnknown, Interface};
use windows_numerics::Matrix3x2;

use crate::gpu::Gpu;
use crate::preview::Role;
use content::{Content, Drawing};
use visuals::Visuals;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Geometry {
    pub area: [f64; 4],
    pub picture: [f64; 4],
    pub program: [f64; 4],
    pub extra: [f64; 4],
    pub scale: f64,
    pub meter: [f64; 4],
    pub has_setup: bool,
    pub live: [bool; 3],
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeterLevel {
    pub rms: f32,
    pub peak: f32,
    pub hold: f32,
}

pub struct MeterColours {
    pub inset: [u8; 3],
    pub track: [u8; 3],
    pub zones: [[u8; 3]; 3],
    pub peaks: [[u8; 4]; 3],
    pub warn_db: f32,
    pub clip_db: f32,
    pub radius: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BoxShape {
    pub rect: [f32; 4],
    pub rotate: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BoxStyle {
    pub locked: bool,
    pub cropping: bool,
}

pub struct Colours {
    pub window: [u8; 3],
    pub stage: [u8; 3],
    pub monitor: [u8; 3],
    pub accent: [u8; 3],
    pub ring: [u8; 4],
    pub locked: [u8; 3],
    pub bone: [u8; 3],
    pub handle_edge: [u8; 4],
    pub frame: [u8; 3],
    pub live: [u8; 3],
    pub meter: MeterColours,
}

thread_local! {
    // Weak: closing a window must release its composition target and surfaces.
    static WINDOWS: RefCell<HashMap<isize, Weak<Composition>>> = RefCell::new(HashMap::new());
}

struct Composition {
    device: IDCompositionDevice,
    _target: IDCompositionTarget,
    root: IDCompositionVisual,
    interface: IDCompositionVisual,
    gpu: Gpu,
    configured: Cell<bool>,
}

impl Composition {
    fn new(hwnd: HWND, gpu: &Gpu) -> Result<Self> {
        // The UI swapchain uses the existing DX12 device. Preview drawing uses
        // an On12 wrapper on that same device/queue, not another GPU adapter.
        unsafe {
            let device: IDCompositionDevice = DCompositionCreateDevice2(None::<&IUnknown>)?;
            let target = device.CreateTargetForHwnd(hwnd, false)?;
            let root = device.CreateVisual()?;
            let interface = device.CreateVisual()?;
            root.AddVisual(&interface, false, None)?;
            target.SetRoot(&root)?;
            Ok(Self {
                device,
                _target: target,
                root,
                interface,
                gpu: gpu.clone(),
                configured: Cell::new(false),
            })
        }
    }

    fn commit(&self) -> Result<()> {
        unsafe { self.device.Commit() }.context("commit the preview composition")
    }
}

/// Called before Slint creates any HWND surface. The renderer retains the
/// composition through its callback; this map alone never keeps a window alive.
pub fn install(gpu: &Gpu) {
    use i_slint_renderer_skia::wgpu_30_surface::{
        CompositionSurface, set_composition_surface_factory,
    };
    let gpu = gpu.clone();
    let claimed = Cell::new(false);
    set_composition_surface_factory(Some(Rc::new(move |instance, window| {
        // AppWindow is shown first. Projectors retain the same image path as
        // on macOS; do not change every window's presentation implicitly.
        if claimed.replace(true) {
            return None;
        }
        let make = || -> Result<CompositionSurface> {
            // Deterministic qualification of the normal setup-failure path,
            // without disabling the system compositor or touching its driver.
            anyhow::ensure!(
                std::env::var_os("ISOCAST_DCOMP_FAIL_SETUP").is_none(),
                "DirectComposition setup failure requested by ISOCAST_DCOMP_FAIL_SETUP"
            );
            let RawWindowHandle::Win32(handle) = window.window_handle()?.as_raw() else {
                anyhow::bail!("composition requires a Windows window");
            };
            let key = handle.hwnd.get();
            let composition = Rc::new(Composition::new(HWND(key as *mut _), &gpu)?);
            let surface = unsafe {
                instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CompositionVisual(
                    composition.interface.as_raw(),
                ))?
            };
            WINDOWS.with(|windows| {
                let mut windows = windows.borrow_mut();
                windows.retain(|_, state| state.strong_count() > 0);
                windows.insert(key, Rc::downgrade(&composition));
            });
            Ok(CompositionSurface {
                surface,
                alpha_mode: wgpu::CompositeAlphaMode::PreMultiplied,
                configured: Rc::new(move || {
                    composition
                        .commit()
                        .map_err(|error| slint::PlatformError::from(format!("{error:#}")))?;
                    composition.configured.set(true);
                    Ok(())
                }),
            })
        };
        match make() {
            Ok(surface) => Some(surface),
            Err(error) => {
                isocast_diagnostics::diagnostic!("warn";
                    "DirectComposition unavailable; using Slint pictures: {error:#}");
                None
            }
        }
    })));
}

pub fn uninstall() {
    i_slint_renderer_skia::wgpu_30_surface::set_composition_surface_factory(None);
    WINDOWS.with(|windows| windows.borrow_mut().clear());
}

pub struct Layers {
    composition: Rc<Composition>,
    window: slint::Weak<crate::AppWindow>,
    hwnd: isize,
    visuals: Visuals,
    pictures: [Content; 3],
    _atlas: Content,
    sizes: [(u32, u32); 3],
    geometry: Geometry,
    shape: Option<BoxShape>,
    style: BoxStyle,
    dpi: f64,
    failed: bool,
    frames: [u64; 3],
    first_frame: [Option<Instant>; 3],
    last_frame: [Option<Instant>; 3],
}

impl Layers {
    pub fn attach(app: &crate::AppWindow, colours: Colours, test_outline: bool) -> Result<Self> {
        use slint::winit_030::winit::raw_window_handle::HasWindowHandle;
        let key = app
            .window()
            .with_winit_window(|window| match window.window_handle().ok()?.as_raw() {
                RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
                _ => None,
            })
            .flatten()
            .context("the window has no HWND")?;
        let composition = WINDOWS
            .with(|windows| windows.borrow().get(&key).and_then(Weak::upgrade))
            .filter(|state| state.configured.get())
            .context("the interface has no premultiplied composition surface")?;
        let atlas_visual = unsafe { composition.device.CreateVisual()? };
        let drawing = Drawing::new(&composition.gpu, &composition.device)?;
        let mut atlas = Content::new(&composition.gpu, &atlas_visual, drawing.clone())?;
        atlas.upload(
            visuals::PALETTE_WIDTH,
            1,
            &visuals::palette(&colours, test_outline),
        )?;
        let visuals = Visuals::new(
            &composition.device,
            &composition.root,
            atlas
                .content()
                .context("the colour atlas was not presented")?,
            &colours,
            test_outline,
        )?;
        let pictures = [
            Content::new(&composition.gpu, &visuals.picture_content, drawing.clone())?,
            Content::new(&composition.gpu, &visuals.program_content, drawing.clone())?,
            Content::new(&composition.gpu, &visuals.extra_content, drawing)?,
        ];
        let size = app.window().size();
        visuals.window_size(f64::from(size.width), f64::from(size.height))?;
        composition.commit()?;
        isocast_diagnostics::diagnostic!("info";
            "DirectComposition previews attached: transactional surfaces, one GPU copy per picture");
        Ok(Self {
            composition,
            window: app.as_weak(),
            hwnd: key,
            visuals,
            pictures,
            _atlas: atlas,
            sizes: [(0, 0); 3],
            geometry: Geometry::default(),
            shape: None,
            style: BoxStyle::default(),
            dpi: 0.0,
            failed: false,
            frames: [0; 3],
            first_frame: [None; 3],
            last_frame: [None; 3],
        })
    }

    pub fn available(&self) -> bool {
        !self.failed
    }
    pub fn device(&self) -> wgpu::Device {
        self.composition.gpu.device.clone()
    }
    pub fn needs_layout(&self, geometry: Geometry) -> bool {
        geometry != self.geometry || self.scale() != self.dpi
    }

    fn scale(&self) -> f64 {
        self.window
            .upgrade()
            .map_or(1.0, |app| f64::from(app.window().scale_factor()))
    }

    fn fail(&mut self, error: anyhow::Error) {
        if self.failed {
            return;
        }
        self.failed = true;
        isocast_diagnostics::diagnostic!("warn";
            "DirectComposition preview stopped; falling back to Slint pictures: {error:#}");
        if let Some(app) = self.window.upgrade() {
            app.global::<crate::CanvasArea>().set_native_picture(false);
            app.global::<crate::MixerArea>().set_master_native(false);
            app.window().request_redraw();
        }
        // Remove stale underlays even beneath translucent edges. Keep Content
        // alive until its in-flight GPU work can be safely retired.
        self.visuals.detach();
        let _ = self.composition.commit();
    }

    fn place(
        &mut self,
        geometry: Geometry,
        shape: Option<BoxShape>,
        style: BoxStyle,
    ) -> Result<()> {
        let dpi = self.scale();
        if let Some(app) = self.window.upgrade() {
            let size = app.window().size();
            self.visuals
                .window_size(f64::from(size.width), f64::from(size.height))?;
        }
        if geometry != self.geometry
            || shape != self.shape
            || style != self.style
            || dpi != self.dpi
        {
            self.visuals.place(geometry, shape, style, dpi)?;
            self.geometry = geometry;
            self.shape = shape;
            self.style = style;
            self.dpi = dpi;
            for i in 0..3 {
                self.fit(i)?;
            }
        }
        Ok(())
    }

    fn fit(&self, index: usize) -> Result<()> {
        let (width, height) = self.sizes[index];
        if width == 0 || height == 0 {
            return Ok(());
        }
        let rect = [
            self.geometry.picture,
            self.geometry.program,
            self.geometry.extra,
        ][index];
        let visual = [
            &self.visuals.picture_content,
            &self.visuals.program_content,
            &self.visuals.extra_content,
        ][index];
        unsafe {
            visual.SetTransform2(&Matrix3x2 {
                M11: (rect[2].max(0.0) * self.dpi / f64::from(width)) as f32,
                M22: (rect[3].max(0.0) * self.dpi / f64::from(height)) as f32,
                M12: 0.0,
                M21: 0.0,
                M31: 0.0,
                M32: 0.0,
            })?;
        }
        Ok(())
    }

    fn picture(&mut self, index: usize, frame: &SharedFrame) -> Result<bool> {
        use slint::winit_030::winit::raw_window_handle::HasWindowHandle;
        let hwnd = self.window.upgrade().and_then(|app| {
            app.window()
                .with_winit_window(|window| match window.window_handle().ok()?.as_raw() {
                    RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
                    _ => None,
                })
                .flatten()
        });
        anyhow::ensure!(
            hwnd == Some(self.hwnd),
            "the native window was recreated; using Slint pictures for its new surface"
        );
        let visible = self.window.upgrade().is_some_and(|app| {
            app.window()
                .with_winit_window(|window| {
                    let size = window.inner_size();
                    !window.is_minimized().unwrap_or(false) && size.width > 0 && size.height > 0
                })
                .unwrap_or(false)
        });
        if !visible {
            return Ok(false);
        }
        let texture = frame.texture(&self.composition.gpu.device)?;
        let size = (texture.width(), texture.height());
        if !self.pictures[index].show(frame)? {
            return Ok(false);
        }
        let now = Instant::now();
        self.frames[index] += 1;
        self.first_frame[index].get_or_insert(now);
        self.last_frame[index] = Some(now);
        if self.sizes[index] != size {
            self.sizes[index] = size;
            self.fit(index)?;
        }
        Ok(true)
    }

    pub fn show(
        &mut self,
        frame: Option<&SharedFrame>,
        shape: Option<BoxShape>,
        style: BoxStyle,
        geometry: Geometry,
    ) -> bool {
        if self.failed {
            return false;
        }
        let result = (|| {
            let shown = match frame {
                Some(frame) => self.picture(0, frame)?,
                None => true,
            };
            if !shown {
                return Ok(false);
            }
            self.place(geometry, shape, style)?;
            self.composition.commit()?;
            Ok(shown)
        })();
        match result {
            Ok(shown) => shown,
            Err(error) => {
                self.fail(error);
                false
            }
        }
    }

    pub fn show_program(&mut self, frame: &SharedFrame) -> bool {
        self.show_other(1, frame)
    }
    pub fn show_extra(&mut self, frame: &SharedFrame) -> bool {
        self.show_other(2, frame)
    }
    fn show_other(&mut self, index: usize, frame: &SharedFrame) -> bool {
        if self.failed {
            return false;
        }
        let result = self.picture(index, frame).and_then(|shown| {
            self.composition.commit()?;
            Ok(shown)
        });
        match result {
            Ok(shown) => shown,
            Err(error) => {
                self.fail(error);
                false
            }
        }
    }

    pub fn clear(&mut self, role: Role) {
        let index = match role {
            Role::Editor => 0,
            Role::Program => 1,
            Role::Extra => 2,
        };
        if let Err(error) = self.pictures[index]
            .clear()
            .and_then(|_| self.composition.commit())
        {
            self.fail(error);
        }
    }

    pub fn show_meter(&mut self, rect: [f64; 4], levels: [MeterLevel; 2]) {
        if self.failed {
            return;
        }
        let result = self
            .visuals
            .show_meter(rect, levels, self.scale())
            .and_then(|_| self.composition.commit());
        if let Err(error) = result {
            self.fail(error);
        }
    }

    pub fn show_test_pointer(&self, rect: [f64; 4]) -> bool {
        !self.failed
            && self
                .visuals
                .show_test_pointer(rect, self.scale())
                .and_then(|shown| {
                    self.composition.commit()?;
                    Ok(shown)
                })
                .unwrap_or(false)
    }
}

impl Drop for Layers {
    fn drop(&mut self) {
        let span = |i: usize| {
            self.first_frame[i]
                .zip(self.last_frame[i])
                .map_or(0.0, |(first, last)| {
                    last.duration_since(first).as_secs_f64()
                })
        };
        isocast_diagnostics::diagnostic!("info";
            "DirectComposition submitted pictures: editor={} in {:.3}s, program={} in {:.3}s, extra={} in {:.3}s; fallback={}",
            self.frames[0], span(0), self.frames[1], span(1), self.frames[2], span(2), self.failed);
        self.visuals.detach();
        let _ = self.composition.commit();
    }
}
