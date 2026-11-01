//! Linux: shared memory, not GPU memory. A surface is a sealed `memfd`
//! holding one 8-bit RGBA picture (rows packed, `width * 4` bytes each). The
//! daemon draws on its GPU, reads the picture back into the slot's memory,
//! and announces the slot; the client maps the same memory read-only and
//! takes the pixels while it holds the slot. No H.264 encode or decode, and
//! it works on machines without a GPU.
//!
//! The descriptors travel once per ring over a local socket in the abstract
//! namespace (`SCM_RIGHTS`), the counterpart of the macOS Mach service: the
//! client gets its name in the reply and a one-time token per ring over the
//! control socket. The service answers only processes of the same user
//! (`SO_PEERCRED`) holding a token it offered.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};

use crate::{MAX_SLOTS, Token};

/// Fetches served at once (each waits at most 5 s for its client).
const MAX_FETCHES: usize = 16;

/// The largest side of a surface (as the compositor's device limit).
const MAX_SIDE: u32 = 8192;

/// A picture in memory shared with another process: 8-bit RGBA, rows
/// packed. Clones share the mapping.
#[derive(Clone)]
pub struct Surface(Arc<Mapping>);

struct Mapping {
    fd: OwnedFd,
    ptr: *mut u8,
    len: usize,
    width: u32,
    height: u32,
    writable: bool,
    id: u32,
}

// SAFETY: the mapping is plain shared memory owned by this value; writes
// happen only on the producer's side while the viewer doesn't hold the
// slot (the ring's release contract), reads only while it does.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: mapped in `map` with this pointer and length.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

impl std::fmt::Debug for Surface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Surface({} {}×{})",
            self.id(),
            self.width(),
            self.height()
        )
    }
}

fn bytes_for(width: u32, height: u32) -> Result<usize> {
    ensure!(
        (1..=MAX_SIDE).contains(&width) && (1..=MAX_SIDE).contains(&height),
        "a shared surface can't be {width}×{height} (each side 1 to {MAX_SIDE})"
    );
    Ok(width as usize * height as usize * 4)
}

fn map(fd: OwnedFd, width: u32, height: u32, writable: bool) -> Result<Surface> {
    static NEXT: AtomicU32 = AtomicU32::new(1);
    let len = bytes_for(width, height)?;
    let protection = if writable {
        libc::PROT_READ | libc::PROT_WRITE
    } else {
        libc::PROT_READ
    };
    // SAFETY: a fresh mapping of a descriptor at least `len` long (checked
    // by the callers), shared so both processes see the same pages.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            protection,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error()).context("could not map a shared surface");
    }
    Ok(Surface(Arc::new(Mapping {
        fd,
        ptr: ptr.cast(),
        len,
        width,
        height,
        writable,
        id: NEXT.fetch_add(1, Ordering::Relaxed),
    })))
}

impl Surface {
    /// A new `width`×`height` RGBA surface (black), to draw into and share.
    pub fn rgba(width: u32, height: u32) -> Result<Self> {
        let len = bytes_for(width, height)?;
        // SAFETY: plain syscalls on a descriptor this function owns.
        unsafe {
            let raw = libc::memfd_create(
                c"isocast-frame".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            );
            if raw < 0 {
                return Err(std::io::Error::last_os_error())
                    .context("could not make a shared surface (memfd_create)");
            }
            let fd = OwnedFd::from_raw_fd(raw);
            if libc::ftruncate(raw, len as libc::off_t) != 0 {
                return Err(std::io::Error::last_os_error())
                    .context("could not size a shared surface");
            }
            // Its size is fixed for good: a client's mapping can never run
            // past the end of the file.
            let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
            if libc::fcntl(raw, libc::F_ADD_SEALS, seals) != 0 {
                return Err(std::io::Error::last_os_error())
                    .context("could not seal a shared surface");
            }
            map(fd, width, height, true)
        }
    }

    /// A surface another process made and sent: mapped read-only once its
    /// descriptor proves big enough and sealed against shrinking.
    fn received(fd: OwnedFd, width: u32, height: u32) -> Result<Self> {
        let len = bytes_for(width, height)?;
        // SAFETY: fstat and fcntl on a descriptor this function owns.
        unsafe {
            let mut stat: libc::stat = std::mem::zeroed();
            if libc::fstat(fd.as_raw_fd(), &mut stat) != 0 {
                return Err(std::io::Error::last_os_error())
                    .context("could not read a shared surface's size");
            }
            ensure!(
                stat.st_size >= len as libc::off_t,
                "a shared surface is {} bytes, not the {len} a {width}×{height} picture needs",
                stat.st_size
            );
            let seals = libc::fcntl(fd.as_raw_fd(), libc::F_GET_SEALS);
            ensure!(
                seals >= 0 && seals & libc::F_SEAL_SHRINK != 0,
                "a shared surface isn't sealed against shrinking"
            );
        }
        map(fd, width, height, false)
    }

    pub fn width(&self) -> u32 {
        self.0.width
    }

    pub fn height(&self) -> u32 {
        self.0.height
    }

    /// A number for logs (unique in this process).
    pub fn id(&self) -> u32 {
        self.0.id
    }

    /// The picture: `height` rows of `width * 4` bytes, RGBA. Read it only
    /// while holding its slot (the producer may draw the next picture into
    /// it once released).
    pub fn pixels(&self) -> &[u8] {
        // SAFETY: a live mapping of `len` bytes.
        unsafe { std::slice::from_raw_parts(self.0.ptr, self.0.len) }
    }

    /// Copy a picture into the surface: `rows` holds `height` rows `stride`
    /// bytes apart, each starting with `width * 4` bytes of RGBA (of BGRA
    /// if `bgra`: red and blue are swapped on the way). Shared pictures are
    /// opaque (drawn over black, or premultiplied canvases): alpha is
    /// written as 255. Only the producer writes, and only while no viewer
    /// holds the slot.
    pub fn write(&self, rows: &[u8], stride: usize, bgra: bool) -> Result<()> {
        ensure!(self.0.writable, "this shared surface is read-only here");
        let row = self.0.width as usize * 4;
        let height = self.0.height as usize;
        ensure!(
            stride >= row && rows.len() >= stride * (height - 1) + row,
            "a {}×{} picture needs {height} rows of {row} bytes",
            self.0.width,
            self.0.height
        );
        for y in 0..height {
            let line = &rows[y * stride..y * stride + row];
            // SAFETY: a row inside the mapping (`len` is height rows).
            let out = unsafe { std::slice::from_raw_parts_mut(self.0.ptr.add(y * row), row) };
            let pixels = out
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(line.as_chunks::<4>().0);
            if bgra {
                pixels.for_each(|(to, from)| *to = [from[2], from[1], from[0], 255]);
            } else {
                pixels.for_each(|(to, from)| *to = [from[0], from[1], from[2], 255]);
            }
        }
        Ok(())
    }

    #[cfg(feature = "wgpu")]
    pub fn texture(
        &self,
        _device: &wgpu::Device,
        _usage: wgpu::TextureUsages,
    ) -> Result<wgpu::Texture> {
        bail!("Linux shared frames are in shared memory, not GPU memory: read them with pixels()")
    }
}

/// The host clock now, in nanoseconds (wall time; Linux engines time their
/// frames on a clock of their own, so it doesn't compare with frame times).
pub fn host_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos() as u64)
}

/// The daemon's side: a local socket in the abstract namespace that hands
/// each offered ring's descriptors to the one client holding its token.
pub struct Exporter {
    service: String,
    offers: Arc<Mutex<HashMap<[u8; 16], Vec<Surface>>>>,
}

impl Exporter {
    /// The process's exporter, its socket bound on first use.
    pub fn shared() -> Result<&'static Exporter> {
        static SHARED: OnceLock<Result<Exporter, String>> = OnceLock::new();
        SHARED
            .get_or_init(|| Exporter::start().map_err(|err| format!("{err:#}")))
            .as_ref()
            .map_err(|err| anyhow::anyhow!("{err}"))
    }

    fn start() -> Result<Self> {
        let name = format!(
            "isocast-surfaces-{}-{}",
            std::process::id(),
            Token::random()
        );
        let address = SocketAddr::from_abstract_name(name.as_bytes())?;
        let listener = UnixListener::bind_addr(&address)
            .with_context(|| format!("could not open the shared surface service @{name}"))?;
        let offers: Arc<Mutex<HashMap<[u8; 16], Vec<Surface>>>> = Arc::default();
        let serving = offers.clone();
        std::thread::Builder::new()
            .name("shared surfaces".into())
            .spawn(move || {
                // Fetches being served now: past MAX_FETCHES a connection
                // is closed without a reply. The app retries a few fresh
                // offers before falling back; a flood can't spawn unbounded threads.
                let serving_now = Arc::new(AtomicUsize::new(0));
                for stream in listener.incoming() {
                    let stream = match stream {
                        Ok(stream) => stream,
                        // Out of descriptors, say: don't spin on it.
                        Err(_) => {
                            std::thread::sleep(Duration::from_millis(50));
                            continue;
                        }
                    };
                    if serving_now.fetch_add(1, Ordering::AcqRel) >= MAX_FETCHES {
                        serving_now.fetch_sub(1, Ordering::AcqRel);
                        continue;
                    }
                    let offers = serving.clone();
                    let done = serving_now.clone();
                    // One short-lived thread per fetch: a client that
                    // connects and says nothing stalls only itself.
                    let spawned = std::thread::Builder::new()
                        .name("shared surface fetch".into())
                        // A failed fetch is the client's to report: it gets
                        // no surfaces and says why.
                        .spawn(move || {
                            let _ = serve(stream, &offers);
                            done.fetch_sub(1, Ordering::AcqRel);
                        });
                    if spawned.is_err() {
                        serving_now.fetch_sub(1, Ordering::AcqRel);
                    }
                }
            })
            .context("could not start the shared surface service")?;
        Ok(Self {
            service: format!("@{name}"),
            offers,
        })
    }

    /// The service's name, for the client (`@` and its abstract name).
    pub fn service(&self) -> &str {
        &self.service
    }

    /// Offer `surfaces` to whoever brings the returned token, once.
    pub fn offer(&self, surfaces: Vec<Surface>) -> Token {
        let token = Token::random();
        self.offers.lock().unwrap().insert(token.0, surfaces);
        token
    }

    /// Withdraw an offer not taken yet.
    pub fn withdraw(&self, token: Token) {
        self.offers.lock().unwrap().remove(&token.0);
    }
}

fn serve(mut stream: UnixStream, offers: &Mutex<HashMap<[u8; 16], Vec<Surface>>>) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    ensure!(
        peer_uid(stream.as_raw_fd())? == unsafe { libc::getuid() },
        "a process of another user asked for shared surfaces"
    );
    let mut token = [0u8; 16];
    stream.read_exact(&mut token)?;
    let Some(surfaces) = offers.lock().unwrap().remove(&token) else {
        // Nothing for that token: an empty answer.
        stream.write_all(&0u32.to_le_bytes())?;
        return Ok(());
    };
    let mut header = Vec::with_capacity(4 + surfaces.len() * 8);
    header.extend_from_slice(&(surfaces.len() as u32).to_le_bytes());
    for surface in &surfaces {
        header.extend_from_slice(&surface.width().to_le_bytes());
        header.extend_from_slice(&surface.height().to_le_bytes());
    }
    let fds: Vec<RawFd> = surfaces.iter().map(|s| s.0.fd.as_raw_fd()).collect();
    send_with_fds(stream.as_raw_fd(), &header, &fds)
}

fn peer_uid(fd: RawFd) -> Result<libc::uid_t> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt into a ucred of the size given.
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("could not check the client's user");
    }
    Ok(cred.uid)
}

/// The control-message space for `count` descriptors.
fn space_for(count: usize) -> usize {
    // SAFETY: CMSG_SPACE is a pure size computation.
    unsafe { libc::CMSG_SPACE((count * size_of::<RawFd>()) as u32) as usize }
}

fn send_with_fds(fd: RawFd, data: &[u8], fds: &[RawFd]) -> Result<()> {
    let mut control = vec![0u8; space_for(fds.len().max(1))];
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut libc::c_void,
        iov_len: data.len(),
    };
    // SAFETY: a msghdr over live buffers; the control message holds exactly
    // `fds.len()` descriptors.
    unsafe {
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        if !fds.is_empty() {
            message.msg_control = control.as_mut_ptr().cast();
            message.msg_controllen = space_for(fds.len()) as _;
            let header = libc::CMSG_FIRSTHDR(&message);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(size_of_val(fds) as u32) as _;
            std::ptr::copy_nonoverlapping(
                fds.as_ptr(),
                libc::CMSG_DATA(header).cast::<RawFd>(),
                fds.len(),
            );
        }
        let sent = libc::sendmsg(fd, &message, libc::MSG_NOSIGNAL);
        if sent < 0 {
            return Err(std::io::Error::last_os_error())
                .context("could not send the shared surfaces");
        }
        ensure!(
            sent as usize == data.len(),
            "sent part of the shared surfaces' description"
        );
    }
    Ok(())
}

/// Receive one message of at most `data.len()` bytes and up to `MAX_SLOTS`
/// descriptors (owned by the caller from here on).
fn receive_with_fds(fd: RawFd, data: &mut [u8]) -> Result<(usize, Vec<OwnedFd>)> {
    let mut control = vec![0u8; space_for(MAX_SLOTS)];
    let mut iov = libc::iovec {
        iov_base: data.as_mut_ptr().cast(),
        iov_len: data.len(),
    };
    let mut fds = Vec::new();
    // SAFETY: a msghdr over live buffers; every received descriptor is
    // taken into an OwnedFd at once (closed on any error below).
    let read = unsafe {
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = control.len() as _;
        let read = libc::recvmsg(fd, &mut message, libc::MSG_CMSG_CLOEXEC);
        if read < 0 {
            return Err(std::io::Error::last_os_error())
                .context("could not receive the shared surfaces");
        }
        let mut header = libc::CMSG_FIRSTHDR(&message);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let bytes = (*header).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let data = libc::CMSG_DATA(header).cast::<RawFd>();
                for index in 0..bytes / size_of::<RawFd>() {
                    fds.push(OwnedFd::from_raw_fd(data.add(index).read_unaligned()));
                }
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
        if message.msg_flags & libc::MSG_CTRUNC != 0 {
            bail!("more shared surfaces came than a ring may have");
        }
        read as usize
    };
    Ok((read, fds))
}

/// Fetch the surfaces of the ring `token` was offered for, from `service`
/// (the daemon's reply), within `timeout`.
pub fn fetch(service: &str, token: Token, timeout: Duration) -> Result<Vec<Surface>> {
    let name = service
        .strip_prefix('@')
        .with_context(|| format!("service: {service:?} isn't a Linux shared surface service"))?;
    let address = SocketAddr::from_abstract_name(name.as_bytes())?;
    let mut stream = UnixStream::connect_addr(&address)
        .with_context(|| format!("could not reach the shared surface service {service}"))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(&token.0)?;
    let mut header = [0u8; 4 + MAX_SLOTS * 8];
    let (read, fds) = receive_with_fds(stream.as_raw_fd(), &mut header)?;
    ensure!(read >= 4, "the shared surface service answered nothing");
    let count = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
    ensure!(
        count > 0,
        "the shared surface service had no ring for that token"
    );
    ensure!(
        count <= MAX_SLOTS && read >= 4 + count * 8 && fds.len() == count,
        "the shared surface service sent {} descriptors for {count} surfaces",
        fds.len()
    );
    fds.into_iter()
        .enumerate()
        .map(|(index, fd)| {
            let at = 4 + index * 8;
            let width = u32::from_le_bytes(header[at..at + 4].try_into().unwrap());
            let height = u32::from_le_bytes(header[at + 4..at + 8].try_into().unwrap());
            Surface::received(fd, width, height)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ring_travels_once_and_shows_what_was_written() {
        let exporter = Exporter::shared().unwrap();
        let surfaces: Vec<Surface> = (0..3).map(|_| Surface::rgba(4, 2).unwrap()).collect();
        // Opaque, as shared pictures are (alpha is written as 255).
        let picture: Vec<u8> = (0..4 * 2 * 4)
            .map(|v| if v % 4 == 3 { 255 } else { v as u8 })
            .collect();
        surfaces[1].write(&picture, 16, false).unwrap();
        let token = exporter.offer(surfaces.clone());
        let fetched = fetch(exporter.service(), token, Duration::from_secs(2)).unwrap();
        assert_eq!(fetched.len(), 3);
        assert_eq!((fetched[1].width(), fetched[1].height()), (4, 2));
        assert_eq!(fetched[1].pixels(), &picture[..]);
        // The same memory: a later picture shows through the client's map.
        let next: Vec<u8> = picture
            .iter()
            .enumerate()
            .map(|(i, v)| if i % 4 == 3 { 255 } else { v ^ 0xff })
            .collect();
        surfaces[1].write(&next, 16, false).unwrap();
        assert_eq!(fetched[1].pixels(), &next[..]);
        // Read-only on the client's side; a token works once.
        assert!(fetched[0].write(&picture, 16, false).is_err());
        let again = fetch(exporter.service(), token, Duration::from_secs(2)).unwrap_err();
        assert!(format!("{again:#}").contains("no ring"), "{again:#}");
    }

    #[test]
    fn rows_with_padding_are_packed() {
        let surface = Surface::rgba(2, 2).unwrap();
        // Rows 16 bytes apart, 8 used.
        let mut rows = vec![0u8; 16 + 8];
        rows[..8].copy_from_slice(&[1; 8]);
        rows[16..].copy_from_slice(&[2; 8]);
        surface.write(&rows, 16, false).unwrap();
        assert_eq!(
            surface.pixels(),
            &[1, 1, 1, 255, 1, 1, 1, 255, 2, 2, 2, 255, 2, 2, 2, 255]
        );
        assert!(surface.write(&rows[..20], 16, false).is_err());
        // BGRA in, RGBA out.
        let bgra = [10, 20, 30, 255, 40, 50, 60, 255];
        surface.write(&[bgra, bgra].concat(), 8, true).unwrap();
        assert_eq!(&surface.pixels()[..8], &[30, 20, 10, 255, 60, 50, 40, 255]);
        assert!(Surface::rgba(0, 4).is_err());
    }
}
