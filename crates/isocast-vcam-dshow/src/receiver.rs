use isocast_ipc::Stream;
use isocast_vcam_wire::{HEADER_BYTES, Header, Mode, destination};
use std::{
    io::{self, Read},
    net::Shutdown,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::Duration,
};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;

fn session(pid: u32) -> io::Result<u32> {
    let mut id = 0;
    if unsafe { ProcessIdToSessionId(pid, &mut id) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(id)
    }
}
fn endpoint() -> io::Result<(PathBuf, u32)> {
    let id = session(std::process::id())?;
    Ok((destination(&isocast_protocol::api::socket_path(), id), id))
}
fn connect((path, id): &(PathBuf, u32)) -> io::Result<Stream> {
    let pipe = Stream::connect(path)?;
    if session(isocast_ipc::peer_pid(&pipe)?)? != *id {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(pipe)
}
fn header(pipe: &mut Stream, last: Option<Header>) -> io::Result<Header> {
    let mut h = [0; HEADER_BYTES];
    pipe.read_exact(&mut h)?;
    Header::decode(&h, last)
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Format {
    width: u32,
    height: u32,
    fps: u32,
}

#[unsafe(no_mangle)]
extern "C" fn camera_probe() -> Format {
    let mode = (|| -> io::Result<Mode> {
        let mut pipe = connect(&endpoint()?)?;
        pipe.set_read_timeout(Some(Duration::from_millis(200)))?;
        Ok(header(&mut pipe, None)?.mode)
    })()
    .unwrap_or_else(|_| Mode::new(1280, 720, 30).unwrap());
    Format {
        width: mode.width(),
        height: mode.height(),
        fps: mode.fps(),
    }
}

struct Picture {
    mode: Mode,
    data: Vec<u8>,
}
#[derive(Default)]
struct State {
    stopped: bool,
    pipe: Option<Stream>,
    latest: Option<Arc<Picture>>,
}
type Shared = (Mutex<State>, Condvar);

fn read_generation(shared: &Shared, mut pipe: Stream) -> io::Result<()> {
    pipe.set_read_timeout(Some(Duration::from_secs(2)))?;
    {
        let mut state = shared.0.lock().unwrap();
        if state.stopped {
            return Ok(());
        }
        state.pipe = Some(pipe.try_clone()?);
    }
    let mut previous = None;
    let mut spare = Vec::new();
    loop {
        let h = header(&mut pipe, previous)?;
        spare.resize(h.mode.nv12_len(), 0);
        pipe.read_exact(&mut spare)?;
        // Consecutive held samples carry identical bytes. Preserve the Arc so
        // reconnect/fallback scaling and YUY2 packing aren't repeated at 30 Hz.
        if !h.repeated || previous.is_none_or(|p| h.sequence - p.sequence != 1) {
            let picture = Arc::new(Picture {
                mode: h.mode,
                data: std::mem::take(&mut spare),
            });
            let old = shared.0.lock().unwrap().latest.replace(picture);
            if let Some(p) = old.and_then(Arc::into_inner) {
                spare = p.data;
            }
        }
        previous = Some(h);
    }
}

struct Receiver {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    mode: Mode,
    yuy2: bool,
    cached: Option<Arc<Picture>>,
    output: Vec<u8>,
}

impl Receiver {
    fn open(mode: Mode, yuy2: bool) -> io::Result<Self> {
        let shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let feed = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("Isocast camera IPC".into())
            .spawn(move || {
                while !feed.0.lock().unwrap().stopped {
                    // Any transport, identity or format failure invalidates this
                    // generation; never retain old pixels while attempting reopen.
                    if let Ok(pipe) = endpoint().and_then(|ep| connect(&ep)) {
                        let _ = read_generation(&feed, pipe);
                    }
                    let mut state = feed.0.lock().unwrap();
                    state.latest = None;
                    state.pipe = None;
                    let _wait = feed
                        .1
                        .wait_timeout_while(state, Duration::from_millis(200), |s| !s.stopped)
                        .unwrap();
                }
            })?;
        let output = render(mode, yuy2, None);
        Ok(Self {
            shared,
            worker: Some(worker),
            mode,
            yuy2,
            cached: None,
            output,
        })
    }
    fn fill(&mut self, dst: &mut [u8]) -> bool {
        let live = self.shared.0.lock().unwrap().latest.clone();
        if let Some(p) = live.as_ref()
            && !self.yuy2
            && p.mode.width() == self.mode.width()
            && p.mode.height() == self.mode.height()
        {
            // Native mode: copy directly to the consumer's allocator buffer.
            dst.copy_from_slice(&p.data);
            self.cached = live;
            return true;
        }
        let unchanged = match (&live, &self.cached) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !unchanged {
            self.output = render(self.mode, self.yuy2, live.as_deref());
            self.cached = live;
        }
        dst.copy_from_slice(&self.output);
        true
    }
}
impl Drop for Receiver {
    fn drop(&mut self) {
        let mut state = self.shared.0.lock().unwrap();
        state.stopped = true;
        let pipe = state.pipe.take();
        drop(state);
        if let Some(p) = pipe {
            let _ = p.shutdown(Shutdown::Both);
        }
        self.shared.1.notify_all();
        if let Some(t) = self.worker.take() {
            let _ = t.join();
        }
    }
}

// A graph's sample type cannot silently change after connection. Fit a new
// generation to the existing pin (e.g. a graph opened while the camera was off).
fn render(mode: Mode, yuy2: bool, picture: Option<&Picture>) -> Vec<u8> {
    let (w, h) = (mode.width() as usize, mode.height() as usize);
    let mut out = vec![128; mode.nv12_len()];
    out[..w * h].fill(16);
    if let Some(p) = picture {
        let (sw, sh) = (p.mode.width() as usize, p.mode.height() as usize);
        if (sw, sh) == (w, h) {
            out.copy_from_slice(&p.data);
        } else {
            // Widen before products: both the wire and this DLL support x86.
            let scale = |a: usize, b: usize, c: usize| {
                usize::try_from(a as u64 * b as u64 / c as u64).unwrap_or(usize::MAX)
            };
            let dw = scale(sw, h, sh).min(w).max(2) & !1;
            let dh = scale(sh, w, sw).min(h).max(2) & !1;
            let (ox, oy) = (((w - dw) / 2) & !1, ((h - dh) / 2) & !1);
            for y in 0..dh {
                let sy = scale(y, sh, dh);
                for x in 0..dw {
                    let sx = scale(x, sw, dw);
                    out[(oy + y) * w + ox + x] = p.data[sy * sw + sx];
                    if (x | y) & 1 == 0 {
                        let src = sw * sh + (sy / 2) * sw + (sx & !1);
                        let dst = w * h + (oy + y) / 2 * w + ox + x;
                        out[dst..dst + 2].copy_from_slice(&p.data[src..src + 2]);
                    }
                }
            }
        }
    } else {
        // A centered, quiet "OFF" slate, drawn only on a live -> off edge.
        let glyphs = [[7, 5, 5, 5, 7], [7, 4, 6, 4, 4], [7, 4, 6, 4, 4]];
        let s = (w / 32).min(h / 16);
        for (n, g) in glyphs.iter().enumerate() {
            for (y, row) in g.iter().enumerate() {
                for x in 0..3 {
                    if row & (4 >> x) != 0 {
                        for dy in 0..s {
                            let i = ((h - 5 * s) / 2 + y * s + dy) * w
                                + (w - 11 * s) / 2
                                + (n * 4 + x) * s;
                            out[i..i + s].fill(170);
                        }
                    }
                }
            }
        }
    }
    if !yuy2 {
        return out;
    }
    let mut packed = vec![0; w * h * 2];
    for (i, pair) in packed.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let y = 2 * i;
        let uv = w * h + (y / w / 2) * w + y % w;
        pair.copy_from_slice(&[out[y], out[uv], out[y + 1], out[uv + 1]]);
    }
    packed
}

// The SDK filter owns this box for one Run interval. Only its delivery thread
// calls fill; Stop joins that thread before returning ownership to close.
#[unsafe(no_mangle)]
extern "C" fn camera_open(format: Format, yuy2: bool) -> *mut Receiver {
    Mode::new(format.width, format.height, format.fps)
        .and_then(|m| Receiver::open(m, yuy2))
        .map_or(std::ptr::null_mut(), |r| Box::into_raw(Box::new(r)))
}
#[unsafe(no_mangle)]
unsafe extern "C" fn camera_fill(receiver: *mut Receiver, data: *mut u8, len: usize) -> bool {
    let r = unsafe { &mut *receiver };
    let pixels = r.mode.width() as usize * r.mode.height() as usize;
    let required = if r.yuy2 {
        pixels * 2
    } else {
        r.mode.nv12_len()
    };
    if data.is_null() || len < required {
        return false;
    }
    r.fill(unsafe { std::slice::from_raw_parts_mut(data, required) })
}
#[unsafe(no_mangle)]
unsafe extern "C" fn camera_close(receiver: *mut Receiver) {
    if !receiver.is_null() {
        drop(unsafe { Box::from_raw(receiver) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yuy2_keeps_luma_and_reuses_chroma_on_both_lines() {
        let mode = Mode::new(2, 2, 30).unwrap();
        let p = Picture {
            mode,
            data: vec![16, 37, 52, 230, 109, 140],
        };
        assert_eq!(
            render(mode, true, Some(&p)),
            [16, 109, 37, 140, 52, 109, 230, 140]
        );
        let wide = Mode::new(8, 2, 30).unwrap();
        let bytes = render(wide, false, Some(&p));
        assert_eq!(&bytes[..8], &[16, 16, 16, 37, 16, 16, 16, 16]);
        assert_eq!(&bytes[16..], &[128, 128, 109, 140, 128, 128, 128, 128]);
    }
}
