//! Local X11 fallback. A pick hands off a live X connection, never a saved
//! numeric window id (X servers recycle ids). No automatic target selection.
mod shm;

use crate::{
    frames::{Frame, FramePool},
    pipewire_video::even,
};
use anyhow::{Context, Result, bail, ensure};
use isocast_engine::{CaptureFormat, FramePool as _, VideoSink};
use std::{
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::{
        Event,
        composite::{ConnectionExt as _, Redirect},
        randr::ConnectionExt as _,
        shm::ConnectionExt as _,
        xfixes::{ConnectionExt as _, GetCursorImageReply},
        xproto::*,
    },
    rust_connection::RustConnection,
};

#[derive(Clone, Copy)]
enum Target {
    Monitor(Atom),
    Window(Window),
}

pub(super) struct Share {
    conn: Arc<RustConnection>,
    root: Window,
    target: Target,
    /// Drawable (window targets use a named pixmap), read rectangle, root-space origin.
    drawable: Drawable,
    // A hidden window has no current Composite pixmap. Its last published
    // frame stays in the engine's slot until we bind a new pixmap on map.
    mapped: bool,
    rect: Rectangle,
    origin: (i16, i16),
    visual: Visualtype,
    pub cursor: bool,
    reason: String,
}

fn monitors(c: &RustConnection, root: Window) -> Result<Vec<x11rb::protocol::randr::MonitorInfo>> {
    Ok(c.randr_get_monitors(root, true)?.reply()?.monitors)
}

fn candidates(c: &RustConnection, root: Window, window: bool) -> Result<Vec<(Target, String)>> {
    if !window {
        return monitors(c, root)?
            .into_iter()
            .map(|m| {
                let atom = c.get_atom_name(m.name)?.reply()?;
                Ok((
                    Target::Monitor(m.name),
                    format!(
                        "{} ({} × {})",
                        String::from_utf8_lossy(&atom.name),
                        m.width,
                        m.height
                    ),
                ))
            })
            .collect();
    }
    let atom = |name: &[u8]| -> Result<u32> { Ok(c.intern_atom(false, name)?.reply()?.atom) };
    let clients = c
        .get_property(
            false,
            root,
            atom(b"_NET_CLIENT_LIST")?,
            AtomEnum::WINDOW,
            0,
            4096,
        )?
        .reply()?;
    let ids: Vec<_> = match clients.value32() {
        Some(ids) => ids.collect(),
        None => c.query_tree(root)?.reply()?.children,
    };
    let title = atom(b"_NET_WM_NAME")?;
    let mut choices = Vec::new();
    for id in ids.into_iter().take(4096) {
        let Ok(attrs) = c.get_window_attributes(id)?.reply() else {
            continue;
        };
        if attrs.map_state != MapState::VIEWABLE
            || attrs.class != WindowClass::INPUT_OUTPUT
            || attrs.override_redirect
        {
            continue;
        }
        if c.change_window_attributes(
            id,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
        )?
        .check()
        .is_err()
        {
            continue;
        }
        let mut title_text = String::new();
        for property in [title, AtomEnum::WM_NAME.into()] {
            if let Ok(reply) = c
                .get_property(false, id, property, AtomEnum::ANY, 0, 96)?
                .reply()
                && reply.format == 8
                && !reply.value.is_empty()
            {
                title_text = label_text(&reply.value, reply.bytes_after != 0);
                break;
            }
        }
        let class = c
            .get_property(false, id, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 96)?
            .reply()
            .ok();
        let app = class
            .as_ref()
            .and_then(|r| {
                // ICCCM: instance NUL class NUL. Don't mistake a partial
                // instance name for its class if the property was truncated.
                let mut fields = r.value.split(|b| *b == 0);
                let instance = fields.next()?;
                Some(label_text(
                    fields.next().filter(|s| !s.is_empty()).unwrap_or(instance),
                    false,
                ))
            })
            .unwrap_or_default();
        let title = if title_text.is_empty() {
            "Untitled window"
        } else {
            &title_text
        };
        choices.push((
            Target::Window(id),
            if app.is_empty() {
                title.to_owned()
            } else {
                format!("{app} — {title}")
            },
        ));
    }
    let mut counts = std::collections::HashMap::new();
    for (_, label) in &choices {
        *counts.entry(label.clone()).or_insert(0usize) += 1;
    }
    for (target, label) in &mut choices {
        if counts[label] > 1
            && let Target::Window(id) = target
        {
            label.push_str(&format!(" (0x{id:x})"));
        }
    }
    Ok(choices)
}

fn label_text(bytes: &[u8], truncated: bool) -> String {
    // XGetProperty limits the wire read in 4-byte units, not UTF-8
    // characters. Ignore an incomplete final character and elide by char.
    let end = match std::str::from_utf8(bytes) {
        Err(e) if truncated && e.error_len().is_none() => e.valid_up_to(),
        _ => bytes.len(),
    };
    let text = String::from_utf8_lossy(&bytes[..end]);
    let mut chars = text.trim().chars().filter(|c| !c.is_control());
    let mut label = chars.by_ref().take(96).collect::<String>();
    if truncated || chars.next().is_some() {
        label.push('…');
    }
    label
}

fn choose(heading: &str, choices: &[(Target, String)]) -> Result<Target> {
    ensure!(!choices.is_empty(), "{heading}: no available targets");
    let exe = std::env::current_exe()?.with_file_name("isocast-ui");
    let mut command = Command::new(&exe);
    // Ctrl-C shuts down a foreground recording's private daemon. Its dialog
    // must not outlive it and ask the user to share with a vanished source.
    let parent = std::process::id() as libc::pid_t;
    // SAFETY: only async-signal-safe Linux syscalls between fork and exec;
    // check parent afterward to cover death just before prctl.
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
    let mut picker = command
        .arg("--x11-pick")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("open X11 picker ({})", exe.display()))?;
    let labels: Vec<_> = choices.iter().map(|(_, label)| label).collect();
    let result = (|| -> Result<()> {
        serde_json::to_writer(
            picker.stdin.take().context("picker input")?,
            &(heading, labels),
        )?;
        let start = Instant::now();
        while picker.try_wait()?.is_none() {
            ensure!(
                start.elapsed() < Duration::from_secs(180),
                "X11 picker timed out"
            );
            thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    })();
    if let Err(error) = result {
        let _ = picker.kill();
        let _ = picker.wait();
        return Err(error);
    }
    let answer = picker.wait_with_output()?;
    ensure!(
        answer.status.success(),
        "X11 picker failed: {}",
        answer.status
    );
    let index: isize = std::str::from_utf8(&answer.stdout)?.trim().parse()?;
    choices
        .get(index as usize)
        .map(|(target, _)| *target)
        .context("X11 sharing cancelled")
}

impl Share {
    pub fn pick(window: bool, cursor: bool, reason: String) -> Result<Self> {
        let (conn, screen) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen].root;
        let choices = candidates(&conn, root, window)?;
        let target = choose(
            if window {
                "Choose a window to share"
            } else {
                "Choose a screen to share"
            },
            &choices,
        )?;
        Self::open(conn, screen, target, cursor, reason)
    }

    fn open(
        conn: RustConnection,
        screen: usize,
        target: Target,
        cursor: bool,
        reason: String,
    ) -> Result<Self> {
        let root = conn.setup().roots[screen].root;
        if cursor {
            conn.xfixes_query_version(4, 0)?
                .reply()
                .context("XFixes is required to include the cursor")?;
        }
        let window = match target {
            Target::Monitor(_) => root,
            Target::Window(w) => {
                conn.composite_query_version(0, 4)?
                    .reply()
                    .context("XComposite is required for window sharing")?;
                conn.change_window_attributes(
                    w,
                    &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
                )?
                .check()?;
                conn.composite_redirect_window(w, Redirect::AUTOMATIC)?
                    .check()?;
                w
            }
        };
        let id = conn.get_window_attributes(window)?.reply()?.visual;
        let visual = conn.setup().roots[screen]
            .allowed_depths
            .iter()
            .flat_map(|d| &d.visuals)
            .find(|v| v.visual_id == id)
            .copied()
            .context("unknown X11 visual")?;
        ensure!(
            visual.class == VisualClass::TRUE_COLOR,
            "X11 capture needs a TrueColor visual"
        );
        ensure!(
            [visual.red_mask, visual.green_mask, visual.blue_mask]
                .into_iter()
                .all(|m| m != 0 && m.count_ones() <= 16),
            "unsupported X11 color masks"
        );
        let geometry = conn.get_geometry(window)?.reply()?;
        let mut share = Self {
            conn: Arc::new(conn),
            root,
            target,
            drawable: root,
            mapped: true,
            rect: Rectangle {
                width: geometry.width,
                height: geometry.height,
                ..Default::default()
            },
            origin: (0, 0),
            visual,
            cursor,
            reason,
        };
        share.refresh(true)?;
        Ok(share)
    }

    fn refresh(&mut self, mut changed: bool) -> Result<()> {
        let c = &self.conn;
        match self.target {
            Target::Window(w) => {
                while let Some(e) = c.poll_for_event()? {
                    match e {
                        Event::DestroyNotify(e) if e.window == w => {
                            bail!("chosen X11 window closed; pick a window again")
                        }
                        Event::UnmapNotify(e) if e.window == w => self.mapped = false,
                        Event::MapNotify(e) if e.window == w => changed = true,
                        Event::ConfigureNotify(e) if e.window == w => changed = true,
                        _ => (),
                    }
                }
                // A reparenting WM may unmap the *frame*, not the client.
                // The client's map_state then becomes Unviewable, with no
                // UnmapNotify on the client itself.
                if c.get_window_attributes(w)?.reply()?.map_state != MapState::VIEWABLE {
                    self.mapped = false;
                    return Ok(());
                }
                if changed || !self.mapped {
                    let geom = c.get_geometry(w)?.reply()?;
                    let pixmap = c.generate_id()?;
                    if let Err(err) = c.composite_name_window_pixmap(w, pixmap)?.check() {
                        if matches!(&err, x11rb::errors::ReplyError::X11Error(e) if e.error_kind == x11rb::protocol::ErrorKind::Match)
                        {
                            // Unmapped between GetWindowAttributes and NameWindowPixmap.
                            self.mapped = false;
                            return Ok(());
                        }
                        return Err(err.into());
                    }
                    if self.drawable != self.root {
                        c.free_pixmap(self.drawable)?.check()?;
                    }
                    self.drawable = pixmap;
                    self.mapped = true;
                    let border = i16::try_from(geom.border_width)?;
                    // The client may have resized between requests: use
                    // the immutable named pixmap's dimensions, not the window's.
                    let backing = c.get_geometry(pixmap)?.reply()?;
                    let inset = geom.border_width.saturating_mul(2);
                    self.rect = Rectangle {
                        x: border,
                        y: border,
                        width: backing.width.saturating_sub(inset),
                        height: backing.height.saturating_sub(inset),
                    };
                }
                if self.cursor {
                    let pos = c.translate_coordinates(w, self.root, 0, 0)?.reply()?;
                    self.origin = (pos.dst_x, pos.dst_y);
                }
            }
            Target::Monitor(name) => {
                let m = monitors(c, self.root)?
                    .into_iter()
                    .find(|m| m.name == name)
                    .context("chosen RandR monitor is gone; pick a screen again")?;
                self.rect = Rectangle {
                    x: m.x,
                    y: m.y,
                    width: m.width,
                    height: m.height,
                };
                self.origin = (m.x, m.y);
            }
        }
        ensure!(
            (1..=8192).contains(&self.rect.width) && (1..=8192).contains(&self.rect.height),
            "unsupported X11 capture size {} × {}",
            self.rect.width,
            self.rect.height
        );
        Ok(())
    }

    pub fn size(&self) -> (u32, u32) {
        (even(self.rect.width.into()), even(self.rect.height.into()))
    }
}

struct Reader {
    share: Share,
    pool: FramePool,
    segment: Option<shm::Segment>,
    shm_enabled: bool,
    depth: u8,
    bytes: usize,
    pad: usize,
}

impl Reader {
    fn new(share: Share) -> Result<Self> {
        let c = &share.conn;
        let depth = c
            .get_geometry(match share.target {
                Target::Window(w) => w,
                Target::Monitor(_) => share.root,
            })?
            .reply()?
            .depth;
        let fmt = *c
            .setup()
            .pixmap_formats
            .iter()
            .find(|f| f.depth == depth)
            .context("unknown X11 pixel layout")?;
        ensure!(
            [16, 24, 32].contains(&fmt.bits_per_pixel) && [8, 16, 32].contains(&fmt.scanline_pad),
            "unsupported X11 pixel layout: {fmt:?}"
        );
        let shm_enabled = c
            .shm_query_version()
            .ok()
            .and_then(|q| q.reply().ok())
            .is_some();
        let (w, h) = share.size();
        Ok(Self {
            share,
            pool: FramePool::new(w, h)?,
            segment: None,
            shm_enabled,
            depth,
            bytes: usize::from(fmt.bits_per_pixel / 8),
            pad: usize::from(fmt.scanline_pad / 8),
        })
    }

    fn frame(&mut self) -> Result<Option<Frame>> {
        self.share.refresh(false)?;
        if !self.share.mapped {
            return Ok(None);
        }
        let share = &self.share;
        let c = &share.conn;
        let Rectangle {
            x,
            y,
            width,
            height,
        } = share.rect;
        // Crop the right/bottom odd pixel at readback. A native-sized
        // 801×601 window should copy 800×600, not scale and letterbox it.
        // A one-pixel axis still needs one input sample for its 2px output.
        let (width, height) = ((width & !1).max(1), (height & !1).max(1));
        let stride = (usize::from(width) * self.bytes).next_multiple_of(self.pad);
        let len = stride * usize::from(height);
        let mut cursor = share
            .cursor
            .then(|| c.xfixes_get_cursor_image()?.reply())
            .transpose()?;
        // Do not paint the pointer of an occluding, unrelated window into
        // the isolated pixmap. Reparenting WMs put the client under a frame.
        if let (Some(image), Target::Window(window)) = (&cursor, share.target) {
            let mut parent = share.root;
            while parent != window {
                let hit = c
                    .translate_coordinates(share.root, parent, image.x, image.y)?
                    .reply()?;
                if hit.child == x11rb::NONE {
                    cursor = None;
                    break;
                }
                parent = hit.child;
            }
        }
        if self.shm_enabled && self.segment.as_ref().is_none_or(|s| s.len < len) {
            self.segment = shm::Segment::new(c, len).inspect_err(|e| {
                isocast_diagnostics::diagnostic!("warn"; "X11 MIT-SHM attach failed; switching capture to GetImage: {e:#}");
            }).ok();
            self.shm_enabled = self.segment.is_some();
        }
        let reply;
        let shm_ok = if let Some(segment) = &self.segment {
            c.shm_get_image(
                share.drawable,
                x,
                y,
                width,
                height,
                u32::MAX,
                ImageFormat::Z_PIXMAP.into(),
                segment.id,
                0,
            )?
            .reply()
            .is_ok_and(|r| r.depth == self.depth && r.size as usize == len)
        } else {
            false
        };
        let data = if shm_ok {
            &self.segment.as_ref().unwrap().bytes()[..len]
        } else {
            if self.segment.is_some() {
                isocast_diagnostics::diagnostic!("warn"; "X11 MIT-SHM GetImage failed or returned a mismatched image; switching capture to GetImage");
            }
            self.segment = None;
            self.shm_enabled = false;
            reply = c
                .get_image(
                    ImageFormat::Z_PIXMAP,
                    share.drawable,
                    x,
                    y,
                    width,
                    height,
                    u32::MAX,
                )?
                .reply()?;
            ensure!(
                reply.depth == self.depth && reply.data.len() >= len,
                "short or mismatched X11 image"
            );
            &reply.data[..len]
        };
        let (dw, dh) = self.pool.size();
        let (sw, sh) = (usize::from(width), usize::from(height));
        let (dw, dh) = (dw as usize, dh as usize);
        let (w, h) = if dw * sh <= dh * sw {
            (dw, (sh * dw / sw).max(1))
        } else {
            ((sw * dh / sh).max(1), dh)
        };
        let little = c.setup().image_byte_order == ImageOrder::LSB_FIRST;
        let masks = [
            share.visual.blue_mask,
            share.visual.green_mask,
            share.visual.red_mask,
        ];
        let native = little && self.bytes == 4 && masks == [255, 65280, 16711680];
        self.pool.draw(&mut |out, _| {
            if w != dw || h != dh {
                out.as_chunks_mut::<4>().0.fill([0, 0, 0, 255]);
            }
            for dy in 0..h {
                let sy = dy * sh / h;
                let row = &data[sy * stride..][..sw * self.bytes];
                let at = ((dy + (dh - h) / 2) * dw + (dw - w) / 2) * 4;
                let dest = &mut out[at..][..w * 4];
                if native && w == sw {
                    dest.copy_from_slice(row);
                    for px in dest.as_chunks_mut::<4>().0 {
                        px[3] = 255;
                    }
                } else {
                    for (dx, pixel) in dest.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                        let at = (dx * sw / w) * self.bytes;
                        let input = &row[at..at + self.bytes];
                        let mut raw = 0u32;
                        for (i, &b) in input.iter().enumerate() {
                            raw |=
                                u32::from(b) << (8 * if little { i } else { self.bytes - 1 - i });
                        }
                        for (channel, mask) in pixel.iter_mut().zip(masks) {
                            let shift = mask.trailing_zeros();
                            *channel = (((raw & mask) >> shift) * 255 / (mask >> shift)) as u8;
                        }
                        pixel[3] = 255;
                    }
                }
                if let Some(image) = &cursor {
                    blend_row(dest, sy, (sw, w), share.origin, image);
                }
            }
        })
    }
}

fn blend_row(
    row: &mut [u8],
    sy: usize,
    (sw, w): (usize, usize),
    origin: (i16, i16),
    c: &GetCursorImageReply,
) {
    let left = i32::from(c.x) - i32::from(c.xhot) - i32::from(origin.0);
    let cy = sy as i32 - (i32::from(c.y) - i32::from(c.yhot) - i32::from(origin.1));
    if !(0..i32::from(c.height)).contains(&cy) {
        return;
    }
    let start = (left.max(0) as usize * w).div_ceil(sw).min(w);
    let end = ((left + i32::from(c.width)).max(0) as usize * w)
        .div_ceil(sw)
        .min(w);
    for dx in start..end {
        let cx = (dx * sw / w) as i32 - left;
        let argb = c.cursor_image[cy as usize * usize::from(c.width) + cx as usize];
        for (i, ch) in row[dx * 4..dx * 4 + 3].iter_mut().enumerate() {
            *ch = (((argb >> (8 * i)) & 255) + (u32::from(*ch) * (255 - (argb >> 24)) + 127) / 255)
                .min(255) as u8;
        }
    }
}

static ACTIVE: Mutex<Vec<String>> = Mutex::new(Vec::new());
pub(super) fn active_reason() -> Option<String> {
    ACTIVE.lock().unwrap().last().cloned()
}

pub struct Capture {
    cancel: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Capture {
    pub(super) fn paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(t) = self.worker.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

pub(super) fn start(
    share: Share,
    fps: u32,
    sink: VideoSink<Frame>,
) -> Result<(Capture, CaptureFormat)> {
    ensure!(
        (1..=240).contains(&fps),
        "X11 capture fps: expected 1..=240"
    );
    let (width, height) = share.size();
    let format = CaptureFormat { width, height, fps };
    let reason = share.reason.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    let stop = cancel.clone();
    let paused = Arc::new(AtomicBool::new(false));
    let hidden = paused.clone();
    let (ready, rx) = std::sync::mpsc::sync_channel(1);
    let worker = thread::Builder::new().name("x11-capture".into()).spawn(move || {
        let mut started = false;
        let mut drops = 0u64;
        let result = (|| -> Result<()> {
            let mut reader = Reader::new(share)?;
            let period = Duration::from_secs_f64(1.0/f64::from(fps));
            let mut next = Instant::now();
            while !stop.load(Ordering::Acquire) {
                let timestamp = crate::system::now();
                let mut frame = reader.frame()?;
                hidden.store(!reader.share.mapped, Ordering::Relaxed);
                if !started && !reader.share.mapped {
                    // Hidden between pick and first capture: seed the slot
                    // with opaque black once. Otherwise leave its frame alone.
                    frame = reader.pool.draw(&mut |p,_|p.as_chunks_mut::<4>().0.fill([0,0,0,255]))?;
                }
                if let Some(frame) = frame {
                    if !started { ready.send(Ok(()))?; ACTIVE.lock().unwrap().push(reason.clone()); started = true; }
                    (sink.frame)(frame,timestamp);
                } else if reader.share.mapped { drops += 1; }
                next += period;
                let now = Instant::now();
                if now >= next {
                    let missed = 1+(now.duration_since(next).as_nanos()/period.as_nanos()) as u64;
                    drops += missed; next += period*missed.min(u64::from(u32::MAX)) as u32;
                }
                thread::park_timeout(next.saturating_duration_since(now));
            }
            Ok(())
        })();
        if started { let mut active=ACTIVE.lock().unwrap(); if let Some(i)=active.iter().position(|r|r==&reason) { active.swap_remove(i); } }
        if drops != 0 { isocast_diagnostics::diagnostic!("warn"; "X11 capture missed {drops} source frames"); }
        if let Err(e) = result { if !started { let _=ready.send(Err(e)); } else if !stop.load(Ordering::Acquire) { (sink.stopped)(format!("X11 capture: {e:#}")); } }
    })?;
    let capture = Capture {
        cancel,
        paused,
        worker: Some(worker),
    };
    rx.recv_timeout(Duration::from_secs(15))
        .context("waiting for first X11 frame")??;
    Ok((capture, format))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Run only on the fixture's private X server. Check this process's SHM
    // allocations, not unrelated agents' segments in the same IPC namespace.
    fn segments() -> Vec<String> {
        let pid = std::process::id().to_string();
        std::fs::read_to_string("/proc/sysvipc/shm")
            .unwrap()
            .lines()
            .skip(1)
            .filter(|line| line.split_whitespace().nth(4) == Some(&pid))
            .map(str::to_owned)
            .collect()
    }

    #[test]
    #[ignore = "needs a private 24-bit Xvfb; scripts/linux-x11-capture-check.sh"]
    fn x11_pixels_and_shm_lifetime() -> Result<()> {
        let expected_shm = std::env::var("ISOCAST_TEST_X11_SHM")? == "yes";
        ensure!(
            crate::x11_session::x11_session(),
            "dead WAYLAND_DISPLAY must permit X11"
        );
        assert!(segments().is_empty());
        let (c, n) = x11rb::connect(None)?;
        let root = c.setup().roots[n].root;
        let make = |color| -> Result<u32> {
            let w = c.generate_id()?;
            c.create_window(
                0,
                w,
                root,
                5,
                7,
                81,
                49,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new().background_pixel(color),
            )?
            .check()?;
            c.map_window(w)?.check()?;
            Ok(w)
        };
        let green = make(0x00ee11)?;
        let open = |target, cursor| -> Result<Reader> {
            let (c, n) = x11rb::connect(None)?;
            Reader::new(Share::open(c, n, target, cursor, String::new())?)
        };
        let mut window = open(Target::Window(green), false)?;
        let marker = c.generate_id()?;
        c.create_gc(marker, green, &CreateGCAux::new().foreground(0xcc2211))?
            .check()?;
        c.poly_fill_rectangle(
            green,
            marker,
            &[Rectangle {
                x: 79,
                y: 47,
                width: 1,
                height: 1,
            }],
        )?
        .check()?;
        let red = make(0xed1234)?;
        use x11rb::wrapper::ConnectionExt as _;
        for id in [red, green] {
            c.change_property8(
                PropMode::REPLACE,
                id,
                AtomEnum::WM_CLASS,
                AtomEnum::STRING,
                b"fixture\0Pixel test\0",
            )?
            .check()?;
            c.change_property8(
                PropMode::REPLACE,
                id,
                AtomEnum::WM_NAME,
                AtomEnum::STRING,
                b"View",
            )?
            .check()?;
        }
        let labels = || -> Result<Vec<String>> {
            Ok(candidates(&c, root, true)?
                .into_iter()
                .map(|(_, s)| s)
                .collect())
        };
        assert!(labels()?.contains(&format!("Pixel test — View (0x{green:x})")));
        let title = format!("{}🌿", "草".repeat(127)); // wire read ends three bytes into the emoji
        c.change_property8(
            PropMode::REPLACE,
            red,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            title.as_bytes(),
        )?
        .check()?;
        let unique = labels()?;
        assert!(unique.contains(&"Pixel test — View".to_owned()));
        assert!(unique.contains(&format!("Pixel test — {}…", "草".repeat(96))));
        let monitor = monitors(&c, root)?[0].name;
        let mut screen = open(Target::Monitor(monitor), false)?;
        let pixel = |reader: &mut Reader, x: usize, y: usize| -> Result<[u8; 4]> {
            let f = reader.frame()?.unwrap();
            Ok(f.read_bgra()?[(y * f.width() as usize + x) * 4..][..4].try_into()?)
        };
        assert_eq!(
            pixel(&mut screen, 15, 20)?,
            [0x34, 0x12, 0xed, 255],
            "red occluder on monitor"
        );
        assert_eq!(
            pixel(&mut window, 10, 10)?,
            [0x11, 0xee, 0, 255],
            "isolated green window"
        );
        assert_eq!(screen.segment.is_some(), expected_shm);
        assert_eq!(window.pool.size(), (80, 48));
        assert_eq!(
            pixel(&mut window, 79, 47)?,
            [0x11, 0x22, 0xcc, 255],
            "last even pixel must be copied without resampling"
        );
        assert_eq!(pixel(&mut window, 78, 47)?, [0x11, 0xee, 0, 255]);
        drop(screen);
        // Give the root a known all-white cursor, hotspot 2,3.
        let bitmap = c.generate_id()?;
        c.create_pixmap(1, bitmap, root, 8, 8)?.check()?;
        let gc = c.generate_id()?;
        c.create_gc(gc, bitmap, &CreateGCAux::new().foreground(1))?
            .check()?;
        c.poly_fill_rectangle(
            bitmap,
            gc,
            &[Rectangle {
                x: 0,
                y: 0,
                width: 8,
                height: 8,
            }],
        )?
        .check()?;
        let cursor = c.generate_id()?;
        c.create_cursor(cursor, bitmap, bitmap, 65535, 65535, 65535, 0, 0, 0, 2, 3)?
            .check()?;
        c.change_window_attributes(root, &ChangeWindowAttributesAux::new().cursor(cursor))?
            .check()?;
        c.warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, 15, 20)?
            .check()?;
        let mut screen = open(Target::Monitor(monitor), true)?;
        assert_eq!(pixel(&mut screen, 13, 17)?, [255; 4]);
        assert_eq!(pixel(&mut screen, 12, 17)?, [0x34, 0x12, 0xed, 255]);
        drop(screen);
        c.configure_window(green, &ConfigureWindowAux::new().width(160).height(96))?
            .check()?;
        c.change_window_attributes(
            green,
            &ChangeWindowAttributesAux::new().background_pixel(0x2345ef),
        )?
        .check()?;
        c.clear_area(false, green, 0, 0, 0, 0)?.check()?;
        // Opposite corner is newly allocated, not part of the old pixmap.
        assert_eq!(pixel(&mut window, 79, 47)?, [0xef, 0x45, 0x23, 255]);
        for _ in 0..2000 {
            assert_eq!(pixel(&mut window, 20, 20)?, [0xef, 0x45, 0x23, 255]);
        }
        assert_eq!(window.segment.is_some(), expected_shm);
        let live = segments();
        assert_eq!(live.len(), usize::from(expected_shm));
        if let Some(s) = live.first() {
            assert_ne!(
                u32::from_str_radix(s.split_whitespace().nth(2).unwrap(), 8)? & 0o1000,
                0,
                "segment must already be marked for deletion"
            );
        }
        c.destroy_window(red)?.check()?;
        c.destroy_window(green)?.check()?;
        assert!(window.frame().is_err());
        drop(window);
        assert!(segments().is_empty(), "leaked SysV segments");
        println!(
            "X11 screen / occluded + resized window / cursor pixels OK, 2000 frames, MIT-SHM={expected_shm}, 0 segments after close"
        );
        Ok(())
    }
}
