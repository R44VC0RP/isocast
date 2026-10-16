//! A QuickTime / MP4 file that stays readable while it is being written.
//!
//! The file grows as movie fragments (`moof` + `mdat`, ISO/IEC 14496-12
//! §8.8; the QuickTime File Format allows the same boxes), after a
//! fragmented `moov` whose `mvex` says so. Every fragment that reaches the
//! disk is playable by itself: a crash loses only what came after the last
//! one. When the file is finished, the complete sample tables are appended
//! as an ordinary `moov`, and one 16-byte header patch turns everything
//! between the `ftyp` and that `moov` into a single `mdat`, so the result
//! is the plain movie editors expect (the old fragment indexes are just
//! unused bytes inside it, which is also how AVAssetWriter finishes its
//! fragmented movies). A file interrupted before that patch is recognised
//! by the placeholder after its `ftyp` and finished from its fragments at
//! the next launch (`recover`).

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use isocast_protocol::Container;

/// Samples the encoder's priming delay hides at the start of an AAC
/// track (Apple's AAC encoder: 2112 at any rate), expressed in the audio
/// timescale. The edit list of a finished file skips them; while the file
/// is fragmented, the video is delayed by the same amount instead, as
/// AVAssetWriter does for its fragmented MP4 output.
pub const AAC_PRIMING: u64 = 2112;
/// The movie timescale: the audio rate, so audio edits are exact and the
/// frame grids of the usual rates (60, 30, 25, 24 fps) divide it.
pub const MOVIE_TIMESCALE: u32 = 48_000;
/// Where the `mdat` header is written at the end: right after `ftyp`.
const PLACEHOLDER: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video { width: u16, height: u16 },
    Audio { channels: u16, sample_rate: u32 },
}

/// What the `moov` needs to know about a track, besides its samples.
#[derive(Debug, Clone)]
pub struct Track {
    pub kind: TrackKind,
    pub timescale: u32,
    /// The complete sample entry box (`hvc1`, `mp4a`, ...), as stored in
    /// `stsd`.
    pub sample_entry: Vec<u8>,
    /// Media time the edit list of the finished file skips (AAC priming).
    pub skip: u64,
}

/// One sample: where it is in the file, how big, how long, and whether a
/// decoder can start at it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub offset: u64,
    pub size: u32,
    /// Decode time in the track's timescale.
    pub time: u64,
    pub duration: u32,
    pub sync: bool,
}

/// A track's samples for one fragment: their descriptions and their
/// bytes, in order.
pub struct FragmentTrack<'a> {
    pub samples: &'a [(u32, u64, u32, bool)],
    pub data: &'a [u8],
}

/// The file being written.
pub struct Writer {
    file: File,
    container: Container,
    tracks: Vec<Track>,
    /// Every sample written so far, per track, for the final `moov`.
    samples: Vec<Vec<Sample>>,
    len: u64,
    sequence: u32,
    moov_written: bool,
    /// Creation and modification time of the movie and its tracks
    /// ([`movie_time`]).
    created: u64,
}

impl Writer {
    /// A new file at `path`, dated `created` (the take's start; it goes in
    /// the movie's and tracks' creation time, and recovery keeps it).
    pub fn create(path: &Path, container: Container, created: SystemTime) -> Result<Self> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("could not create {}", path.display()))?;
        // Advisory lock for as long as this writer holds the file: repair
        // (`open_for_repair`) refuses a file whose recorder is still alive,
        // wherever the file was moved meanwhile. It dies with the process.
        // Only a lock someone else holds stops the take: a file system that
        // refuses locks altogether (some SMB and NFS shares) records without
        // one: repair can't take the folder's lock there either, so it never
        // touches these files while they're written.
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => bail!(
                "{} is already being written by another recorder",
                path.display()
            ),
            Err(std::fs::TryLockError::Error(err)) => {
                isocast_diagnostics::diagnostic!("warn";
                    "isocastd: {} can't be locked ({err}); recording it without the lock",
                    path.display()
                );
            }
        }
        let mut head = ftyp(container, true);
        head.extend_from_slice(&placeholder());
        file.write_all(&head)?;
        Ok(Self {
            file,
            container,
            tracks: Vec::new(),
            samples: Vec::new(),
            len: head.len() as u64,
            sequence: 0,
            moov_written: false,
            created: movie_time(created),
        })
    }

    /// The tracks, once every format is known; before the first fragment.
    pub fn set_tracks(&mut self, tracks: Vec<Track>) {
        self.samples = tracks.iter().map(|_| Vec::new()).collect();
        self.tracks = tracks;
    }

    pub fn has_tracks(&self) -> bool {
        !self.tracks.is_empty()
    }

    /// Write one fragment with these tracks' new samples (`tracks[i]` is
    /// track `i`; empty tracks are left out of the `moof`). Video decode
    /// times are shifted by `video_shift` (its timescale) in the fragment
    /// index only.
    pub fn write_fragment(&mut self, tracks: &[FragmentTrack<'_>], video_shift: u64) -> Result<()> {
        if !self.moov_written {
            let moov = moov(self.container, &self.tracks, None, 0, self.created);
            self.file.write_all(&moov)?;
            self.len += moov.len() as u64;
            self.moov_written = true;
        }
        if tracks.iter().all(|t| t.samples.is_empty()) {
            return Ok(());
        }
        self.sequence += 1;
        let moof_offset = self.len;
        let data_len: usize = tracks.iter().map(|t| t.data.len()).sum();
        // Build the moof twice: once to learn its size (the data offsets
        // are relative to its start), once for real.
        let sizes: Vec<u64> = tracks.iter().map(|t| t.data.len() as u64).collect();
        let draft = moof(self.sequence, &self.tracks, tracks, &sizes, 0, video_shift);
        let moof = moof(
            self.sequence,
            &self.tracks,
            tracks,
            &sizes,
            draft.len() as u64 + 8,
            video_shift,
        );
        debug_assert_eq!(draft.len(), moof.len());
        let mut head = moof;
        head.extend_from_slice(&box_header(b"mdat", data_len as u64 + 8));
        self.file.write_all(&head)?;
        let mut offset = moof_offset + head.len() as u64;
        for (index, track) in tracks.iter().enumerate() {
            if track.data.is_empty() {
                continue;
            }
            self.file.write_all(track.data)?;
            let mut at = offset;
            for &(size, time, duration, sync) in track.samples {
                self.samples[index].push(Sample {
                    offset: at,
                    size,
                    time,
                    duration,
                    sync,
                });
                at += u64::from(size);
            }
            offset += track.data.len() as u64;
        }
        self.len = offset;
        Ok(())
    }

    /// Flush the file through to the drive. Rust's `sync_data` is
    /// `fcntl(F_FULLFSYNC)` on Apple platforms (a drive-cache flush, what a
    /// power loss needs), so there is no weaker variant to choose here; it
    /// costs tens of milliseconds on some drives, which is why it runs on
    /// commit and finish and not per fragment.
    pub fn sync(&self) -> Result<()> {
        self.file.sync_data().context("fsync")
    }

    /// Append the final `moov` for a movie `duration` long (movie
    /// timescale) and make the file an ordinary movie.
    pub fn finish(mut self, duration: u64) -> Result<()> {
        if !self.moov_written {
            let moov = moov(self.container, &self.tracks, None, 0, self.created);
            self.file.write_all(&moov)?;
            self.len += moov.len() as u64;
            self.moov_written = true;
        }
        fix_durations(&mut self.samples);
        let start = ftyp(self.container, true).len() as u64;
        let moov = moov(
            self.container,
            &self.tracks,
            Some(&self.samples),
            duration,
            self.created,
        );
        finalize(&mut self.file, self.len, start, &moov)?;
        self.sync()
    }

    /// Give up on the file (its writer failed): the descriptor is closed so
    /// a repair can reopen the path.
    pub fn abandon(self) {}

    #[cfg(test)]
    fn samples(&self, track: usize) -> &[Sample] {
        &self.samples[track]
    }
}

/// Make every sample last until the next one of its track: the times are
/// what the encoder delivered (and what the fragments' `tfdt` carry), so a
/// gap where frames were dropped stretches the picture before it instead
/// of pulling everything after it earlier. The last sample keeps its own
/// duration. Stop and recovery both go through this, so they agree.
fn fix_durations(samples: &mut [Vec<Sample>]) {
    for track in samples {
        for i in 0..track.len().saturating_sub(1) {
            let next = track[i + 1].time;
            if next > track[i].time {
                track[i].duration = (next - track[i].time).min(u64::from(u32::MAX)) as u32;
            }
        }
    }
}

/// Append the final `moov` at `len` (the end of the fragments) and patch
/// the placeholder into an `mdat` spanning everything in between.
fn finalize(file: &mut File, len: u64, start: u64, moov: &[u8]) -> Result<()> {
    file.seek(SeekFrom::Start(len))?;
    file.write_all(moov)?;
    // The data region must be whole on disk before the header says it is
    // one box. The patch is one 16-byte write; should it tear (a crash in
    // the middle of it), `recover` recognises any state of those bytes.
    file.sync_data()?;
    file.write_all_at(&mdat_header(len - start), start)?;
    Ok(())
}

fn mdat_header(size: u64) -> Vec<u8> {
    let mut header = Vec::with_capacity(PLACEHOLDER);
    header.extend_from_slice(&1u32.to_be_bytes());
    header.extend_from_slice(b"mdat");
    header.extend_from_slice(&size.to_be_bytes());
    header
}

/// One track of a movie rebuilt from fragments.
#[derive(Debug, Clone, PartialEq)]
pub struct RecoveredTrack {
    pub video: bool,
    /// Samples in the movie; a track with none is left out of it.
    pub samples: usize,
    /// Seconds the track presents (its media after the priming it skips).
    pub seconds: f64,
}

/// What `recover` found and finished.
#[derive(Debug)]
pub struct Recovered {
    pub fragments: u32,
    /// The tracks, in the file's order; empty when only the header patch
    /// was missing (`rebuilt` false: the writer's own counts stand).
    pub tracks: Vec<RecoveredTrack>,
    pub rebuilt: bool,
}

impl Recovered {
    /// Pictures in the rebuilt movie.
    pub fn pictures(&self) -> Option<u64> {
        self.rebuilt.then(|| {
            self.tracks
                .iter()
                .filter(|t| t.video)
                .map(|t| t.samples as u64)
                .sum()
        })
    }

    /// The shortest kept track's seconds.
    pub fn shortest(&self) -> Option<f64> {
        self.tracks
            .iter()
            .filter(|t| t.samples > 0)
            .map(|t| t.seconds)
            .reduce(f64::min)
    }

    /// What the movie holds, track by track: "video 61 pictures (2.033 s);
    /// audio 2.001 s, 2.001 s; left out: video (no picture)".
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        let kept_video: Vec<&RecoveredTrack> = self
            .tracks
            .iter()
            .filter(|t| t.video && t.samples > 0)
            .collect();
        if !kept_video.is_empty() {
            parts.push(format!(
                "video {} ({})",
                kept_video
                    .iter()
                    .map(|t| format!("{} pictures", t.samples))
                    .collect::<Vec<_>>()
                    .join(", "),
                kept_video
                    .iter()
                    .map(|t| format!("{:.3} s", t.seconds))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let kept_audio: Vec<String> = self
            .tracks
            .iter()
            .filter(|t| !t.video && t.samples > 0)
            .map(|t| format!("{:.3} s", t.seconds))
            .collect();
        if !kept_audio.is_empty() {
            parts.push(format!("audio {}", kept_audio.join(", ")));
        }
        let mut left_out = Vec::new();
        if self.tracks.iter().any(|t| t.video && t.samples == 0) {
            left_out.push("video (no picture)".to_string());
        }
        let silent = self
            .tracks
            .iter()
            .filter(|t| !t.video && t.samples == 0)
            .count();
        if silent > 0 {
            left_out.push(format!("{silent} audio track(s) without any packet"));
        }
        if !left_out.is_empty() {
            parts.push(format!("left out: {}", left_out.join(", ")));
        }
        parts.join("; ")
    }
}

/// Open `name` inside `folder` for repair, and nothing else: the entry
/// must be a regular file reached through the directory itself (no
/// symbolic link is followed, `openat` with `O_NOFOLLOW`), with no other
/// name for its inode (`st_nlink == 1` where the volume has hard links),
/// so a staged alias can never make
/// recovery write into a movie another engine is still writing through
/// another folder. The folder itself may be a link (a recording folder on
/// an external disk); its lock lives in the real folder either way.
pub fn open_for_repair(folder: &Path, name: &OsStr) -> Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    let bytes = name.as_bytes();
    if bytes.is_empty()
        || bytes.contains(&b'/')
        || bytes.contains(&0)
        || bytes == b"."
        || bytes == b".."
    {
        bail!("{} is not a plain file name", name.to_string_lossy());
    }
    let dir = File::open(folder)
        .with_context(|| format!("could not open the folder {}", folder.display()))?;
    if !dir.metadata()?.is_dir() {
        bail!("{} is not a folder", folder.display());
    }
    let cname = std::ffi::CString::new(bytes).expect("no NUL");
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            cname.as_ptr(),
            libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ELOOP) {
            bail!(
                "{} is a symbolic link; recovery only repairs regular files in the folder it owns",
                name.to_string_lossy()
            );
        }
        return Err(err).with_context(|| format!("could not open {}", name.to_string_lossy()));
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let meta = file.metadata()?;
    if !meta.is_file() {
        bail!("{} is not a regular file", name.to_string_lossy());
    }
    // Only where the volume can make hard links: elsewhere the count is
    // how the volume reports links, not a second name (#56).
    if meta.nlink() > 1 && crate::hard_links_possible_fd(&file) {
        bail!(
            "{} has {} names (hard links); recovery only repairs files no other folder can reach",
            name.to_string_lossy(),
            meta.nlink()
        );
    }
    // A recorder holds its movie locked while it writes (`Writer::create`):
    // a file still being written, even one renamed into this folder, is
    // left alone. The lock stays with this handle for the repair. A file
    // system that refuses locks (the caller already holds the folder's
    // lock, exclusively) is repaired without one, as it was recorded.
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => bail!(
            "{} is still being written by a running recorder; it is left alone",
            name.to_string_lossy()
        ),
        Err(std::fs::TryLockError::Error(err)) => {
            isocast_diagnostics::diagnostic!("warn";
                "isocastd: {} can't be locked ({err}); repairing it without the lock",
                name.to_string_lossy()
            );
        }
    }
    Ok(file)
}

/// Finish an interrupted file from its fragments. Returns `None` when the
/// file isn't one of ours or is already finished.
///
/// The 16 bytes after `ftyp` are the placeholder (`free`) while recording
/// and the `mdat` header once finished; a crash in the middle of that
/// patch can leave any of the 17 mixtures (0 to 16 bytes of the new
/// header). So those bytes are never trusted, and never decide anything:
/// the file is one of ours if the fragmented `moov` (with `mvex`) sits
/// right behind them, where the first fragment put it; it is finished if
/// a complete final `moov` follows the fragments *and* the 16 bytes are
/// exactly the `mdat` header that ends on it; anything else is scanned and
/// patched (again). The caller opens the file (`open_for_repair`), so only
/// a regular file inside the folder it owns is ever modified.
pub fn recover(file: &mut File) -> Result<Option<Recovered>> {
    let len = file.metadata()?.len();
    if len < 8 {
        return Ok(None);
    }
    // Ours, byte for byte: the `ftyp` this writer puts first (with its
    // own brand) and nothing else; its measured size is where the
    // placeholder starts.
    let (ftyp_size, kind, _) = read_header(file, 0, len)?;
    if &kind != b"ftyp" || ftyp_size < 16 || ftyp_size + PLACEHOLDER as u64 > len {
        return Ok(None);
    }
    let head = read_range(file, 0, ftyp_size)?;
    let Some(container) = [Container::Mov, Container::Mp4]
        .into_iter()
        .find(|container| ftyp(*container, true) == head)
    else {
        return Ok(None);
    };
    let probe = ftyp_size + PLACEHOLDER as u64;
    let mut placeholder = [0u8; PLACEHOLDER];
    file.read_exact_at(&mut placeholder, ftyp_size)?;
    if len == probe && placeholder == placeholder_bytes()[..] {
        bail!(
            "no picture reached the file before the recording was interrupted (a take \
             shorter than two frames is not kept)"
        );
    }
    if probe + 8 > len {
        return Ok(None);
    }
    // Ours: the fragmented moov right behind the placeholder (or, for a
    // file killed before its first fragment, the pristine placeholder with
    // nothing behind it).
    let (first_size, first_kind, first_header) = read_header(file, probe, len)?;
    let ours = &first_kind == b"moov"
        && first_size >= first_header
        && probe + first_size <= len
        && find_box(
            &read_range(file, probe + first_header, first_size - first_header)?,
            b"mvex",
        )
        .is_some();
    if !ours {
        if placeholder == placeholder_bytes()[..] && &first_kind != b"moov" {
            bail!(
                "no picture reached the file before the recording was interrupted (a take \
                 shorter than two frames is not kept)"
            );
        }
        return Ok(None);
    }
    // A complete final moov where the header's length field points (the
    // field may have landed before the type did, or after; the scan below
    // may not reach it past a fragment the crash cut short).
    let pointed = ftyp_size.checked_add(u64::from_be_bytes(placeholder[8..].try_into().unwrap()));
    let final_moov = |at: u64| -> Result<bool> {
        if at < probe + 8 || at + 8 > len {
            return Ok(false);
        }
        let (size, kind, header) = read_header(file, at, len)?;
        Ok(&kind == b"moov"
            && size >= header
            && at + size <= len
            && find_box(&read_range(file, at + header, size - header)?, b"mvex").is_none())
    };
    let mut moov_at = match pointed {
        Some(at) if final_moov(at)? => Some(at),
        _ => None,
    };
    // The boxes behind the placeholder, up to the first one cut short.
    let mut top = Vec::new();
    let mut pos = probe;
    while pos + 8 <= len {
        let (size, kind, header) = read_header(file, pos, len)?;
        if size < header || pos + size > len {
            break;
        }
        top.push((kind, pos, header, size));
        pos += size;
    }
    // A crash during `finish`: the final moov is complete, only the patch
    // is missing (or torn). The same patch repeated is a no-op.
    let mut fragmented = None;
    for (kind, pos, header, size) in &top {
        if kind == b"moov" {
            let body = read_range(file, pos + header, size - header)?;
            if find_box(&body, b"mvex").is_some() {
                fragmented = Some((*pos, body));
            } else if moov_at.is_none() {
                moov_at = Some(*pos);
            }
        }
    }
    if let Some(at) = moov_at {
        let header = mdat_header(at - ftyp_size);
        if placeholder[..] == header[..] {
            return Ok(None);
        }
        file.write_all_at(&header, ftyp_size)?;
        file.sync_data()?;
        return Ok(Some(Recovered {
            fragments: 0,
            tracks: Vec::new(),
            rebuilt: false,
        }));
    }
    let Some((_, moov_body)) = fragmented else {
        bail!(
            "no picture reached the file before the recording was interrupted (a take \
             shorter than two frames is not kept)"
        );
    };
    let tracks = parse_tracks(&moov_body)?;
    let created = parse_created(&moov_body);
    if tracks.is_empty() {
        bail!("the fragment index describes no tracks");
    }
    let has_audio = tracks
        .iter()
        .any(|t| matches!(t.kind, TrackKind::Audio { .. }));
    let mut samples: Vec<Vec<Sample>> = tracks.iter().map(|_| Vec::new()).collect();
    let mut fragments = 0;
    let mut end = ftyp_size + PLACEHOLDER as u64;
    let mut index = 0;
    while index < top.len() {
        let (kind, pos, header, size) = top[index];
        index += 1;
        if &kind != b"moof" {
            end = end.max(pos + size);
            continue;
        }
        let Some(&(mdat_kind, mdat_pos, mdat_header, mdat_size)) = top.get(index) else {
            break;
        };
        if &mdat_kind != b"mdat" {
            break;
        }
        let body = read_range(file, pos + header, size - header)?;
        let data = mdat_pos + mdat_header..mdat_pos + mdat_size;
        let mut whole = true;
        for (track, sample) in parse_moof(&body, pos, &tracks)? {
            let shift = match tracks[track].kind {
                TrackKind::Video { .. } if has_audio => video_shift(tracks[track].timescale),
                _ => 0,
            };
            if sample.offset < data.start || sample.offset + u64::from(sample.size) > data.end {
                whole = false;
                break;
            }
            samples[track].push(Sample {
                time: sample.time.saturating_sub(shift),
                ..sample
            });
        }
        if !whole {
            break;
        }
        fragments += 1;
        end = mdat_pos + mdat_size;
        index += 1;
    }
    if samples.iter().all(Vec::is_empty) {
        bail!("no complete fragment reached the file before the recording was interrupted");
    }
    fix_durations(&mut samples);
    // Each kept track presents all the media it has (after the priming
    // an audio track skips); the movie is as long as the longest of them,
    // so sound recorded after the last saved picture, or with no picture
    // at all, stays presented.
    let report: Vec<RecoveredTrack> = tracks
        .iter()
        .zip(&samples)
        .map(|(t, s)| {
            let media = s
                .last()
                .map_or(0, |last| last.time + u64::from(last.duration));
            RecoveredTrack {
                video: matches!(t.kind, TrackKind::Video { .. }),
                samples: s.len(),
                seconds: media.saturating_sub(t.skip) as f64 / f64::from(t.timescale),
            }
        })
        .collect();
    let longest = report
        .iter()
        .filter(|t| t.samples > 0)
        .map(|t| t.seconds)
        .fold(0.0f64, f64::max);
    let duration = (longest * f64::from(MOVIE_TIMESCALE)).round() as u64;
    let moov = moov(container, &tracks, Some(&samples), duration, created);
    finalize(file, len.max(end), ftyp_size, &moov)?;
    file.sync_data()?;
    Ok(Some(Recovered {
        fragments,
        tracks: report,
        rebuilt: true,
    }))
}

/// The video delay that stands in for the AAC edit list while the file is
/// fragmented, in a video timescale.
pub fn video_shift(timescale: u32) -> u64 {
    AAC_PRIMING * u64::from(timescale) / u64::from(MOVIE_TIMESCALE)
}

fn read_header(file: &File, pos: u64, len: u64) -> Result<(u64, [u8; 4], u64)> {
    let mut head = [0u8; 8];
    file.read_exact_at(&mut head, pos)?;
    let mut size = u64::from(u32::from_be_bytes([head[0], head[1], head[2], head[3]]));
    let kind = [head[4], head[5], head[6], head[7]];
    let mut header = 8;
    if size == 1 {
        if pos + 16 > len {
            return Ok((0, kind, 16));
        }
        let mut large = [0u8; 8];
        file.read_exact_at(&mut large, pos + 8)?;
        size = u64::from_be_bytes(large);
        header = 16;
    } else if size == 0 {
        size = len - pos;
    }
    Ok((size, kind, header))
}

fn read_range(file: &File, pos: u64, len: u64) -> Result<Vec<u8>> {
    if len > (64 << 20) {
        bail!("a {len}-byte index box is not plausible");
    }
    let mut data = vec![0u8; len as usize];
    file.read_exact_at(&mut data, pos)?;
    Ok(data)
}

/// Children of a box body: (kind, body range).
fn children(body: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8], usize)> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        if pos + 8 > body.len() {
            return None;
        }
        let size = u32::from_be_bytes(body[pos..pos + 4].try_into().unwrap()) as usize;
        let kind: [u8; 4] = body[pos + 4..pos + 8].try_into().unwrap();
        let (size, header) = match size {
            1 => {
                if pos + 16 > body.len() {
                    return None;
                }
                (
                    u64::from_be_bytes(body[pos + 8..pos + 16].try_into().unwrap()) as usize,
                    16,
                )
            }
            0 => (body.len() - pos, 8),
            n => (n, 8),
        };
        if size < header || pos + size > body.len() {
            return None;
        }
        let start = pos;
        pos += size;
        Some((kind, &body[start + header..start + size], start))
    })
}

fn find_box<'a>(body: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    children(body).find(|(k, ..)| k == kind).map(|(_, b, _)| b)
}

fn be32(data: &[u8], at: usize) -> Option<u32> {
    data.get(at..at + 4)
        .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
}

fn be64(data: &[u8], at: usize) -> Option<u64> {
    data.get(at..at + 8)
        .map(|b| u64::from_be_bytes(b.try_into().unwrap()))
}

fn be16(data: &[u8], at: usize) -> Option<u16> {
    data.get(at..at + 2)
        .map(|b| u16::from_be_bytes(b.try_into().unwrap()))
}

/// The creation time a `moov`'s `mvhd` carries ([`movie_time`]), 0 if none.
fn parse_created(moov: &[u8]) -> u64 {
    let Some(mvhd) = find_box(moov, b"mvhd") else {
        return 0;
    };
    if mvhd.first().copied().unwrap_or(0) == 1 {
        be64(mvhd, 4).unwrap_or(0)
    } else {
        be32(mvhd, 4).map_or(0, u64::from)
    }
}

/// The tracks of a fragmented `moov`, in track id order (ids are 1-based
/// positions, as this writer assigns them).
fn parse_tracks(moov: &[u8]) -> Result<Vec<Track>> {
    let mut tracks = Vec::new();
    for (kind, trak, _) in children(moov) {
        if &kind != b"trak" {
            continue;
        }
        let tkhd = find_box(trak, b"tkhd").context("a track without tkhd")?;
        let version = tkhd.first().copied().unwrap_or(0);
        let (id_at, size_at) = if version == 1 { (20, 88) } else { (12, 76) };
        let id = be32(tkhd, id_at).context("tkhd")? as usize;
        let mdia = find_box(trak, b"mdia").context("a track without mdia")?;
        let mdhd = find_box(mdia, b"mdhd").context("mdhd")?;
        let timescale = if mdhd.first().copied().unwrap_or(0) == 1 {
            be32(mdhd, 20)
        } else {
            be32(mdhd, 12)
        }
        .context("mdhd timescale")?;
        let hdlr = find_box(mdia, b"hdlr").context("hdlr")?;
        let handler: [u8; 4] = hdlr.get(8..12).context("hdlr type")?.try_into().unwrap();
        let minf = find_box(mdia, b"minf").context("minf")?;
        let stbl = find_box(minf, b"stbl").context("stbl")?;
        let stsd = find_box(stbl, b"stsd").context("stsd")?;
        let entry_len = be32(stsd, 8).context("stsd entry")? as usize;
        let sample_entry = stsd
            .get(8..8 + entry_len)
            .context("stsd entry is cut short")?
            .to_vec();
        let kind = match &handler {
            b"vide" => TrackKind::Video {
                width: (be32(tkhd, size_at).context("tkhd width")? >> 16) as u16,
                height: (be32(tkhd, size_at + 4).context("tkhd height")? >> 16) as u16,
            },
            b"soun" => TrackKind::Audio {
                channels: be16(&sample_entry, 24).unwrap_or(2),
                sample_rate: timescale,
            },
            other => bail!(
                "unsupported track handler {:?}",
                String::from_utf8_lossy(other)
            ),
        };
        let skip = match kind {
            TrackKind::Audio { .. } if &sample_entry[4..8] == b"mp4a" => {
                AAC_PRIMING * u64::from(timescale) / u64::from(MOVIE_TIMESCALE)
            }
            _ => 0,
        };
        tracks.push((
            id,
            Track {
                kind,
                timescale,
                sample_entry,
                skip,
            },
        ));
    }
    tracks.sort_by_key(|(id, _)| *id);
    if tracks.iter().enumerate().any(|(i, (id, _))| *id != i + 1) {
        bail!("track ids are not 1..n");
    }
    Ok(tracks.into_iter().map(|(_, t)| t).collect())
}

/// The samples a `moof` at `moof_offset` describes, as (track index, sample).
fn parse_moof(moof: &[u8], moof_offset: u64, tracks: &[Track]) -> Result<Vec<(usize, Sample)>> {
    let mut out = Vec::new();
    for (kind, traf, _) in children(moof) {
        if &kind != b"traf" {
            continue;
        }
        let tfhd = find_box(traf, b"tfhd").context("tfhd")?;
        let flags = be32(tfhd, 0).context("tfhd")? & 0xff_ffff;
        let track_id = be32(tfhd, 4).context("tfhd track")? as usize;
        let track = track_id
            .checked_sub(1)
            .filter(|i| *i < tracks.len())
            .context("tfhd names an unknown track")?;
        let mut at = 8;
        let mut base = moof_offset;
        if flags & 0x1 != 0 {
            base = be64(tfhd, at).context("tfhd base")?;
            at += 8;
        }
        if flags & 0x2 != 0 {
            at += 4;
        }
        let mut default_duration = 0;
        if flags & 0x8 != 0 {
            default_duration = be32(tfhd, at).context("tfhd duration")?;
            at += 4;
        }
        let mut default_size = 0;
        if flags & 0x10 != 0 {
            default_size = be32(tfhd, at).context("tfhd size")?;
            at += 4;
        }
        let mut default_flags = 0;
        if flags & 0x20 != 0 {
            default_flags = be32(tfhd, at).context("tfhd flags")?;
        }
        let tfdt = find_box(traf, b"tfdt").context("tfdt")?;
        let mut time = if tfdt.first().copied().unwrap_or(0) == 1 {
            be64(tfdt, 4).context("tfdt")?
        } else {
            u64::from(be32(tfdt, 4).context("tfdt")?)
        };
        for (kind, trun, _) in children(traf) {
            if &kind != b"trun" {
                continue;
            }
            let flags = be32(trun, 0).context("trun")? & 0xff_ffff;
            let count = be32(trun, 4).context("trun count")?;
            let mut at = 8;
            let mut offset = base;
            if flags & 0x1 != 0 {
                let data_offset = be32(trun, at).context("trun offset")? as i32;
                offset = base
                    .checked_add_signed(i64::from(data_offset))
                    .context("offset")?;
                at += 4;
            }
            let mut first_flags = None;
            if flags & 0x4 != 0 {
                first_flags = be32(trun, at);
                at += 4;
            }
            for i in 0..count {
                let mut duration = default_duration;
                let mut size = default_size;
                let mut sample_flags = default_flags;
                if flags & 0x100 != 0 {
                    duration = be32(trun, at).context("trun duration")?;
                    at += 4;
                }
                if flags & 0x200 != 0 {
                    size = be32(trun, at).context("trun size")?;
                    at += 4;
                }
                if flags & 0x400 != 0 {
                    sample_flags = be32(trun, at).context("trun flags")?;
                    at += 4;
                }
                if flags & 0x800 != 0 {
                    at += 4;
                }
                if i == 0
                    && let Some(first) = first_flags
                {
                    sample_flags = first;
                }
                out.push((
                    track,
                    Sample {
                        offset,
                        size,
                        time,
                        duration,
                        sync: sample_flags & 0x1_0000 == 0,
                    },
                ));
                offset += u64::from(size);
                time += u64::from(duration);
            }
        }
    }
    Ok(out)
}

// Box builders.

fn box_header(kind: &[u8; 4], size: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    if size > u32::MAX as u64 {
        out.extend_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(&size.to_be_bytes());
    } else {
        out.extend_from_slice(&(size as u32).to_be_bytes());
        out.extend_from_slice(kind);
    }
    out
}

fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = box_header(kind, body.len() as u64 + 8);
    out.extend_from_slice(body);
    out
}

fn full(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut inner = Vec::with_capacity(body.len() + 4);
    inner.extend_from_slice(&((u32::from(version) << 24) | (flags & 0xff_ffff)).to_be_bytes());
    inner.extend_from_slice(body);
    boxed(kind, &inner)
}

fn placeholder() -> Vec<u8> {
    placeholder_bytes().to_vec()
}

fn placeholder_bytes() -> [u8; PLACEHOLDER] {
    let mut out = [0u8; PLACEHOLDER];
    out[..8].copy_from_slice(&box_header(b"free", PLACEHOLDER as u64));
    out
}

/// The compatible brand that marks a file as written by this writer: the
/// `ftyp` is written once at creation and never changes, so recovery can
/// require it byte for byte (an AVAssetWriter movie or any other file
/// with a similar layout is left alone). Readers ignore brands they don't
/// know.
const ISOCAST_BRAND: &[u8; 4] = b"isoc";

fn ftyp(container: Container, fragments: bool) -> Vec<u8> {
    let mut body = Vec::new();
    match container {
        Container::Mov => {
            body.extend_from_slice(b"qt  ");
            body.extend_from_slice(&0u32.to_be_bytes());
            body.extend_from_slice(b"qt  ");
        }
        Container::Mp4 => {
            body.extend_from_slice(b"mp42");
            body.extend_from_slice(&1u32.to_be_bytes());
            body.extend_from_slice(b"mp42");
            body.extend_from_slice(b"mp41");
            body.extend_from_slice(b"isom");
            if fragments {
                body.extend_from_slice(b"iso6");
            }
        }
    }
    body.extend_from_slice(ISOCAST_BRAND);
    boxed(b"ftyp", &body)
}

/// A `moov`: fragmented (no samples, with `mvex`) or final. A final
/// `moov` leaves out a track that never got a sample (an edit over no
/// media made AVAssetReader fail the track in review).
fn moov(
    container: Container,
    tracks: &[Track],
    samples: Option<&[Vec<Sample>]>,
    duration: u64,
    created: u64,
) -> Vec<u8> {
    let mut body = mvhd(duration, tracks.len() as u32 + 1, created);
    for (index, track) in tracks.iter().enumerate() {
        let samples = samples.map(|s| s[index].as_slice());
        if samples.is_some_and(<[Sample]>::is_empty) {
            continue;
        }
        body.extend(trak(
            container,
            index as u32 + 1,
            track,
            samples,
            duration,
            created,
        ));
    }
    if samples.is_none() {
        let mut mvex = Vec::new();
        for index in 0..tracks.len() {
            let mut trex = Vec::new();
            trex.extend_from_slice(&(index as u32 + 1).to_be_bytes());
            trex.extend_from_slice(&1u32.to_be_bytes());
            trex.extend_from_slice(&[0u8; 12]);
            mvex.extend(full(b"trex", 0, 0, &trex));
        }
        body.extend(boxed(b"mvex", &mvex));
    }
    boxed(b"moov", &body)
}

const MATRIX: [u32; 9] = [0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x4000_0000];

/// Seconds from 1904-01-01 00:00 UTC (the QuickTime and ISO epoch) to
/// `time`, as `mvhd`, `tkhd` and `mdhd` date a movie.
pub fn movie_time(time: SystemTime) -> u64 {
    const UNIX_FROM_1904: u64 = 2_082_844_800;
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() + UNIX_FROM_1904)
}

/// Whether a header needs version 1 (64-bit times and duration).
fn wide(duration: u64, created: u64) -> bool {
    duration > u64::from(u32::MAX) || created > u64::from(u32::MAX)
}

/// The creation and modification times of a header (both `created`).
fn times(body: &mut Vec<u8>, v1: bool, created: u64) {
    for _ in 0..2 {
        if v1 {
            body.extend_from_slice(&created.to_be_bytes());
        } else {
            body.extend_from_slice(&(created as u32).to_be_bytes());
        }
    }
}

fn mvhd(duration: u64, next_track: u32, created: u64) -> Vec<u8> {
    let mut body = Vec::new();
    let v1 = wide(duration, created);
    times(&mut body, v1, created);
    if v1 {
        body.extend_from_slice(&MOVIE_TIMESCALE.to_be_bytes());
        body.extend_from_slice(&duration.to_be_bytes());
    } else {
        body.extend_from_slice(&MOVIE_TIMESCALE.to_be_bytes());
        body.extend_from_slice(&(duration as u32).to_be_bytes());
    }
    body.extend_from_slice(&0x1_0000u32.to_be_bytes());
    body.extend_from_slice(&0x100u16.to_be_bytes());
    body.extend_from_slice(&[0u8; 10]);
    for m in MATRIX {
        body.extend_from_slice(&m.to_be_bytes());
    }
    body.extend_from_slice(&[0u8; 24]);
    body.extend_from_slice(&next_track.to_be_bytes());
    full(b"mvhd", u8::from(v1), 0, &body)
}

fn trak(
    container: Container,
    id: u32,
    track: &Track,
    samples: Option<&[Sample]>,
    duration: u64,
    created: u64,
) -> Vec<u8> {
    let media_duration = samples.map_or(0, |s| {
        s.last()
            .map_or(0, |last| last.time + u64::from(last.duration))
    });
    // The edit presents the movie's duration, or as much of this track as
    // its media actually covers after the priming it skips (a track that
    // ended early, in a recovered file, must not claim sound it lacks).
    let available = media_duration.saturating_sub(track.skip) * u64::from(MOVIE_TIMESCALE)
        / u64::from(track.timescale);
    let presented = if samples.is_some() {
        duration.min(available)
    } else {
        0
    };
    let mut body = tkhd(id, track, presented, created);
    if samples.is_some() {
        let mut elst = Vec::new();
        let v1 = presented > u64::from(u32::MAX) || track.skip > u64::from(u32::MAX);
        elst.extend_from_slice(&1u32.to_be_bytes());
        if v1 {
            elst.extend_from_slice(&presented.to_be_bytes());
            elst.extend_from_slice(&(track.skip as i64).to_be_bytes());
        } else {
            elst.extend_from_slice(&(presented as u32).to_be_bytes());
            elst.extend_from_slice(&(track.skip as u32).to_be_bytes());
        }
        elst.extend_from_slice(&0x1_0000u32.to_be_bytes());
        body.extend(boxed(b"edts", &full(b"elst", u8::from(v1), 0, &elst)));
    }
    let mut mdia = mdhd(track.timescale, media_duration, created);
    let (handler, name): (&[u8; 4], &str) = match track.kind {
        TrackKind::Video { .. } => (b"vide", "VideoHandler"),
        TrackKind::Audio { .. } => (b"soun", "SoundHandler"),
    };
    mdia.extend(hdlr(container, handler, name));
    let mut minf = match track.kind {
        TrackKind::Video { .. } => full(b"vmhd", 0, 1, &[0u8; 8]),
        TrackKind::Audio { .. } => full(b"smhd", 0, 0, &[0u8; 4]),
    };
    if container == Container::Mov {
        minf.extend(hdlr(container, b"alis", "DataHandler"));
    }
    let dref_entry = match container {
        Container::Mov => full(b"alis", 0, 1, &[]),
        Container::Mp4 => full(b"url ", 0, 1, &[]),
    };
    let mut dref = 1u32.to_be_bytes().to_vec();
    dref.extend(dref_entry);
    minf.extend(boxed(b"dinf", &full(b"dref", 0, 0, &dref)));
    minf.extend(stbl(track, samples));
    mdia.extend(boxed(b"minf", &minf));
    body.extend(boxed(b"mdia", &mdia));
    boxed(b"trak", &body)
}

fn tkhd(id: u32, track: &Track, duration: u64, created: u64) -> Vec<u8> {
    let mut body = Vec::new();
    let v1 = wide(duration, created);
    times(&mut body, v1, created);
    if v1 {
        body.extend_from_slice(&id.to_be_bytes());
        body.extend_from_slice(&[0u8; 4]);
        body.extend_from_slice(&duration.to_be_bytes());
    } else {
        body.extend_from_slice(&id.to_be_bytes());
        body.extend_from_slice(&[0u8; 4]);
        body.extend_from_slice(&(duration as u32).to_be_bytes());
    }
    body.extend_from_slice(&[0u8; 8]);
    body.extend_from_slice(&0u16.to_be_bytes());
    body.extend_from_slice(&0u16.to_be_bytes());
    let volume: u16 = match track.kind {
        TrackKind::Video { .. } => 0,
        TrackKind::Audio { .. } => 0x100,
    };
    body.extend_from_slice(&volume.to_be_bytes());
    body.extend_from_slice(&[0u8; 2]);
    for m in MATRIX {
        body.extend_from_slice(&m.to_be_bytes());
    }
    let (width, height) = match track.kind {
        TrackKind::Video { width, height } => (u32::from(width) << 16, u32::from(height) << 16),
        TrackKind::Audio { .. } => (0, 0),
    };
    body.extend_from_slice(&width.to_be_bytes());
    body.extend_from_slice(&height.to_be_bytes());
    full(b"tkhd", u8::from(v1), 0xf, &body)
}

fn mdhd(timescale: u32, duration: u64, created: u64) -> Vec<u8> {
    let mut body = Vec::new();
    let v1 = wide(duration, created);
    times(&mut body, v1, created);
    if v1 {
        body.extend_from_slice(&timescale.to_be_bytes());
        body.extend_from_slice(&duration.to_be_bytes());
    } else {
        body.extend_from_slice(&timescale.to_be_bytes());
        body.extend_from_slice(&(duration as u32).to_be_bytes());
    }
    // Language "und", then pre-defined.
    body.extend_from_slice(&0x55c4u16.to_be_bytes());
    body.extend_from_slice(&[0u8; 2]);
    full(b"mdhd", u8::from(v1), 0, &body)
}

fn hdlr(container: Container, handler: &[u8; 4], name: &str) -> Vec<u8> {
    let mut body = Vec::new();
    match container {
        Container::Mov => {
            body.extend_from_slice(if handler == b"alis" { b"dhlr" } else { b"mhlr" });
            body.extend_from_slice(handler);
            body.extend_from_slice(b"appl");
            body.extend_from_slice(&[0u8; 8]);
            body.push(name.len() as u8);
            body.extend_from_slice(name.as_bytes());
        }
        Container::Mp4 => {
            body.extend_from_slice(&[0u8; 4]);
            body.extend_from_slice(handler);
            body.extend_from_slice(&[0u8; 12]);
            body.extend_from_slice(name.as_bytes());
            body.push(0);
        }
    }
    full(b"hdlr", 0, 0, &body)
}

fn stbl(track: &Track, samples: Option<&[Sample]>) -> Vec<u8> {
    let mut body = Vec::new();
    let mut stsd = 1u32.to_be_bytes().to_vec();
    stsd.extend_from_slice(&track.sample_entry);
    body.extend(full(b"stsd", 0, 0, &stsd));
    let samples = samples.unwrap_or(&[]);
    // AAC: every packet is in a "roll" group with distance -1, so
    // AVFoundation decodes from the packet before the edit's start instead
    // of skipping to the first whole packet after it (what AVAssetWriter
    // writes; without it AVAssetReader began the audio 64 ms late).
    if track.skip > 0 && !samples.is_empty() {
        let mut sgpd = b"roll".to_vec();
        sgpd.extend_from_slice(&2u32.to_be_bytes());
        sgpd.extend_from_slice(&1u32.to_be_bytes());
        sgpd.extend_from_slice(&(-1i16).to_be_bytes());
        body.extend(full(b"sgpd", 1, 0, &sgpd));
        let mut sbgp = b"roll".to_vec();
        sbgp.extend_from_slice(&1u32.to_be_bytes());
        sbgp.extend_from_slice(&(samples.len() as u32).to_be_bytes());
        sbgp.extend_from_slice(&1u32.to_be_bytes());
        body.extend(full(b"sbgp", 0, 0, &sbgp));
    }
    // stts: runs of equal durations.
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for sample in samples {
        match runs.last_mut() {
            Some((count, duration)) if *duration == sample.duration => *count += 1,
            _ => runs.push((1, sample.duration)),
        }
    }
    let mut stts = (runs.len() as u32).to_be_bytes().to_vec();
    for (count, duration) in &runs {
        stts.extend_from_slice(&count.to_be_bytes());
        stts.extend_from_slice(&duration.to_be_bytes());
    }
    body.extend(full(b"stts", 0, 0, &stts));
    if matches!(track.kind, TrackKind::Video { .. }) && samples.iter().any(|s| !s.sync) {
        let syncs: Vec<u32> = samples
            .iter()
            .enumerate()
            .filter(|(_, s)| s.sync)
            .map(|(i, _)| i as u32 + 1)
            .collect();
        let mut stss = (syncs.len() as u32).to_be_bytes().to_vec();
        for s in syncs {
            stss.extend_from_slice(&s.to_be_bytes());
        }
        body.extend(full(b"stss", 0, 0, &stss));
    }
    // Chunks: runs of samples stored back to back.
    let mut chunks: Vec<(u64, u32)> = Vec::new();
    let mut expected: Option<u64> = None;
    for sample in samples {
        if expected == Some(sample.offset) {
            chunks.last_mut().expect("a chunk").1 += 1;
        } else {
            chunks.push((sample.offset, 1));
        }
        expected = Some(sample.offset + u64::from(sample.size));
    }
    let mut stsc_runs: Vec<(u32, u32)> = Vec::new();
    for (i, (_, count)) in chunks.iter().enumerate() {
        match stsc_runs.last() {
            Some((_, last)) if last == count => {}
            _ => stsc_runs.push((i as u32 + 1, *count)),
        }
    }
    let mut stsc = (stsc_runs.len() as u32).to_be_bytes().to_vec();
    for (first, count) in &stsc_runs {
        stsc.extend_from_slice(&first.to_be_bytes());
        stsc.extend_from_slice(&count.to_be_bytes());
        stsc.extend_from_slice(&1u32.to_be_bytes());
    }
    body.extend(full(b"stsc", 0, 0, &stsc));
    let mut stsz = 0u32.to_be_bytes().to_vec();
    stsz.extend_from_slice(&(samples.len() as u32).to_be_bytes());
    for sample in samples {
        stsz.extend_from_slice(&sample.size.to_be_bytes());
    }
    body.extend(full(b"stsz", 0, 0, &stsz));
    // 32-bit chunk offsets unless the file needs more: AVFoundation reads
    // the fragments of an unfinished movie only after an (empty) `stco`,
    // not a `co64` (one sample, then nothing, with `co64`), and small
    // finished files are then also what every reader expects.
    if chunks
        .iter()
        .all(|(offset, _)| *offset <= u64::from(u32::MAX))
    {
        let mut stco = (chunks.len() as u32).to_be_bytes().to_vec();
        for (offset, _) in &chunks {
            stco.extend_from_slice(&(*offset as u32).to_be_bytes());
        }
        body.extend(full(b"stco", 0, 0, &stco));
    } else {
        let mut co64 = (chunks.len() as u32).to_be_bytes().to_vec();
        for (offset, _) in &chunks {
            co64.extend_from_slice(&offset.to_be_bytes());
        }
        body.extend(full(b"co64", 0, 0, &co64));
    }
    boxed(b"stbl", &body)
}

/// Sample flags of a video sample in a `trun`.
fn sample_flags(sync: bool) -> u32 {
    if sync {
        // Depends on no other sample.
        0x0200_0000
    } else {
        // Depends on others, not a sync sample.
        0x0101_0000
    }
}

fn moof(
    sequence: u32,
    tracks: &[Track],
    fragment: &[FragmentTrack<'_>],
    sizes: &[u64],
    data_start: u64,
    video_shift: u64,
) -> Vec<u8> {
    let mut body = full(b"mfhd", 0, 0, &sequence.to_be_bytes());
    let mut data_offset = data_start;
    for (index, part) in fragment.iter().enumerate() {
        if part.samples.is_empty() {
            data_offset += sizes[index];
            continue;
        }
        let track = &tracks[index];
        let video = matches!(track.kind, TrackKind::Video { .. });
        let mut traf = Vec::new();
        // tfhd: default-base-is-moof, sample description 1.
        let mut tfhd = (index as u32 + 1).to_be_bytes().to_vec();
        tfhd.extend_from_slice(&1u32.to_be_bytes());
        traf.extend(full(b"tfhd", 0, 0x2_0002, &tfhd));
        let shift = if video { video_shift } else { 0 };
        let first_time = part.samples[0].1 + shift;
        traf.extend(full(b"tfdt", 1, 0, &first_time.to_be_bytes()));
        // trun: data offset, per-sample duration and size (and flags for video).
        let flags = if video { 0x701 } else { 0x301 };
        let mut trun = (part.samples.len() as u32).to_be_bytes().to_vec();
        trun.extend_from_slice(&(data_offset as u32).to_be_bytes());
        for &(size, _, duration, sync) in part.samples {
            trun.extend_from_slice(&duration.to_be_bytes());
            trun.extend_from_slice(&size.to_be_bytes());
            if video {
                trun.extend_from_slice(&sample_flags(sync).to_be_bytes());
            }
        }
        traf.extend(full(b"trun", 0, flags, &trun));
        body.extend(boxed(b"traf", &traf));
        data_offset += sizes[index];
    }
    boxed(b"moof", &body)
}

// Sample entries.

/// A video sample entry for `fourcc` with the codec's own atoms (`avcC`,
/// `hvcC`, ...) and Rec. 709 colour tags.
pub fn video_sample_entry(
    fourcc: [u8; 4],
    width: u16,
    height: u16,
    depth: u16,
    name: &str,
    atoms: &[([u8; 4], Vec<u8>)],
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0u8; 6]);
    body.extend_from_slice(&1u16.to_be_bytes());
    body.extend_from_slice(&[0u8; 16]);
    body.extend_from_slice(&width.to_be_bytes());
    body.extend_from_slice(&height.to_be_bytes());
    body.extend_from_slice(&0x48_0000u32.to_be_bytes());
    body.extend_from_slice(&0x48_0000u32.to_be_bytes());
    body.extend_from_slice(&[0u8; 4]);
    body.extend_from_slice(&1u16.to_be_bytes());
    let mut compressor = [0u8; 32];
    let name = name.as_bytes();
    let n = name.len().min(31);
    compressor[0] = n as u8;
    compressor[1..1 + n].copy_from_slice(&name[..n]);
    body.extend_from_slice(&compressor);
    body.extend_from_slice(&depth.to_be_bytes());
    body.extend_from_slice(&0xffffu16.to_be_bytes());
    let mut has_colr = false;
    for (kind, data) in atoms {
        has_colr |= kind == b"colr";
        body.extend(boxed(kind, data));
    }
    if !has_colr {
        let mut colr = b"nclc".to_vec();
        colr.extend_from_slice(&1u16.to_be_bytes());
        colr.extend_from_slice(&1u16.to_be_bytes());
        colr.extend_from_slice(&1u16.to_be_bytes());
        body.extend(boxed(b"colr", &colr));
    }
    boxed(&fourcc, &body)
}

/// An AAC-LC sample entry: QuickTime's (version 1, with a `wave` atom) or
/// MP4's (version 0, `esds` directly).
pub fn aac_sample_entry(container: Container, channels: u16, sample_rate: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0u8; 6]);
    body.extend_from_slice(&1u16.to_be_bytes());
    let esds = esds(channels, sample_rate);
    match container {
        Container::Mov => {
            body.extend_from_slice(&1u16.to_be_bytes());
            body.extend_from_slice(&[0u8; 6]);
            body.extend_from_slice(&channels.to_be_bytes());
            body.extend_from_slice(&16u16.to_be_bytes());
            body.extend_from_slice(&0xfffeu16.to_be_bytes());
            body.extend_from_slice(&[0u8; 2]);
            body.extend_from_slice(&(sample_rate << 16).to_be_bytes());
            body.extend_from_slice(&1024u32.to_be_bytes());
            body.extend_from_slice(&0u32.to_be_bytes());
            body.extend_from_slice(&1u32.to_be_bytes());
            body.extend_from_slice(&2u32.to_be_bytes());
            let mut wave = boxed(b"frma", b"mp4a");
            wave.extend(boxed(b"mp4a", &[0u8; 4]));
            wave.extend(&esds);
            wave.extend_from_slice(&[0u8; 8]);
            body.extend(boxed(b"wave", &wave));
        }
        Container::Mp4 => {
            body.extend_from_slice(&[0u8; 8]);
            body.extend_from_slice(&channels.to_be_bytes());
            body.extend_from_slice(&16u16.to_be_bytes());
            body.extend_from_slice(&[0u8; 4]);
            body.extend_from_slice(&(sample_rate << 16).to_be_bytes());
            body.extend(&esds);
        }
    }
    boxed(b"mp4a", &body)
}

fn esds(channels: u16, sample_rate: u32) -> Vec<u8> {
    let rate_index = [
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000,
    ]
    .iter()
    .position(|r| *r == sample_rate)
    .unwrap_or(3) as u16;
    // AudioSpecificConfig: AAC LC (2), the rate index, the channel configuration.
    let config = ((2u16 << 11) | (rate_index << 7) | (channels.min(7) << 3)).to_be_bytes();
    let descriptor = |tag: u8, body: &[u8]| {
        let mut out = vec![tag, 0x80, 0x80, 0x80, body.len() as u8];
        out.extend_from_slice(body);
        out
    };
    let specific = descriptor(0x05, &config);
    let mut decoder = vec![0x40, 0x15, 0, 0, 0];
    let bitrate: u32 = 128_000 * u32::from(channels.clamp(1, 2));
    decoder.extend_from_slice(&bitrate.to_be_bytes());
    decoder.extend_from_slice(&bitrate.to_be_bytes());
    decoder.extend(specific);
    let decoder = descriptor(0x04, &decoder);
    let mut es = vec![0, 0, 0];
    es.extend(decoder);
    es.extend(descriptor(0x06, &[0x02]));
    full(b"esds", 0, 0, &descriptor(0x03, &es))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recover(path: &Path) -> Result<Option<Recovered>> {
        let mut file = open_for_repair(path.parent().unwrap(), path.file_name().unwrap())?;
        super::recover(&mut file)
    }

    fn tracks() -> Vec<Track> {
        vec![
            Track {
                kind: TrackKind::Video {
                    width: 64,
                    height: 48,
                },
                timescale: 60_000,
                sample_entry: video_sample_entry(
                    *b"hvc1",
                    64,
                    48,
                    24,
                    "HEVC",
                    &[(*b"hvcC", vec![1, 2, 3])],
                ),
                skip: 0,
            },
            Track {
                kind: TrackKind::Audio {
                    channels: 2,
                    sample_rate: 48_000,
                },
                timescale: 48_000,
                sample_entry: aac_sample_entry(Container::Mov, 2, 48_000),
                skip: AAC_PRIMING,
            },
        ]
    }

    /// Fragments survive a cut-short tail and come back as one movie whose
    /// sample tables point at the same bytes.
    #[test]
    fn interrupted_fragments_are_finished_at_recovery() {
        let dir = std::env::temp_dir().join(format!("isocast-mp4-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("take.mov");
        let started = SystemTime::now();
        let mut writer = Writer::create(&path, Container::Mov, started).unwrap();
        writer.set_tracks(tracks());
        let shift = video_shift(60_000);
        for fragment in 0..3u64 {
            // The third fragment starts 10 frames late (frames were
            // dropped at the boundary): the finished table must keep the
            // gap by stretching the picture before it.
            let gap = if fragment == 2 { 10_000 } else { 0 };
            let video: Vec<(u32, u64, u32, bool)> = (0..30)
                .map(|i| (4, (fragment * 30 + i) * 1000 + gap, 1000, i == 0))
                .collect();
            let audio: Vec<(u32, u64, u32, bool)> = (0..23)
                .map(|i| (3, (fragment * 23 + i) * 1024, 1024, true))
                .collect();
            let vdata = vec![fragment as u8; 30 * 4];
            let adata = vec![0xa0 | fragment as u8; 23 * 3];
            writer
                .write_fragment(
                    &[
                        FragmentTrack {
                            samples: &video,
                            data: &vdata,
                        },
                        FragmentTrack {
                            samples: &audio,
                            data: &adata,
                        },
                    ],
                    shift,
                )
                .unwrap();
        }
        let written: Vec<Sample> = writer.samples(0).to_vec();
        drop(writer);
        // A fourth fragment the crash cut in half.
        {
            let mut file = OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(&box_header(b"moof", 500)).unwrap();
            file.write_all(&[0u8; 100]).unwrap();
        }
        let recovered = recover(&path).unwrap().unwrap();
        assert_eq!(recovered.fragments, 3);
        assert_eq!(recovered.tracks[0].samples, 90);
        assert_eq!(recovered.tracks[1].samples, 69);
        // 90 frames plus the 10-frame gap.
        assert!((recovered.tracks[0].seconds - 100.0 / 60.0).abs() < 1e-9);
        assert_eq!(recovered.pictures(), Some(90));
        assert_eq!(
            recovered.summary(),
            "video 90 pictures (1.667 s); audio 1.428 s"
        );
        // Now an ordinary movie: ftyp, mdat, moov.
        let data = std::fs::read(&path).unwrap();
        let kinds: Vec<[u8; 4]> = children(&data).map(|(k, ..)| k).collect();
        assert_eq!(kinds, [*b"ftyp", *b"mdat", *b"moov"]);
        let moov = find_box(&data, b"moov").unwrap();
        assert!(find_box(moov, b"mvex").is_none());
        let recovered_tracks = parse_tracks(moov).unwrap();
        assert_eq!(recovered_tracks[0].kind, tracks()[0].kind);
        assert_eq!(recovered_tracks[1].skip, AAC_PRIMING);
        // The first sample of the second fragment is where the writer put it.
        let moov_offset = data.len() - (moov.len() + 8);
        let head = ftyp(Container::Mov, true).len();
        let mdat_len = u64::from_be_bytes(data[head + 8..head + 16].try_into().unwrap());
        assert_eq!(mdat_len, (moov_offset - head) as u64);
        let stbl_video = {
            let trak = find_box(moov, b"trak").unwrap();
            let mdia = find_box(trak, b"mdia").unwrap();
            let minf = find_box(mdia, b"minf").unwrap();
            find_box(minf, b"stbl").unwrap().to_vec()
        };
        let stco = find_box(&stbl_video, b"stco").unwrap();
        let second_chunk = u64::from(be32(stco, 12).unwrap());
        assert_eq!(second_chunk, written[30].offset);
        assert_eq!(data[second_chunk as usize], 1);
        // stts spans the gap: 59 frames of 1000, one of 11000, 30 of 1000.
        let stts = find_box(&stbl_video, b"stts").unwrap();
        let entries = be32(stts, 4).unwrap();
        let runs: Vec<(u32, u32)> = (0..entries)
            .map(|i| {
                (
                    be32(stts, 8 + 8 * i as usize).unwrap(),
                    be32(stts, 12 + 8 * i as usize).unwrap(),
                )
            })
            .collect();
        assert_eq!(runs, [(59, 1000), (1, 11_000), (30, 1000)]);
        let mdhd = {
            let trak = find_box(moov, b"trak").unwrap();
            let mdia = find_box(trak, b"mdia").unwrap();
            find_box(mdia, b"mdhd").unwrap().to_vec()
        };
        assert_eq!(be32(&mdhd, 16).unwrap(), 100_000);
        // The take's start survives as the movie's creation time.
        assert_eq!(parse_created(moov), movie_time(started));
        assert_eq!(be32(&mdhd, 4).map(u64::from), Some(movie_time(started)));
        // The audio edit presents only the sound that exists (69 packets
        // minus priming = 1.428 s), not the movie's 1.667 s.
        let audio_elst = {
            let traks: Vec<&[u8]> = children(moov)
                .filter(|(k, ..)| k == b"trak")
                .map(|(_, b, _)| b)
                .collect();
            let edts = find_box(traks[1], b"edts").unwrap();
            find_box(edts, b"elst").unwrap().to_vec()
        };
        let presented = be32(&audio_elst, 8).unwrap();
        assert_eq!(presented, (69 * 1024 - 2112) as u32);
        // Already finished: nothing to do.
        assert!(recover(&path).unwrap().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_torn_finish_only_needs_the_header_patch() {
        let dir = std::env::temp_dir().join(format!("isocast-mp4-torn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("take.mp4");
        let mut writer = Writer::create(&path, Container::Mp4, SystemTime::now()).unwrap();
        writer.set_tracks(tracks());
        let video = [(4u32, 0u64, 1000u32, true)];
        writer
            .write_fragment(
                &[
                    FragmentTrack {
                        samples: &video,
                        data: &[9; 4],
                    },
                    FragmentTrack {
                        samples: &[],
                        data: &[],
                    },
                ],
                0,
            )
            .unwrap();
        // The final moov lands, the patch doesn't.
        let len = writer.len;
        let moov = moov(
            Container::Mp4,
            &writer.tracks,
            Some(&writer.samples),
            800,
            writer.created,
        );
        writer.file.seek(SeekFrom::Start(len)).unwrap();
        writer.file.write_all(&moov).unwrap();
        drop(writer);
        let recovered = recover(&path).unwrap().unwrap();
        assert_eq!(recovered.fragments, 0);
        assert_eq!(recovered.pictures(), None);
        assert_eq!(recovered.summary(), "");
        let data = std::fs::read(&path).unwrap();
        let kinds: Vec<[u8; 4]> = children(&data).map(|(k, ..)| k).collect();
        assert_eq!(kinds, [*b"ftyp", *b"mdat", *b"moov"]);
        // The audio track had no samples: the movie has one track.
        let moov = find_box(&data, b"moov").unwrap();
        assert_eq!(children(moov).filter(|(k, ..)| k == b"trak").count(), 1);
        // A patch torn after any number of bytes, 0 to 16, is recognised
        // and completed: every one of the 17 states ends as the same file.
        let ftyp_len = ftyp(Container::Mp4, true).len();
        let header = mdat_header(moov_at(&data) as u64 - ftyp_len as u64);
        for written in 0..=16usize {
            let mut bytes = data.clone();
            bytes[ftyp_len..ftyp_len + 16].copy_from_slice(&placeholder());
            bytes[ftyp_len..ftyp_len + written].copy_from_slice(&header[..written]);
            std::fs::write(&path, &bytes).unwrap();
            let recovered = recover(&path).unwrap();
            if written == 16 {
                assert!(recovered.is_none(), "{written} bytes: already finished");
            } else {
                assert_eq!(recovered.unwrap().fragments, 0, "{written} bytes");
            }
            assert_eq!(std::fs::read(&path).unwrap(), data, "{written} bytes");
            assert!(recover(&path).unwrap().is_none());
        }
        // Garbage in the placeholder is recovered too (the file behind it
        // is ours), a foreign movie is left alone.
        let mut bytes = data.clone();
        bytes[ftyp_len..ftyp_len + 16].copy_from_slice(&[0xa5; 16]);
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(recover(&path).unwrap().unwrap().fragments, 0);
        assert_eq!(std::fs::read(&path).unwrap(), data);
        let foreign = dir.join("foreign.mp4");
        let mut other = ftyp(Container::Mp4, false);
        other.extend(boxed(b"wide", &[]));
        other.extend(boxed(b"mdat", &[7u8; 64]));
        other.extend(boxed(b"moov", &mvhd(0, 1, 0)));
        std::fs::write(&foreign, &other).unwrap();
        assert!(recover(&foreign).unwrap().is_none());
        assert_eq!(std::fs::read(&foreign).unwrap(), other);
        // A look-alike from elsewhere (another muxer's ftyp, then exactly
        // our layout: a 16-byte free box and a fragmented moov) is not ours
        // and stays byte for byte as it is.
        let shaped = dir.join("shaped.mp4");
        let mut body = b"isom".to_vec();
        body.extend_from_slice(&512u32.to_be_bytes());
        body.extend_from_slice(b"isomiso2avc1mp41");
        let mut lookalike = boxed(b"ftyp", &body);
        let own = ftyp(Container::Mp4, true).len();
        lookalike.extend_from_slice(&data[own..]);
        std::fs::write(&shaped, &lookalike).unwrap();
        assert!(recover(&shaped).unwrap().is_none());
        assert_eq!(std::fs::read(&shaped).unwrap(), lookalike);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A take whose video stalled after its first picture: audio
    /// fragments reached the disk, no picture did. The movie presents
    /// the audio for as long as it lasts, leaves the video track out, and
    /// says so.
    #[test]
    fn a_take_without_pictures_keeps_its_audio() {
        let dir = std::env::temp_dir().join(format!("isocast-mp4-audio-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("take.mov");
        let mut writer = Writer::create(&path, Container::Mov, SystemTime::now()).unwrap();
        writer.set_tracks(tracks());
        for fragment in 0..2u64 {
            let audio: Vec<(u32, u64, u32, bool)> = (0..23)
                .map(|i| (3, (fragment * 23 + i) * 1024, 1024, true))
                .collect();
            let adata = vec![0xa0 | fragment as u8; 23 * 3];
            writer
                .write_fragment(
                    &[
                        FragmentTrack {
                            samples: &[],
                            data: &[],
                        },
                        FragmentTrack {
                            samples: &audio,
                            data: &adata,
                        },
                    ],
                    video_shift(60_000),
                )
                .unwrap();
        }
        drop(writer);
        let recovered = recover(&path).unwrap().unwrap();
        assert_eq!(recovered.fragments, 2);
        assert_eq!(recovered.pictures(), Some(0));
        let audio_seconds = (46.0 * 1024.0 - 2112.0) / 48_000.0;
        assert!((recovered.shortest().unwrap() - audio_seconds).abs() < 1e-9);
        assert_eq!(
            recovered.summary(),
            "audio 0.937 s; left out: video (no picture)"
        );
        let data = std::fs::read(&path).unwrap();
        let moov = find_box(&data, b"moov").unwrap();
        let traks: Vec<&[u8]> = children(moov)
            .filter(|(k, ..)| k == b"trak")
            .map(|(_, b, _)| b)
            .collect();
        assert_eq!(traks.len(), 1);
        // The movie lasts as long as the audio; so does the audio's edit.
        let mvhd = find_box(moov, b"mvhd").unwrap();
        let expected = (audio_seconds * f64::from(MOVIE_TIMESCALE)).round() as u32;
        assert_eq!(be32(mvhd, 16).unwrap(), expected);
        let edts = find_box(traks[0], b"edts").unwrap();
        let elst = find_box(edts, b"elst").unwrap();
        assert_eq!(be32(elst, 8).unwrap(), expected);
        assert_eq!(be32(elst, 12).unwrap(), AAC_PRIMING as u32);
        assert!(recover(&path).unwrap().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A recorder keeps its movie locked: repair leaves it alone until the
    /// recorder is gone, wherever the file was moved meanwhile.
    #[test]
    fn repair_skips_a_file_its_recorder_still_holds() {
        let dir = std::env::temp_dir().join(format!("isocast-mp4-lock-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::create_dir_all(dir.join("b")).unwrap();
        let path = dir.join("a/take.mov");
        let writer = Writer::create(&path, Container::Mov, SystemTime::now()).unwrap();
        let original = std::fs::read(&path).unwrap();
        assert!(Writer::create(&path, Container::Mov, SystemTime::now()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::rename(&path, dir.join("b/take.mov")).unwrap();
        let err = open_for_repair(&dir.join("b"), OsStr::new("take.mov")).unwrap_err();
        assert!(err.to_string().contains("still being written"), "{err:#}");
        drop(writer);
        assert!(open_for_repair(&dir.join("b"), OsStr::new("take.mov")).is_ok());
        let err = recover(&dir.join("b/take.mov")).unwrap_err();
        assert!(
            err.to_string().contains("shorter than two frames"),
            "{err:#}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Where the final `moov` of a finished file starts.
    fn moov_at(data: &[u8]) -> usize {
        children(data)
            .find(|(k, ..)| k == b"moov")
            .map(|(_, _, start)| start)
            .unwrap()
    }

    /// Repair opens only regular files inside the folder: a symbolic link
    /// (to a movie another engine may still be writing) and a hard link
    /// are refused before anything is read.
    #[test]
    fn repair_refuses_aliases() {
        let dir = std::env::temp_dir().join(format!("isocast-mp4-alias-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("owner")).unwrap();
        std::fs::create_dir_all(dir.join("other")).unwrap();
        let movie = dir.join("owner/take.mov");
        let mut writer = Writer::create(&movie, Container::Mov, SystemTime::now()).unwrap();
        writer.set_tracks(tracks());
        drop(writer);
        std::os::unix::fs::symlink(&movie, dir.join("other/link.mov")).unwrap();
        let original = std::fs::read(&movie).unwrap();
        assert!(
            Writer::create(
                &dir.join("other/link.mov"),
                Container::Mov,
                SystemTime::now()
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&movie).unwrap(), original);
        let err = open_for_repair(&dir.join("other"), OsStr::new("link.mov")).unwrap_err();
        assert!(err.to_string().contains("symbolic link"), "{err:#}");
        std::fs::hard_link(&movie, dir.join("other/hard.mov")).unwrap();
        assert!(
            Writer::create(
                &dir.join("other/hard.mov"),
                Container::Mov,
                SystemTime::now()
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&movie).unwrap(), original);
        let err = open_for_repair(&dir.join("other"), OsStr::new("hard.mov")).unwrap_err();
        assert!(err.to_string().contains("hard links"), "{err:#}");
        // And the owner's own entry now has two names: refused there too.
        let err = open_for_repair(&dir.join("owner"), OsStr::new("take.mov")).unwrap_err();
        assert!(err.to_string().contains("hard links"), "{err:#}");
        std::fs::remove_file(dir.join("other/hard.mov")).unwrap();
        assert!(open_for_repair(&dir.join("owner"), OsStr::new("take.mov")).is_ok());
        for bad in ["sub/take.mov", "..", "."] {
            assert!(open_for_repair(&dir.join("owner"), OsStr::new(bad)).is_err());
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
