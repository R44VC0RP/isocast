//! Watching a target's picture without copying it (`shared_frames`): the
//! daemon's GPU draws each frame into a surface shared with this process,
//! and [`SharedFrames::next`] hands it over, ready for this process's GPU.
//!
//! ```no_run
//! # fn show(_: &wgpu::Texture, _: Option<u64>) {}
//! # fn main() -> anyhow::Result<()> {
//! # let device: wgpu::Device = todo!();
//! # let queue: wgpu::Queue = todo!();
//! use isocast_client::Client;
//! use isocast_protocol::api::SharedFramesParams;
//!
//! isocast_client::shared::register_gpu(&device, &queue)?;
//! let mut frames = Client::connect_or_launch()?.shared_frames(SharedFramesParams {
//!     target: "program".into(),
//!     max_width: Some(1600),
//!     max_height: Some(900),
//!     fps: Some(60),
//!     slots: None,
//!     filtered: false,
//! })?;
//! // Keep the frame on screen until the next one has been drawn, then drop
//! // it: dropping hands its slot back to the daemon.
//! let mut showing = None;
//! while let Some(frame) = frames.next(None)? {
//!     let texture = frame.texture(&device)?; // cached per slot
//!     show(&texture, frame.revision);
//!     showing = Some(frame);
//! }
//! # Ok(()) }
//! ```

use isocast_ipc::Stream as UnixStream;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use isocast_protocol::api::shared::{Control, Event};
use isocast_protocol::api::{Method, SharedFrameStream, SharedFramesParams};
pub use isocast_surfaces::host_now;
use isocast_surfaces::{Surface, Token};

use crate::Client;

/// How long fetching a ring's surfaces from the daemon may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(all(windows, feature = "wgpu"))]
type RenderingGpu = Mutex<Option<(wgpu::Device, wgpu::Queue)>>;
#[cfg(all(windows, feature = "wgpu"))]
static RENDERING_GPU: std::sync::OnceLock<RenderingGpu> = std::sync::OnceLock::new();

/// Select and pin the daemon's actual adapter before constructing a Windows
/// renderer (including engine::Gpu in the CLI). Only an older/unsupported
/// engine may use local selection so its UI can still open without previews.
#[cfg(all(windows, feature = "wgpu"))]
pub fn adapter(client: &mut Client, instance: &wgpu::Instance) -> Result<wgpu::Adapter> {
    use isocast_protocol::api::RpcError;

    let luid = match client.call::<Option<(u32, i32)>>(Method::GpuAdapter) {
        Ok(luid) => {
            Some(luid.context("the daemon did not provide a Windows graphics adapter LUID")?)
        }
        Err(error)
            if error
                .downcast_ref::<crate::DaemonError>()
                .is_some_and(|error| {
                    matches!(
                        error.code(),
                        RpcError::METHOD_NOT_FOUND | RpcError::UNSUPPORTED
                    )
                }) =>
        {
            None
        }
        Err(error) => return Err(error),
    };
    isocast_surfaces::dx12_adapter(instance, luid)
}

/// Register the actual rendering queue before receiving Windows previews.
/// Slint/Skia and the app must submit through this same queue. A frame must
/// remain alive until no future render will reference its image; its drop
/// then fences already-submitted reads before handing the slot back.
#[cfg(feature = "wgpu")]
pub fn register_gpu(device: &wgpu::Device, queue: &wgpu::Queue) -> Result<()> {
    #[cfg(windows)]
    {
        isocast_surfaces::check_render_queue(device, queue)?;
        let mut gpu = RENDERING_GPU
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap();
        if let Some((existing, existing_queue)) = gpu.as_ref() {
            if existing != device || existing_queue != queue {
                bail!("shared previews require one rendering device/queue per client process");
            }
        } else {
            *gpu = Some((device.clone(), queue.clone()));
        }
    }
    #[cfg(not(windows))]
    let _ = (device, queue);
    Ok(())
}

impl Client {
    /// Watch a target's picture through surfaces shared with the daemon
    /// (`shared_frames`). Dropping the result stops watching.
    pub fn shared_frames(mut self, params: SharedFramesParams) -> Result<SharedFrames> {
        #[cfg(all(windows, feature = "wgpu"))]
        if isocast_surfaces::retirement_failed() {
            bail!("GPU previews failed; restart the client before opening another viewer");
        }
        let info: SharedFrameStream = self.call(Method::SharedFrames(params))?;
        if info.transport != isocast_surfaces::TRANSPORT {
            bail!(
                "isocastd hands surfaces over by {:?}, which this client can't take",
                info.transport
            );
        }
        #[cfg(windows)]
        self.writer
            .set_write_timeout(Some(Duration::from_millis(250)))?;
        Ok(SharedFrames {
            info,
            writer: Arc::new(Mutex::new(self.writer)),
            reader: self.reader,
            line: Vec::new(),
            ring: None,
        })
    }
}

/// A viewer's stream of shared frames.
pub struct SharedFrames {
    info: SharedFrameStream,
    reader: BufReader<UnixStream>,
    /// Shared with frames, which release their slots through it.
    writer: Arc<Mutex<UnixStream>>,
    /// A line read in part before a timeout.
    line: Vec<u8>,
    ring: Option<Arc<Ring>>,
}

/// One ring of surfaces, and the textures made of them.
struct Ring {
    /// Process-local identity; wire generations restart on every stream.
    identity: u64,
    generation: u64,
    surfaces: Vec<Surface>,
    #[cfg(feature = "wgpu")]
    textures: Mutex<Vec<Option<(wgpu::Device, wgpu::Texture)>>>,
}

impl SharedFrames {
    /// The daemon's reply: target, rate, ring size, transport.
    pub fn info(&self) -> &SharedFrameStream {
        &self.info
    }

    /// The next frame, waiting up to `timeout` (for ever if `None`, not at
    /// all if zero): `Ok(None)` if none came in time. A stream the daemon
    /// ended is an error saying why.
    pub fn next(&mut self, timeout: Option<Duration>) -> Result<Option<SharedFrame>> {
        let stream = self.reader.get_ref();
        let poll = timeout == Some(Duration::ZERO);
        #[cfg(not(windows))]
        stream.set_nonblocking(poll)?;
        #[cfg(windows)]
        {
            // Never flip the shared pipe to NOWAIT while the GPU-retirement
            // worker writes releases. Peek first; a fragmented line has a
            // bounded overlapped read rather than a blocking poll.
            if poll && self.reader.buffer().is_empty() && !isocast_ipc::readable(stream)? {
                return Ok(None);
            }
            stream.set_read_timeout(if poll {
                Some(Duration::from_nanos(1))
            } else {
                timeout
            })?;
        }
        #[cfg(not(windows))]
        if !poll {
            stream.set_read_timeout(timeout)?;
        }
        loop {
            let event = match self.read_event() {
                Ok(Some(event)) => event,
                Ok(None) => bail!("isocastd closed the shared frame stream"),
                Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return Ok(None);
                }
                Err(err) => return Err(err).context("lost the shared frame stream"),
            };
            match event {
                Event::Ring(ring) => {
                    let token = Token::parse(&ring.token)?;
                    let surfaces =
                        isocast_surfaces::fetch(&self.info.service, token, FETCH_TIMEOUT)
                            .context("could not fetch the shared surfaces")?;
                    if surfaces.len() != ring.slots as usize {
                        bail!(
                            "isocastd sent {} surfaces for a ring of {}",
                            surfaces.len(),
                            ring.slots
                        );
                    }
                    #[cfg(feature = "wgpu")]
                    let textures = Mutex::new(vec![None; surfaces.len()]);
                    self.ring = Some(Arc::new(Ring {
                        identity: next_ring_identity()?,
                        generation: ring.generation,
                        surfaces,
                        #[cfg(feature = "wgpu")]
                        textures,
                    }));
                }
                Event::Frame(frame) => {
                    let release = Release {
                        writer: self.writer.clone(),
                        generation: frame.generation,
                        slot: frame.slot,
                        #[cfg(all(windows, feature = "wgpu"))]
                        queue: RENDERING_GPU.get().and_then(|gpu| {
                            gpu.lock().unwrap().as_ref().map(|(_, queue)| queue.clone())
                        }),
                    };
                    let Some(ring) = self
                        .ring
                        .clone()
                        .filter(|ring| ring.generation == frame.generation)
                        .filter(|ring| (frame.slot as usize) < ring.surfaces.len())
                    else {
                        // Not a slot of the ring this client has: hand it
                        // straight back.
                        drop(release);
                        continue;
                    };
                    return Ok(Some(SharedFrame {
                        ring,
                        slot: frame.slot,
                        time: frame.time,
                        revision: frame.revision,
                        _release: release,
                    }));
                }
                Event::End(why) => bail!("the shared frame stream ended: {why}"),
            }
        }
    }

    /// The newest frame that has arrived, waiting up to `timeout` for one
    /// if none has; older frames read on the way are handed back at once.
    pub fn newest(&mut self, timeout: Option<Duration>) -> Result<Option<SharedFrame>> {
        let Some(mut newest) = self.next(timeout)? else {
            return Ok(None);
        };
        // Whatever else is already here.
        while let Some(frame) = self.next(Some(Duration::ZERO))? {
            newest = frame;
        }
        Ok(Some(newest))
    }

    /// A handle that changes this stream's rate or size from any thread.
    pub fn controller(&self) -> SharedController {
        SharedController(self.writer.clone())
    }

    /// The next line's event; `None` at the end of the stream. A timeout
    /// keeps what was read of the line for the next call.
    fn read_event(&mut self) -> std::io::Result<Option<Event>> {
        loop {
            let remaining = 4097usize.saturating_sub(self.line.len());
            let read = std::io::Read::take(&mut self.reader, remaining as u64)
                .read_until(b'\n', &mut self.line)?;
            if self.line.len() > 4096 {
                return Err(std::io::Error::new(
                    ErrorKind::InvalidData,
                    "shared frame event exceeds 4096 bytes",
                ));
            }
            if read == 0 && self.line.is_empty() {
                return Ok(None);
            }
            let line = std::mem::take(&mut self.line);
            // Kinds this build doesn't know (and blank lines) are skipped.
            if let Ok(event) = serde_json::from_slice::<Event>(&line) {
                return Ok(Some(event));
            }
        }
    }
}

#[cfg(windows)]
impl Drop for SharedFrames {
    fn drop(&mut self) {
        // Outstanding GPU-retirement callbacks must not keep a dead viewer's
        // pipe alive or allow its obsolete slots to be republished.
        let _ = self
            .writer
            .lock()
            .unwrap()
            .shutdown(std::net::Shutdown::Both);
    }
}

/// Changes a stream's rate or size (see [`SharedFrames::controller`]).
#[derive(Clone)]
pub struct SharedController(Arc<Mutex<UnixStream>>);

impl SharedController {
    /// At most `fps` frames a second from now on (1 to 60).
    pub fn set_fps(&self, fps: u32) -> Result<()> {
        send(&self.0, &Control::Fps(fps.clamp(1, 60)))
    }

    /// Fit the picture in a new size; a new ring of surfaces follows if its
    /// size changes.
    pub fn set_size(&self, max_width: Option<u32>, max_height: Option<u32>) -> Result<()> {
        send(
            &self.0,
            &Control::Size {
                max_width,
                max_height,
            },
        )
    }
}

fn send(writer: &Mutex<UnixStream>, control: &Control) -> Result<()> {
    let mut line = serde_json::to_vec(control)?;
    line.push(b'\n');
    let mut writer = writer.lock().unwrap();
    let result = writer.write_all(&line);
    #[cfg(windows)]
    if result.is_err() {
        // A timed-out/partial control message cannot safely be retried as a
        // new line. End this viewer instead of silently losing a slot forever.
        let _ = writer.shutdown(std::net::Shutdown::Both);
    }
    result.context("lost the shared frame stream")
}

fn next_ring_identity() -> Result<u64> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .map_err(|_| anyhow::anyhow!("shared image cache identity exhausted"))
}

/// Hands a slot back when dropped.
struct Release {
    writer: Arc<Mutex<UnixStream>>,
    generation: u64,
    slot: u32,
    #[cfg(all(windows, feature = "wgpu"))]
    queue: Option<wgpu::Queue>,
}

impl Drop for Release {
    fn drop(&mut self) {
        let control = Control::Release {
            generation: self.generation,
            slot: self.slot,
        };
        #[cfg(all(windows, feature = "wgpu"))]
        if let Some(queue) = &self.queue {
            let writer = self.writer.clone();
            if let Err(error) = isocast_surfaces::retire(
                queue,
                Box::new(move |safe| {
                    if safe {
                        let _ = send(&writer, &control);
                    } else {
                        let _ = writer.lock().unwrap().shutdown(std::net::Shutdown::Both);
                    }
                }),
            ) {
                // Never release early on a stalled/lost GPU. The bounded ring
                // stalls only this viewer; closing its stream retires the ring.
                eprintln!("shared preview release: {error:#}");
                let _ = self
                    .writer
                    .lock()
                    .unwrap()
                    .shutdown(std::net::Shutdown::Both);
            }
            return;
        }
        // A frame never submitted to a GPU (or a Mac frame retained by the
        // existing presentation contract) can be returned directly.
        let _ = send(&self.writer, &control);
    }
}

/// A frame in a shared surface, the daemon's GPU done drawing it. Keep it
/// while this process's GPU may still read it (until the next frame has
/// been drawn on screen); dropping it hands the slot back to the daemon.
pub struct SharedFrame {
    ring: Arc<Ring>,
    slot: u32,
    /// When the picture was captured or drawn, on the host clock
    /// ([`isocast_surfaces::host_now`]), in nanoseconds.
    pub time: u64,
    /// For the program and the preview, the setup revision the picture
    /// shows: draw marks over it (a selection box) at that revision's
    /// layout.
    pub revision: Option<u64>,
    _release: Release,
}

impl SharedFrame {
    /// Stop this viewer, leaving its retained images immutable. Used when a
    /// Windows dialog changes targets before replacing its displayed image.
    #[cfg(windows)]
    pub fn close_stream(&self) {
        let _ = self
            ._release
            .writer
            .lock()
            .unwrap()
            .shutdown(std::net::Shutdown::Both);
    }
    /// The surface holding the picture.
    pub fn surface(&self) -> &Surface {
        &self.ring.surfaces[self.slot as usize]
    }

    pub fn width(&self) -> u32 {
        self.surface().width()
    }

    pub fn height(&self) -> u32 {
        self.surface().height()
    }

    /// The slot and the ring (generation) it is in.
    pub fn slot(&self) -> (u64, u32) {
        (self.ring.generation, self.slot)
    }

    /// Image-cache identity, distinct even across reconnects and target
    /// changes whose wire generation/slot numbers happen to repeat.
    pub fn cache_key(&self) -> (u64, u32) {
        (self.ring.identity, self.slot)
    }

    /// The picture as an `Rgba8Unorm` texture of `device` (made once per
    /// slot and device, then reused), with the usages Slint's
    /// `Image::try_from(wgpu::Texture)` asks for (`TEXTURE_BINDING |
    /// RENDER_ATTACHMENT`) and `COPY_SRC`. Don't draw into it.
    #[cfg(feature = "wgpu")]
    pub fn texture(&self, device: &wgpu::Device) -> Result<wgpu::Texture> {
        #[cfg(windows)]
        {
            let registered = RENDERING_GPU
                .get()
                .context("call shared::register_gpu before opening a Windows preview")?
                .lock()
                .unwrap();
            if self._release.queue.is_none()
                || registered.as_ref().is_none_or(|(gpu, _)| gpu != device)
            {
                bail!("shared frame must be received after registering its rendering device/queue");
            }
        }
        let mut textures = self.ring.textures.lock().unwrap();
        let cached = &mut textures[self.slot as usize];
        if let Some((made_for, texture)) = cached.as_ref()
            && made_for == device
        {
            return Ok(texture.clone());
        }
        let texture = self.surface().texture(
            device,
            wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC,
        )?;
        *cached = Some((device.clone(), texture.clone()));
        Ok(texture)
    }
}
