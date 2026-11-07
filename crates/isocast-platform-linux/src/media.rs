//! Pull file-clock samples from the sibling GStreamer process, never from a
//! plugin in the recorder. The caller paces or holds a cued reader; only an
//! outstanding operation has a deadline. A closed pipe is not a file end.
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::{ffi::OsStrExt, net::UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::frames::{Frame, FramePool};
use anyhow::{Context, Result, bail, ensure};
use isocast_engine::{FramePool as _, MediaInfo, MediaPart};
use isocast_media_protocol::{Header, VERSION, read_header, write_record};
use isocast_protocol::api::{Check, CheckStatus};

// Cold init may scan every installed plugin before saving the registry cache.
pub(super) const REGISTRY_INIT: Duration = Duration::from_secs(60);
const OPEN: Duration = Duration::from_millis(7500);
const SAMPLE: Duration = Duration::from_secs(10);
const FIX: &str = "install the matching isocast-media helper, GStreamer 1.x and its base plugins";

struct Channel {
    socket: UnixStream,
    until: Instant,
}
impl Channel {
    fn remaining(&self) -> io::Result<Duration> {
        self.until
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or(io::ErrorKind::TimedOut.into())
    }
    fn body(&mut self, n: usize) -> Result<Vec<u8>> {
        let mut bytes = vec![0; n];
        self.read_exact(&mut bytes)?;
        Ok(bytes)
    }
    fn reply(&mut self) -> Result<Header> {
        let header = read_header(self)?;
        if let Header::Fail(n) = header {
            bail!("{}", String::from_utf8_lossy(&self.body(n)?));
        }
        Ok(header)
    }
}
// Socket timeout by itself is per syscall and resets on a short read. Recompute
// the remaining budget for every header field and partial payload instead.
impl Read for Channel {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.socket.set_read_timeout(Some(self.remaining()?))?;
        self.socket.read(buf)
    }
}
impl Write for Channel {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.socket.set_write_timeout(Some(self.remaining()?))?;
        self.socket.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Helper {
    child: Child,
    channel: Channel,
    what: String,
}
impl Helper {
    fn start(op: &str, path: Option<&Path>) -> Result<Self> {
        let exe = match std::env::var_os("ISOCAST_MEDIA_HELPER") {
            Some(path) => PathBuf::from(path),
            None => std::env::current_exe()?.with_file_name("isocast-media"),
        };
        let (socket, peer) = UnixStream::pair()?;
        let mut cmd = Command::new(&exe);
        cmd.args([VERSION, op])
            .arg(std::process::id().to_string())
            .stdin(Stdio::from(OwnedFd::from(peer.try_clone()?)))
            .stdout(Stdio::from(OwnedFd::from(peer)))
            .stderr(Stdio::inherit());
        let child = cmd
            .spawn()
            .with_context(|| format!("starting {}: {FIX}", exe.display()))?;
        drop(cmd); // no parent copy of the helper's socket on EOF
        let what = match path {
            Some(p) => format!("{op} {}", p.display()),
            None => op.to_owned(),
        };
        let mut h = Self {
            child,
            channel: Channel {
                socket,
                until: Instant::now() + REGISTRY_INIT,
            },
            what: format!("GStreamer registry initialization for {what}"),
        };
        h.call(|c| {
            ensure!(c.reply()? == Header::Initialized, "expected INIT");
            Ok(())
        })?;
        h.what = what;
        h.channel.until = Instant::now() + OPEN;
        if let Some(p) = path {
            let bytes = p.as_os_str().as_bytes();
            ensure!(bytes.len() <= 4096, "media file path exceeds 4096 bytes");
            h.call(|c| Ok(write_record(c, b"PATH", &[bytes])?))?;
        }
        Ok(h)
    }
    fn call<T>(&mut self, f: impl FnOnce(&mut Channel) -> Result<T>) -> Result<T> {
        match f(&mut self.channel) {
            Ok(v) => Ok(v),
            Err(e) => {
                let timed = e.downcast_ref::<io::Error>().is_some_and(|io| {
                    matches!(
                        io.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    )
                });
                if timed {
                    bail!("media decoder timed out during {}", self.what)
                }
                let status = self
                    .child
                    .try_wait()?
                    .map(|s| format!(" ({s})"))
                    .unwrap_or_default();
                Err(e).with_context(|| {
                    format!(
                        "isocast-media {} failed{status}; {FIX} if the runtime is missing",
                        self.what
                    )
                })
            }
        }
    }
}
impl Drop for Helper {
    fn drop(&mut self) {
        // No cooperative shutdown: a stopped plugin cannot consume a quit message.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn media_info(path: &Path) -> Result<MediaInfo> {
    Helper::start("info", Some(path))?.call(|c| {
        let Header::Info(n) = c.reply()? else {
            bail!("expected INFO")
        };
        let (video, audio, ns): (Option<(u32, u32)>, bool, u64) =
            serde_json::from_slice(&c.body(n)?)?;
        ensure!(
            ns > 0 && video.is_none_or(|(w, h)| (1..=8192).contains(&w) && (1..=8192).contains(&h)),
            "invalid media dimensions/duration"
        );
        Ok(MediaInfo {
            video,
            audio,
            duration: Duration::from_nanos(ns),
        })
    })
}
pub fn decoders_check() -> Check {
    let probe = || {
        Helper::start("check", None)?.call(|c| {
            let Header::Check(n) = c.reply()? else {
                bail!("expected CHCK")
            };
            Ok(serde_json::from_slice(&c.body(n)?)?)
        })
    };
    probe().unwrap_or_else(|e| Check {
        name: "media decoders (GStreamer)".into(),
        status: CheckStatus::Warning,
        detail: format!("media file sources unavailable: {e:#}"),
        fix: Some(FIX.into()),
    })
}
pub fn open_media(path: &Path, part: MediaPart) -> Result<Reader> {
    let mut helper = Helper::start(
        match part {
            MediaPart::Video => "video",
            MediaPart::Audio => "audio",
        },
        Some(path),
    )?;
    helper.call(|c| {
        ensure!(c.reply()? == Header::Ready, "expected REDY");
        Ok(())
    })?;
    Ok(Reader {
        helper,
        pool: None,
        ended: false,
    })
}
pub struct Reader {
    helper: Helper,
    pool: Option<FramePool>,
    ended: bool,
}
impl Reader {
    fn next<T>(
        &mut self,
        f: impl FnOnce(Header, &mut Channel, &mut Option<FramePool>) -> Result<T>,
    ) -> Result<Option<T>> {
        if self.ended {
            return Ok(None);
        }
        self.helper.channel.until = Instant::now() + SAMPLE;
        let result = self.helper.call(|c| {
            write_record(c, b"NEXT", &[])?;
            match c.reply()? {
                Header::End => Ok(None),
                h => Ok(Some(f(h, c, &mut self.pool)?)),
            }
        })?;
        self.ended = result.is_none();
        Ok(result)
    }
}
impl isocast_engine::MediaReader for Reader {
    type Frame = Frame;
    fn next_video(&mut self) -> Result<Option<(Frame, Duration)>> {
        self.next(|header, c, pool| {
            let Header::Video { ns, w, h, bytes } = header else {
                bail!("expected VBGR or END")
            };
            if pool.as_ref().is_none_or(|p| p.size() != (w, h)) {
                *pool = Some(FramePool::new(w, h)?);
            }
            let pool = pool.as_mut().expect("initialized");
            let start = Instant::now();
            loop {
                let mut read = Ok(());
                if let Some(frame) = pool.draw(&mut |out, _| {
                    read = c.read_exact(&mut out[..bytes]);
                })? {
                    read?;
                    return Ok((frame, Duration::from_nanos(ns)));
                }
                ensure!(
                    start.elapsed() < Duration::from_secs(2),
                    "every decoded frame is still in use"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
        })
    }
    fn next_audio(&mut self) -> Result<Option<(Vec<f32>, Duration)>> {
        self.next(|header, c, _| {
            let Header::Audio { ns, bytes } = header else {
                bail!("expected AF32 or END")
            };
            let pcm = c
                .body(bytes)?
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect();
            Ok((pcm, Duration::from_nanos(ns)))
        })
    }
}
