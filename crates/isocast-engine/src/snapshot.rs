//! Snapshots: one frame of a video source or a scene, drawn by the GPU as a
//! PNG.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use isocast_protocol::{Filter, Scene, Setup, SourceKind};

use crate::Platform;
use crate::compiled::{NodeIds, compile};
use crate::content::{Contents, Sizer};
use crate::data::DataStore;
use crate::draw_list::DrawList;
use crate::filter::Filters;
use crate::gpu::{FrameImporter, Gpu};
use crate::layout::{Layouter, settings_size};
use crate::render::{Affine, Layer, Renderer, read_png};
use crate::source::{self, Timeline};

/// How long to wait for a device's first frame. iPhone screens can take a
/// few seconds.
const FIRST_FRAME_WAIT: Duration = Duration::from_secs(8);
/// How long to keep a device open after its first frame, so a camera's
/// exposure settles; the newest frame is used.
const SETTLE: Duration = Duration::from_millis(500);

/// The GPU, a renderer for PNGs, and the platform's frame importer, set up
/// once and reused.
pub struct Graphics<P: Platform> {
    gpu: Gpu,
    renderer: Renderer,
    importer: P::Importer,
    filters: Filters,
    /// For the time moving filters are drawn at.
    clock: P,
}

impl<P: Platform> Graphics<P> {
    pub fn new(platform: &P) -> Result<Self> {
        let gpu = Gpu::for_platform(platform)?;
        let renderer = Renderer::new(&gpu, wgpu::TextureFormat::Rgba8Unorm);
        let importer = platform.frame_importer(&gpu)?;
        let filters = Filters::new(&gpu, renderer.format(), false);
        Ok(Self {
            gpu,
            renderer,
            importer,
            filters,
            clock: platform.clone(),
        })
    }

    /// Whether its GPU device was lost: a cached `Graphics` cannot recover,
    /// so open a new one (which shares a fresh device).
    pub fn is_lost(&self) -> bool {
        self.gpu.is_lost()
    }

    /// Draw `frame` of source `id` as a PNG, through its `filters`, scaled
    /// down to at most `max_width` pixels wide. Returns the PNG and its
    /// size.
    pub fn png(
        &mut self,
        id: &str,
        frame: &P::Frame,
        filters: &[Filter],
        max_width: Option<u32>,
    ) -> Result<(Vec<u8>, u32, u32)> {
        let frame = self.importer.import(frame)?;
        let frame = self.filters.apply(
            &self.gpu,
            &self.renderer,
            id,
            frame,
            filters,
            self.clock.now(),
        );
        let (width, height) = fit(frame.width, frame.height, max_width);
        let layer = Layer::full(&frame, width, height);
        let png = self.renderer.png(&self.gpu, &[layer], width, height)?;
        Ok((png, width, height))
    }

    /// Draw `setup`'s `scene` on a canvas-sized picture from its sources'
    /// `frames` (by source id, through their filters), scaled down to at
    /// most `max_width` pixels wide, as the program draws it (laid out with
    /// the pictures' sizes, the same compiled draw list, content drawn with
    /// `data`'s values). Items without a frame draw only their style.
    /// Returns the PNG and its size.
    pub fn scene_png(
        &mut self,
        setup: &Setup,
        scene: &Scene,
        frames: &[(String, P::Frame)],
        max_width: Option<u32>,
        data: &DataStore,
    ) -> Result<(Vec<u8>, u32, u32)> {
        let canvas = (setup.canvas.width, setup.canvas.height);
        let (width, height) = fit(canvas.0, canvas.1, max_width);
        let scale = width as f32 / canvas.0 as f32;
        let mut pictures = Vec::with_capacity(frames.len());
        for (id, frame) in frames {
            let mut frame = self.importer.import(frame)?;
            if let Some(source) = setup.source(id) {
                frame = self.filters.apply(
                    &self.gpu,
                    &self.renderer,
                    id,
                    frame,
                    &source.filters,
                    self.clock.now(),
                );
            }
            pictures.push((id, frame));
        }
        // Laid out with the pictures' sizes, as the program does.
        let size_of = |id: &str| {
            pictures
                .iter()
                .find(|(source, _)| *source == id)
                .map(|(_, frame)| (frame.width, frame.height))
                .or_else(|| {
                    let source = setup.source(id)?;
                    let size = settings_size(&source.kind, canvas)?;
                    Some(crate::filter::output_size(&source.filters, size))
                })
        };
        let mut ids = NodeIds::default();
        let mut scenes = setup.scenes.clone();
        let mut contents = Contents::default();
        contents.begin();
        Layouter::default().resolve(
            &mut scenes,
            canvas,
            &size_of,
            &mut Sizer::new(&mut contents, data),
            &mut ids,
            &mut Vec::new(),
        );
        let compiled = Arc::new(compile(
            scenes.into(),
            canvas,
            &mut ids,
            Some((&scene.id, Affine::scale(scale), (width, height))),
            &setup.keyframes,
            &mut |item| contents.picture(item, data),
        ));
        let index = compiled
            .scene_index(Some(&scene.id))
            .with_context(|| format!("no scene {:?} in the setup", scene.id))?;
        // Animations as they are when the scene comes on air (each at the
        // start of its clock).
        let start = crate::Time::from_nanos(0);
        let mut list = DrawList::new(&self.gpu, &self.renderer, compiled.clone(), start);
        list.animate(index, start, start, start);
        for (id, frame) in &pictures {
            if let Some(source) = compiled.source_index(id) {
                let group = self.renderer.frame_group(&self.gpu, frame);
                list.set_picture(source, Some((frame, group)));
            }
        }
        let target = self.renderer.png_target(&self.gpu, width, height)?;
        let view = target.create_view(&Default::default());
        let mut encoder = self.gpu.device.create_command_encoder(&Default::default());
        list.upload(&mut encoder);
        list.encode(
            &self.gpu,
            &self.renderer,
            &mut encoder,
            index,
            &view,
            wgpu::Color::BLACK,
        );
        let png = read_png(&self.gpu, encoder, &target)?;
        Ok((png, width, height))
    }
}

/// `width`×`height` scaled down to at most `max_width` wide.
fn fit(width: u32, height: u32, max_width: Option<u32>) -> (u32, u32) {
    match max_width {
        Some(max) if max < width => {
            let scaled =
                (u64::from(height) * u64::from(max) + u64::from(width) / 2) / u64::from(width);
            (max.max(1), (scaled as u32).max(1))
        }
        _ => (width, height),
    }
}

/// Take a frame of each of the setup's video sources `ids` at once, as
/// [`take_frame`] does.
pub fn take_frames<P: Platform>(
    platform: &P,
    setup: &Setup,
    ids: &[String],
    data: &Arc<DataStore>,
) -> Result<Vec<(String, P::Frame)>> {
    std::thread::scope(|scope| {
        let threads: Vec<_> = ids
            .iter()
            .map(|id| {
                scope.spawn(move || take_frame(platform, setup, id, data).map(|f| (id.clone(), f)))
            })
            .collect();
        threads
            .into_iter()
            .map(|t| t.join().expect("snapshot thread panicked"))
            .collect()
    })
}

/// Open the setup's video source `id` briefly and take one frame of it: a
/// camera's newest after half a second (so its exposure settles), else the
/// first. Text showing a data key shows `data`'s value.
pub fn take_frame<P: Platform>(
    platform: &P,
    setup: &Setup,
    id: &str,
    data: &Arc<DataStore>,
) -> Result<P::Frame> {
    // Generators start their timeline now.
    let timeline = Arc::new(Timeline::default());
    timeline.set_start(platform.now());
    let opened = source::open(platform, setup, id, &timeline, data)?;
    let Some(video) = &opened.video else {
        let kind = setup.source(id).map_or("audio", |s| s.kind.label());
        bail!("{id:?} is a {kind} source, which has no picture");
    };
    let deadline = Instant::now() + FIRST_FRAME_WAIT;
    while !video.slot.has_frame() {
        if let Some(reason) = video.slot.stopped() {
            bail!("the capture stopped: {reason}");
        }
        if Instant::now() >= deadline {
            bail!("no frame arrived within {} s", FIRST_FRAME_WAIT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let camera = setup
        .source(id)
        .is_some_and(|s| matches!(s.kind, SourceKind::Camera(_)));
    if camera {
        std::thread::sleep(SETTLE);
    }
    video.slot.newest().context("no frame arrived")
}
