//! Crash-safe MOV / MP4 for already-compressed, non-reordered packets.
//!
//! Like the macOS writer, this appends fragments behind a fragmented
//! `moov`. Each index starts as `free`: only after its `mdat` and `icmt`
//! commit trailer are complete is its four-byte kind published as `moof`.
//! The trailer binds the sequence, index offset and data end to SHA-256 of
//! the canonical `moof`, without hashing or rereading media payloads.
//! Finishing appends ordinary sample tables, syncs, then
//! patches just the 16 bytes after `ftyp` into one encompassing `mdat`.
//! The portable format has its own `isw2` brand: it never repairs the Mac
//! writer's files, and that writer never repairs these.
//!
//! Per-track `isct` version 1 stores the explicit edit skip and fragment
//! shift. A final `moov` ends in `isfn` version 1, containing its own size.
//! It also records the end of the last complete fragment, including its
//! commit trailer. That footer
//! locates a completed repair even behind a cut-short fragment
//! and a torn header patch. Neither is trusted until the complete index is
//! reproduced from the original fragments, byte for byte.
//! Before recovery appends an index behind an unverified tail, it seals
//! that tail with one `!` byte at its box-kind start. This marker always
//! overrides a commit: later append bytes cannot complete a torn trailer
//! and promote an abandoned fragment. No committed media is overwritten.
//!
//! Index limits are 64 tracks, 1.5 million samples across the take, 1 MiB
//! per sample description (4 MiB together), and 64 MiB per index box.
//! Producers must split before reaching the sample limit. A fragment's
//! index and data together must fit signed 32-bit `trun` offsets.

use std::ffi::OsStr;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use isocast_protocol::Container;
use sha2::{Digest, Sha256};

use crate::file::MediaFile;

pub const MOVIE_TIMESCALE: u32 = 48_000;
const PLACEHOLDER: usize = 16;
const FINAL_FOOTER: usize = 28;
const FRAGMENT_COMMIT: usize = 64;
const ABANDONED_KIND: u8 = b'!';
const MAX_TRACKS: usize = 64;
/// Across video and audio together. Adapters must split before this budget;
/// its worst-case final index (40 bytes/sample plus descriptions) fits 64 MiB.
pub const MAX_SAMPLES: usize = 1_500_000;
const MAX_INDEX: usize = 64 << 20;
const MAX_ENTRY: usize = 1 << 20;
const MAX_ENTRIES: usize = 4 << 20;
const MAX_CHILDREN: usize = 256;
const ISOCAST_BRAND: &[u8; 4] = b"isw2";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video { width: u16, height: u16 },
    Audio { channels: u16, sample_rate: u32 },
}

#[derive(Debug, Clone)]
pub struct Track {
    pub kind: TrackKind,
    pub timescale: u32,
    /// Complete `avc1`, `hvc1` or `mp4a` box, including its header.
    pub sample_entry: Vec<u8>,
    /// Encoder priming to remove in the finished edit, in track ticks.
    pub skip: u64,
    /// Added to packet DTS in `tfdt` only, in this track's ticks.
    /// The caller chooses a common padding interval across all tracks.
    pub fragment_shift: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub offset: u64,
    pub size: u32,
    /// Unshifted decode time. Presentation must equal decode time.
    pub time: u64,
    pub duration: u32,
    pub sync: bool,
}

pub struct FragmentTrack<'a> {
    /// `(byte length, DTS, duration, random access)`, in decode order.
    pub samples: &'a [(u32, u64, u32, bool)],
    /// Concatenated encoded samples, not pixels or PCM.
    pub data: &'a [u8],
}

pub struct Writer {
    file: MediaFile,
    container: Container,
    tracks: Vec<Track>,
    samples: Vec<Vec<Sample>>,
    len: u64,
    sequence: u32,
    moov_written: bool,
    failed: bool,
    created: u64,
}

impl Writer {
    pub fn create(path: &Path, container: Container, created: SystemTime) -> Result<Self> {
        let mut file = crate::file::create(path)?;
        let mut head = ftyp(container);
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
            failed: false,
            created: movie_time(created),
        })
    }

    /// Install formats once, before writing anything but the file header.
    pub fn set_tracks(&mut self, tracks: Vec<Track>) -> Result<()> {
        ensure!(
            !self.failed,
            "the writer failed; abandon it and repair the file"
        );
        ensure!(
            !self.has_tracks() && !self.moov_written,
            "tracks are already set"
        );
        validate_tracks(&tracks)?;
        self.samples = tracks.iter().map(|_| Vec::new()).collect();
        self.tracks = tracks;
        Ok(())
    }

    pub fn has_tracks(&self) -> bool {
        !self.tracks.is_empty()
    }

    pub(crate) fn container(&self) -> Container {
        self.container
    }

    /// Cancel only the initial, owned header. No published index or media is
    /// removed; identity is rechecked through the original directory lease.
    #[cfg(windows)]
    pub fn cancel_empty(self) -> Result<()> {
        ensure!(
            !self.moov_written && self.sequence == 0,
            "movie has fragments; retained instead of cancelling"
        );
        let mut expected = ftyp(self.container);
        expected.extend_from_slice(&placeholder());
        self.file.cancel_empty(&expected)
    }

    /// Empty tracks are omitted from this fragment, but keep their ids.
    /// Durations inside a fragment follow the next DTS, preserving gaps.
    /// A failed I/O poisons this writer; repair uses the surviving bytes.
    pub fn write_fragment(&mut self, parts: &[FragmentTrack<'_>]) -> Result<()> {
        ensure!(
            !self.failed,
            "the writer failed; abandon it and repair the file"
        );
        ensure!(self.has_tracks(), "tracks must be set before packets");
        ensure!(
            parts.len() == self.tracks.len(),
            "fragment track count changed"
        );
        let mut count = self.samples.iter().map(Vec::len).sum::<usize>();
        let mut data_len = 0u64;
        for (i, part) in parts.iter().enumerate() {
            count = count
                .checked_add(part.samples.len())
                .context("sample count overflow")?;
            ensure!(
                count <= MAX_SAMPLES,
                "sample index exceeds {MAX_SAMPLES} samples; split the take"
            );
            let track = &self.tracks[i];
            let mut previous = self.samples[i].last().map(|s| s.time);
            let mut bytes = 0u64;
            for &(size, time, duration, sync) in part.samples {
                validate_time(time, duration, track.fragment_shift)?;
                movie_ticks(
                    add(time, u64::from(duration))?.saturating_sub(track.skip),
                    track.timescale,
                )?;
                ensure!(size > 0, "track {i}: empty encoded sample");
                if let Some(last) = previous {
                    next_duration(last, time)?;
                }
                if matches!(track.kind, TrackKind::Audio { .. }) {
                    ensure!(sync, "track {i}: AAC packets must be random-access samples");
                }
                bytes = bytes
                    .checked_add(u64::from(size))
                    .context("sample bytes overflow")?;
                previous = Some(time);
            }
            ensure!(
                bytes == part.data.len() as u64,
                "track {i}: sample sizes do not match packet bytes"
            );
            data_len = data_len
                .checked_add(bytes)
                .context("fragment size overflow")?;
        }
        ensure!(
            data_len <= i32::MAX as u64,
            "fragment exceeds signed trun offsets; flush more often"
        );
        if parts.iter().all(|p| p.samples.is_empty()) {
            return Ok(());
        }
        let sequence = self
            .sequence
            .checked_add(1)
            .context("fragment count overflow")?;
        let initial = if self.moov_written {
            Vec::new()
        } else {
            moov(self.container, &self.tracks, None, self.created)?
        };
        let moof_offset = add(self.len, initial.len() as u64)?;
        let draft = moof(sequence, &self.tracks, parts, 0)?;
        let data_start = add(draft.len() as u64, 8)?;
        ensure!(
            add(data_start, data_len)? <= i32::MAX as u64,
            "fragment exceeds signed trun offsets"
        );
        let mut head = moof(sequence, &self.tracks, parts, data_start)?;
        let first_data = add(moof_offset, data_start)?;
        let data_end = add(first_data, data_len)?;
        let end = add(data_end, FRAGMENT_COMMIT as u64)?;
        let commit = fragment_commit(sequence, moof_offset, data_end, &head);
        head[4..8].copy_from_slice(b"free");
        head.extend_from_slice(&box_header(b"mdat", add(data_len, 8)?));
        for (saved, part) in self.samples.iter_mut().zip(parts) {
            saved
                .try_reserve(part.samples.len())
                .context("reserving sample index")?;
        }
        // Readers must not see a moof until its data and commit trailer
        // landed. A crash before publication leaves an unpublished index;
        // repair accepts it only with an intact matching commit trailer.
        // Positional publication does not move MediaFile's append cursor.
        self.failed = true;
        self.file.write_all(&initial)?;
        self.file.write_all(&head)?;
        for part in parts {
            self.file.write_all(part.data)?;
        }
        self.file.write_all(&commit)?;
        self.file.write_all_at(b"moof", add(moof_offset, 4)?)?;
        let mut offset = first_data;
        for (saved, part) in self.samples.iter_mut().zip(parts) {
            for (j, &(size, time, duration, sync)) in part.samples.iter().enumerate() {
                let duration = match part.samples.get(j + 1) {
                    Some(next) => next_duration(time, next.1)?,
                    None => duration,
                };
                saved.push(Sample {
                    offset,
                    size,
                    time,
                    duration,
                    sync,
                });
                offset = add(offset, u64::from(size))?;
            }
        }
        self.len = end;
        self.sequence = sequence;
        self.moov_written = true;
        self.failed = false;
        Ok(())
    }

    /// Durability is the `MediaFile` platform's sync guarantee.
    pub fn sync(&self) -> Result<()> {
        self.file.sync_data().context("syncing media")
    }

    /// The longest actual track end determines movie duration. No caller's
    /// estimate may silently trim a longer complete track.
    pub fn finish(mut self) -> Result<()> {
        ensure!(
            !self.failed,
            "the writer failed; abandon it and repair the file"
        );
        ensure!(
            self.samples.iter().any(|s| !s.is_empty()),
            "no encoded sample reached the file"
        );
        fix_durations(&mut self.samples)?;
        let final_moov = moov(
            self.container,
            &self.tracks,
            Some(&self.samples),
            self.created,
        )?;
        let start = ftyp(self.container).len() as u64;
        finalize(&mut self.file, self.len, start, &final_moov)
    }

    pub fn abandon(self) {}
}

fn add(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b)
        .filter(|n| *n <= i64::MAX as u64)
        .context("file offset or timestamp overflow")
}

fn validate_time(time: u64, duration: u32, shift: u64) -> Result<()> {
    ensure!(duration > 0, "sample duration must be positive");
    add(add(time, u64::from(duration))?, shift)?;
    Ok(())
}

fn next_duration(time: u64, next: u64) -> Result<u32> {
    let delta = next
        .checked_sub(time)
        .filter(|d| *d > 0)
        .context("sample DTS must strictly increase")?;
    u32::try_from(delta).context("sample DTS gap exceeds a 32-bit duration")
}

fn fix_durations(samples: &mut [Vec<Sample>]) -> Result<()> {
    for track in samples {
        for i in 0..track.len().saturating_sub(1) {
            track[i].duration = next_duration(track[i].time, track[i + 1].time)?;
        }
    }
    Ok(())
}

fn finalize(file: &mut MediaFile, len: u64, start: u64, moov: &[u8]) -> Result<()> {
    add(len, moov.len() as u64)?;
    let size = len
        .checked_sub(start)
        .filter(|n| *n >= PLACEHOLDER as u64)
        .context("invalid media extent")?;
    file.seek(SeekFrom::Start(len))?;
    file.write_all(moov)?;
    file.sync_data()?;
    file.write_all_at(&mdat_header(size), start)?;
    file.sync_data()?;
    Ok(())
}

fn mdat_header(size: u64) -> [u8; PLACEHOLDER] {
    let mut header = [0u8; PLACEHOLDER];
    header[..4].copy_from_slice(&1u32.to_be_bytes());
    header[4..8].copy_from_slice(b"mdat");
    header[8..].copy_from_slice(&size.to_be_bytes());
    header
}

/// A full `icmt` box, version 1, flags 0. Its hash covers only the complete
/// index, including its eight-byte header with the kind canonicalized to
/// `moof`. The sequence and both offsets are checked separately on read.
fn fragment_commit(
    sequence: u32,
    moof_offset: u64,
    data_end: u64,
    canonical_moof: &[u8],
) -> [u8; FRAGMENT_COMMIT] {
    let mut commit = [0u8; FRAGMENT_COMMIT];
    commit[..4].copy_from_slice(&(FRAGMENT_COMMIT as u32).to_be_bytes());
    commit[4..8].copy_from_slice(b"icmt");
    commit[8..12].copy_from_slice(&0x0100_0000u32.to_be_bytes());
    commit[12..16].copy_from_slice(&sequence.to_be_bytes());
    commit[16..24].copy_from_slice(&moof_offset.to_be_bytes());
    commit[24..32].copy_from_slice(&data_end.to_be_bytes());
    commit[32..].copy_from_slice(&Sha256::digest(canonical_moof));
    commit
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecoveredTrack {
    pub video: bool,
    /// An empty track is explicitly omitted from the finished movie.
    pub samples: usize,
    /// True end on the movie timeline, including any initial empty edit.
    pub seconds: f64,
}

#[derive(Debug)]
pub struct Recovered {
    /// Complete, validated fragments, including for a patch-only repair.
    pub fragments: u32,
    pub tracks: Vec<RecoveredTrack>,
    /// False when only the 16-byte header needed repair. In `inspect`,
    /// describes what recovery would do, not a write performed by inspect.
    pub rebuilt: bool,
    /// Bytes outside the verified fragment prefix and canonical final
    /// index were left out. They may contain media or interrupted indexes;
    /// no sample count or duration is inferred from them. Also exposed by
    /// `Inspection::media`, including after a completed repair.
    pub unverified_tail: bool,
}

impl Recovered {
    pub fn pictures(&self) -> Option<u64> {
        Some(
            self.tracks
                .iter()
                .filter(|t| t.video)
                .map(|t| t.samples as u64)
                .sum(),
        )
    }

    pub fn shortest(&self) -> Option<f64> {
        self.tracks
            .iter()
            .filter(|t| t.samples > 0)
            .map(|t| t.seconds)
            .reduce(f64::min)
    }

    pub fn longest(&self) -> Option<f64> {
        self.tracks
            .iter()
            .filter(|t| t.samples > 0)
            .map(|t| t.seconds)
            .reduce(f64::max)
    }

    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        for track in &self.tracks {
            parts.push(match (track.video, track.samples) {
                (true, 0) => "left out: video (no picture)".to_string(),
                (false, 0) => "left out: audio (no packet)".to_string(),
                (true, n) => format!("video {n} pictures ({:.3} s)", track.seconds),
                (false, _) => format!("audio {:.3} s", track.seconds),
            });
        }
        if self.unverified_tail {
            parts.push("left out: unverified tail (may contain media)".to_string());
        }
        parts.join("; ")
    }
}

pub fn open_for_repair(folder: &Path, name: &OsStr) -> Result<MediaFile> {
    crate::file::open_for_repair(folder, name)
}

/// Read only; never opens a path or repairs anything. Counts come from
/// validated fragments, even in a finished file (not guessed from fps).
#[derive(Debug)]
pub struct Inspection {
    pub container: Container,
    pub created: u64,
    pub complete: bool,
    pub tracks: Vec<Track>,
    pub media: Recovered,
}

pub fn inspect(file: &MediaFile) -> Result<Option<Inspection>> {
    let Some(scan) = scan(file)? else {
        return Ok(None);
    };
    Ok(Some(Inspection {
        container: scan.container,
        created: scan.created,
        complete: scan.complete,
        tracks: scan.tracks,
        media: scan.report,
    }))
}

/// Only this format's exact `ftyp` is eligible. All reads and writes use
/// the same protected object. Validation finishes before the first write.
pub fn recover(file: &mut MediaFile) -> Result<Option<Recovered>> {
    let Some(scan) = scan(file)? else {
        return Ok(None);
    };
    if scan.complete {
        return Ok(None);
    }
    ensure!(
        scan.report.tracks.iter().any(|t| t.samples > 0),
        "no complete fragment reached the file"
    );
    if let Some(at) = scan.final_at {
        // The completed index may not yet be durable (a process crash,
        // rather than a power loss). Sync it before publishing its header.
        file.sync_data()?;
        file.write_all_at(&mdat_header(at - scan.start), scan.start)?;
        file.sync_data()?;
    } else {
        if scan.len > scan.committed_end {
            // A torn trailer can be only one zero byte short of valid,
            // and the next moov append begins with zero. Permanently seal
            // the rejected tail BEFORE any append can complete it. A
            // one-byte positional write has only before/after kill states.
            let marker_at = add(scan.committed_end, 4)?;
            add(marker_at, 1)?;
            file.write_all_at(&[ABANDONED_KIND], marker_at)?;
        }
        // Sealing a header shorter than five bytes extends the file. Do
        // not overwrite that marker by seeking to the old scan length.
        let len = file.metadata()?.len();
        ensure!(len >= scan.len, "media file shrank during recovery");
        finalize(file, len, scan.start, &scan.final_moov)?;
    }
    Ok(Some(scan.report))
}

struct Scan {
    container: Container,
    created: u64,
    tracks: Vec<Track>,
    report: Recovered,
    len: u64,
    committed_end: u64,
    start: u64,
    final_at: Option<u64>,
    final_moov: Vec<u8>,
    complete: bool,
}

fn scan(file: &MediaFile) -> Result<Option<Scan>> {
    let len = file.metadata()?.len();
    ensure!(
        len <= i64::MAX as u64,
        "media file exceeds supported offsets"
    );
    // Fixed, tiny reads: a foreign ftyp length cannot request an allocation.
    let mut format = None;
    for container in [Container::Mov, Container::Mp4] {
        let expected = ftyp(container);
        if len >= expected.len() as u64 {
            let actual = read_range(file, 0, expected.len() as u64)?;
            if actual == expected {
                format = Some((container, expected.len() as u64));
                break;
            }
        }
    }
    let Some((container, start)) = format else {
        return Ok(None);
    };
    let probe = add(start, PLACEHOLDER as u64)?;
    ensure!(
        len >= add(probe, 8)?,
        "no complete fragment index reached the file"
    );
    let mut placeholder = [0u8; PLACEHOLDER];
    file.read_exact_at(&mut placeholder, start)?;
    let (size, kind, header) = read_header(file, probe, len)?;
    ensure!(
        kind == *b"moov" && size >= header && size <= len - probe,
        "incomplete initial moov"
    );
    let body = read_range(file, add(probe, header)?, size - header)?;
    let tracks = parse_tracks(&body)?;
    let created = parse_created(&body)?;
    let initial = moov(container, &tracks, None, created)?;
    ensure!(
        size == initial.len() as u64 && header == 8 && body == initial[8..],
        "initial moov does not match this format"
    );
    let first_fragment = add(probe, size)?;
    // A torn final footer can itself be extended by a later repair append.
    // Do not use any of its fields to bound the scan. Commit trailers alone
    // establish the prefix; only then can the canonical final index be
    // compared with the suffix of this file.
    let limit = len;
    let mut samples: Vec<Vec<Sample>> = tracks.iter().map(|_| Vec::new()).collect();
    let mut fragments = 0u32;
    let mut total = 0usize;
    let mut pos = first_fragment;
    while limit.saturating_sub(pos) >= 8 {
        let mut header = [0u8; 8];
        file.read_exact_at(&mut header, pos)?;
        // This must precede ALL size, data and trailer interpretation.
        // Appended bytes can make an abandoned trailer appear whole, but
        // can never undo this single-byte boundary marker.
        if header[4] == ABANDONED_KIND {
            break;
        }
        // The commit, not the publication kind, is authoritative. This
        // also accepts suffix/sector tears such as fref, frof and foof.
        let size = u64::from(be32(&header, 0)?);
        if !(8..=MAX_INDEX as u64).contains(&size) {
            break;
        }
        if size > limit - pos {
            break;
        }
        let mdat = add(pos, size)?;
        if limit.saturating_sub(mdat) < 8 {
            break;
        }
        let mut data_header = [0u8; 8];
        file.read_exact_at(&mut data_header, mdat)?;
        let data_size = u64::from(be32(&data_header, 0)?);
        if &data_header[4..8] != b"mdat"
            || data_size < 8
            || size
                .checked_add(data_size)
                .is_none_or(|n| n > i32::MAX as u64)
        {
            break;
        }
        if data_size > limit - mdat {
            break;
        }
        let data_start = add(mdat, 8)?;
        let data_end = add(mdat, data_size)?;
        if limit - data_end < FRAGMENT_COMMIT as u64 {
            break;
        }
        let mut commit = [0u8; FRAGMENT_COMMIT];
        file.read_exact_at(&mut commit, data_end)?;
        let mut index = read_range(file, pos, size)?;
        index[4..8].copy_from_slice(b"moof");
        let sequence = fragments
            .checked_add(1)
            .context("fragment count overflow")?;
        if commit != fragment_commit(sequence, pos, data_end, &index) {
            break;
        }
        // All file reads are above this validation-only boundary, so an
        // I/O failure propagates instead of being mistaken for a bad tail.
        let pending = (|| -> Result<Vec<(usize, Sample)>> {
            let pending = parse_moof(&index[8..], pos, &tracks, sequence, MAX_SAMPLES - total)?;
            ensure!(!pending.is_empty(), "empty fragment index");
            let mut expected = data_start;
            let mut previous: Vec<Option<u64>> =
                samples.iter().map(|s| s.last().map(|s| s.time)).collect();
            // Validate the entire fragment before retaining ANY sample.
            for &(track, sample) in &pending {
                let end = add(sample.offset, u64::from(sample.size))?;
                ensure!(
                    sample.offset == expected && end <= data_end,
                    "fragment sample is outside its mdat or overlaps another sample"
                );
                if let Some(last) = previous[track] {
                    next_duration(last, sample.time)?;
                }
                movie_ticks(
                    add(sample.time, u64::from(sample.duration))?
                        .saturating_sub(tracks[track].skip),
                    tracks[track].timescale,
                )?;
                previous[track] = Some(sample.time);
                expected = end;
            }
            ensure!(
                expected == data_end,
                "fragment sample sizes do not cover mdat"
            );
            Ok(pending)
        })();
        let Ok(pending) = pending else {
            break;
        };
        total += pending.len(); // parse_moof bounded it by the remaining budget.
        for (track, sample) in pending {
            samples[track].push(sample);
        }
        fragments = sequence;
        pos = add(data_end, FRAGMENT_COMMIT as u64)?;
    }
    ensure!(
        fragments > 0,
        "no complete verified fragment reached the file"
    );
    fix_durations(&mut samples)?;
    let final_moov = moov(container, &tracks, Some(&samples), created)?;
    let final_at = final_moov_at(file, len, pos, &final_moov)?;
    let complete = final_at.is_some_and(|at| placeholder == mdat_header(at - start));
    let mut report = Vec::with_capacity(tracks.len());
    for (track, samples) in tracks.iter().zip(&samples) {
        let end = track_end(track, samples)?;
        report.push(RecoveredTrack {
            video: matches!(track.kind, TrackKind::Video { .. }),
            samples: samples.len(),
            seconds: end as f64 / f64::from(track.timescale),
        });
    }
    Ok(Some(Scan {
        container,
        created,
        tracks,
        len,
        committed_end: pos,
        start,
        final_at,
        final_moov,
        complete,
        report: Recovered {
            fragments,
            tracks: report,
            rebuilt: final_at.is_none(),
            unverified_tail: pos < final_at.unwrap_or(len),
        },
    }))
}

fn final_moov_at(file: &MediaFile, len: u64, minimum: u64, expected: &[u8]) -> Result<Option<u64>> {
    ensure!(
        expected.len() >= FINAL_FOOTER && expected.len() <= MAX_INDEX,
        "invalid canonical final index size"
    );
    let Some(at) = len
        .checked_sub(expected.len() as u64)
        .filter(|at| *at >= minimum)
    else {
        return Ok(None);
    };
    // This is a comparison against trusted, regenerated bytes, not a
    // parse of untrusted footer lengths. A footer missing its last byte
    // followed by another partial moov must remain an uncommitted tail,
    // even if their concatenation happens to look like a complete footer.
    let mut footer = [0u8; FINAL_FOOTER];
    file.read_exact_at(&mut footer, len - FINAL_FOOTER as u64)?;
    if footer != expected[expected.len() - FINAL_FOOTER..] {
        return Ok(None);
    }
    let actual = read_range(file, at, expected.len() as u64)?;
    Ok((actual == expected).then_some(at))
}

fn read_header(file: &MediaFile, pos: u64, len: u64) -> Result<(u64, [u8; 4], u64)> {
    ensure!(pos <= len && len - pos >= 8, "box header is cut short");
    let mut head = [0u8; 8];
    file.read_exact_at(&mut head, pos)?;
    let mut size = u64::from(be32(&head, 0)?);
    let kind = head[4..8].try_into().context("box kind")?;
    let mut header = 8;
    if size == 1 {
        if len - pos < 16 {
            return Ok((0, kind, 16));
        }
        let mut large = [0u8; 8];
        file.read_exact_at(&mut large, add(pos, 8)?)?;
        size = u64::from_be_bytes(large);
        header = 16;
    } else {
        ensure!(size != 0, "open-ended boxes are not part of this format");
    }
    Ok((size, kind, header))
}

fn read_range(file: &MediaFile, pos: u64, len: u64) -> Result<Vec<u8>> {
    ensure!(
        len <= MAX_INDEX as u64,
        "index box exceeds {MAX_INDEX} bytes"
    );
    add(pos, len)?;
    let mut data = vec![0u8; len as usize];
    file.read_exact_at(&mut data, pos)?;
    Ok(data)
}

/// Strict, bounded children: malformed tails are errors, not silently
/// missing boxes. Only fixed-depth callers recurse into container boxes.
fn children(body: &[u8]) -> Result<Vec<([u8; 4], &[u8])>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < body.len() {
        ensure!(out.len() < MAX_CHILDREN, "too many child boxes");
        ensure!(body.len() - pos >= 8, "child box header is cut short");
        let size = be32(body, pos)? as usize;
        // Our indexes never need extended or open-ended child boxes.
        ensure!(
            size >= 8 && size <= body.len() - pos,
            "invalid child box length"
        );
        let kind = body[pos + 4..pos + 8].try_into().context("child kind")?;
        out.push((kind, &body[pos + 8..pos + size]));
        pos += size;
    }
    Ok(out)
}

fn find_box<'a>(body: &'a [u8], kind: &[u8; 4]) -> Result<&'a [u8]> {
    let mut found = None;
    for (k, data) in children(body)? {
        if &k == kind {
            ensure!(
                found.is_none(),
                "duplicate {} box",
                String::from_utf8_lossy(kind)
            );
            found = Some(data);
        }
    }
    found.with_context(|| format!("missing {} box", String::from_utf8_lossy(kind)))
}

fn be16(data: &[u8], at: usize) -> Result<u16> {
    let end = at.checked_add(2).context("index offset overflow")?;
    Ok(u16::from_be_bytes(
        data.get(at..end).context("short u16 field")?.try_into()?,
    ))
}

fn be32(data: &[u8], at: usize) -> Result<u32> {
    let end = at.checked_add(4).context("index offset overflow")?;
    Ok(u32::from_be_bytes(
        data.get(at..end).context("short u32 field")?.try_into()?,
    ))
}

fn be64(data: &[u8], at: usize) -> Result<u64> {
    let end = at.checked_add(8).context("index offset overflow")?;
    Ok(u64::from_be_bytes(
        data.get(at..end).context("short u64 field")?.try_into()?,
    ))
}

fn parse_created(moov: &[u8]) -> Result<u64> {
    let mvhd = find_box(moov, b"mvhd")?;
    match be32(mvhd, 0)? {
        0 => Ok(u64::from(be32(mvhd, 4)?)),
        0x0100_0000 => be64(mvhd, 4),
        _ => bail!("unsupported mvhd version or flags"),
    }
}

fn parse_tracks(moov: &[u8]) -> Result<Vec<Track>> {
    let mut tracks = Vec::new();
    let mut entry_bytes = 0usize;
    for (kind, trak) in children(moov)? {
        if kind != *b"trak" {
            continue;
        }
        ensure!(tracks.len() < MAX_TRACKS, "too many tracks");
        let tkhd = find_box(trak, b"tkhd")?;
        let (id_at, size_at) = match be32(tkhd, 0)? {
            0x0000_000f => (12, 76),
            0x0100_000f => (20, 88),
            _ => bail!("unsupported tkhd version or flags"),
        };
        ensure!(
            be32(tkhd, id_at)? as usize == tracks.len() + 1,
            "fragment track ids must be 1..n in order"
        );
        let mdia = find_box(trak, b"mdia")?;
        let mdhd = find_box(mdia, b"mdhd")?;
        let timescale = match be32(mdhd, 0)? {
            0 => be32(mdhd, 12)?,
            0x0100_0000 => be32(mdhd, 20)?,
            _ => bail!("unsupported mdhd version or flags"),
        };
        let hdlr = find_box(mdia, b"hdlr")?;
        let handler = hdlr.get(8..12).context("short handler")?;
        let minf = find_box(mdia, b"minf")?;
        let stbl = find_box(minf, b"stbl")?;
        let stsd = find_box(stbl, b"stsd")?;
        ensure!(
            be32(stsd, 0)? == 0 && be32(stsd, 4)? == 1,
            "expected one sample description"
        );
        let entry = stsd.get(8..).context("missing sample entry")?;
        ensure!(entry.len() <= MAX_ENTRY, "sample entry too large");
        entry_bytes = entry_bytes
            .checked_add(entry.len())
            .context("sample entries overflow")?;
        ensure!(
            entry_bytes <= MAX_ENTRIES,
            "sample descriptions exceed budget"
        );
        let kind = match handler {
            b"vide" => TrackKind::Video {
                width: (be32(tkhd, size_at)? >> 16) as u16,
                height: (be32(tkhd, size_at + 4)? >> 16) as u16,
            },
            b"soun" => TrackKind::Audio {
                channels: be16(entry, 24)?,
                sample_rate: timescale,
            },
            _ => bail!("unsupported track handler"),
        };
        let timing = find_box(trak, b"isct")?;
        ensure!(
            timing.len() == 20 && be32(timing, 0)? == 0x0100_0000,
            "unsupported isct version, flags or size"
        );
        tracks.push(Track {
            kind,
            timescale,
            sample_entry: entry.to_vec(),
            skip: be64(timing, 4)?,
            fragment_shift: be64(timing, 12)?,
        });
    }
    validate_tracks(&tracks)?;
    Ok(tracks)
}

fn parse_moof(
    moof: &[u8],
    offset: u64,
    tracks: &[Track],
    sequence: u32,
    budget: usize,
) -> Result<Vec<(usize, Sample)>> {
    let mfhd = find_box(moof, b"mfhd")?;
    ensure!(
        mfhd.len() == 8 && be32(mfhd, 0)? == 0 && be32(mfhd, 4)? == sequence,
        "invalid fragment sequence"
    );
    let mut out = Vec::new();
    let mut previous_track = None;
    for (kind, traf) in children(moof)? {
        if kind == *b"mfhd" {
            continue;
        }
        ensure!(kind == *b"traf", "unsupported moof child");
        let boxes = children(traf)?;
        ensure!(boxes.len() == 3, "expected tfhd, tfdt and one trun");
        let tfhd = find_box(traf, b"tfhd")?;
        ensure!(
            tfhd.len() == 12 && be32(tfhd, 0)? == 0x0002_0002 && be32(tfhd, 8)? == 1,
            "unsupported tfhd"
        );
        let track = (be32(tfhd, 4)? as usize)
            .checked_sub(1)
            .filter(|i| *i < tracks.len())
            .context("unknown fragment track")?;
        ensure!(
            previous_track.is_none_or(|p| p < track),
            "duplicate or unordered fragment track"
        );
        previous_track = Some(track);
        let tfdt = find_box(traf, b"tfdt")?;
        ensure!(
            tfdt.len() == 12 && be32(tfdt, 0)? == 0x0100_0000,
            "unsupported tfdt"
        );
        let mut time = be64(tfdt, 4)?
            .checked_sub(tracks[track].fragment_shift)
            .context("tfdt precedes track shift")?;
        let trun = find_box(traf, b"trun")?;
        let full_flags = be32(trun, 0)?;
        ensure!(full_flags >> 24 <= 1, "unsupported trun version");
        let flags = full_flags & 0x00ff_ffff;
        let video = matches!(tracks[track].kind, TrackKind::Video { .. });
        let expected = if video { 0x701 } else { 0x301 };
        ensure!(
            flags == expected || flags == (expected | 0x800),
            "unsupported trun flags"
        );
        let composition = flags & 0x800 != 0;
        let count = be32(trun, 4)? as usize;
        ensure!(
            count > 0 && count <= budget.saturating_sub(out.len()),
            "fragment sample count exceeds index budget"
        );
        let stride = 8 + usize::from(video) * 4 + usize::from(composition) * 4;
        let bytes = count
            .checked_mul(stride)
            .and_then(|n| n.checked_add(12))
            .context("trun size overflow")?;
        ensure!(
            bytes == trun.len(),
            "trun count does not match its byte length"
        );
        let delta = be32(trun, 8)? as i32;
        let mut at = offset
            .checked_add_signed(i64::from(delta))
            .context("trun data offset overflow")?;
        out.try_reserve(count)
            .context("reserving fragment samples")?;
        for i in 0..count {
            let p = 12 + i * stride;
            let duration = be32(trun, p)?;
            let size = be32(trun, p + 4)?;
            ensure!(size > 0, "empty encoded sample");
            validate_time(time, duration, tracks[track].fragment_shift)?;
            let sync = if video {
                match be32(trun, p + 8)? {
                    0x0200_0000 => true,
                    0x0101_0000 => false,
                    _ => bail!("unsupported video sample flags"),
                }
            } else {
                true
            };
            if composition {
                ensure!(
                    be32(trun, p + stride - 4)? == 0,
                    "nonzero composition offsets are not supported"
                );
            }
            out.push((
                track,
                Sample {
                    offset: at,
                    size,
                    time,
                    duration,
                    sync,
                },
            ));
            at = add(at, u64::from(size))?;
            time = add(time, u64::from(duration))?;
        }
    }
    Ok(out)
}

fn validate_tracks(tracks: &[Track]) -> Result<()> {
    ensure!(
        !tracks.is_empty() && tracks.len() <= MAX_TRACKS,
        "track count must be 1..={MAX_TRACKS}"
    );
    let mut bytes = 0usize;
    for (i, track) in tracks.iter().enumerate() {
        ensure!(
            track.timescale > 0,
            "tracks[{i}].timescale must be positive"
        );
        ensure!(
            track.skip <= i64::MAX as u64 && track.fragment_shift <= i64::MAX as u64,
            "tracks[{i}] timing exceeds signed media time"
        );
        validate_entry(track).with_context(|| format!("tracks[{i}].sample_entry"))?;
        bytes = bytes
            .checked_add(track.sample_entry.len())
            .context("sample entry size overflow")?;
        ensure!(
            bytes <= MAX_ENTRIES,
            "sample entries exceed {MAX_ENTRIES} bytes"
        );
    }
    Ok(())
}

fn validate_entry(track: &Track) -> Result<()> {
    let entry = &track.sample_entry;
    ensure!(
        entry.len() >= 16 && entry.len() <= MAX_ENTRY,
        "invalid sample entry length"
    );
    ensure!(
        be32(entry, 0)? as usize == entry.len() && be16(entry, 14)? == 1,
        "invalid sample entry header or data reference"
    );
    match track.kind {
        TrackKind::Video { width, height } => {
            ensure!(width > 0 && height > 0, "video dimensions must be positive");
            ensure!(
                entry.len() >= 86 && be16(entry, 32)? == width && be16(entry, 34)? == height,
                "video sample entry dimensions disagree"
            );
            let (config, minimum, length_at) = match &entry[4..8] {
                b"avc1" => (b"avcC", 7, 4),
                b"hvc1" => (b"hvcC", 23, 21),
                _ => bail!("video sample entry must be avc1 or hvc1"),
            };
            let atoms = &entry[86..];
            let data = find_box(atoms, config)?;
            ensure!(
                data.len() >= minimum && data[0] == 1 && data[length_at] & 3 == 3,
                "codec configuration must be version 1 with four-byte NAL lengths"
            );
            let colour = find_box(atoms, b"colr")?;
            ensure!(colour == rec709(), "only Rec. 709 nclc colour is supported");
            // SPS/PPS/VPS contents remain the producer's codec contract;
            // this module does not decode or reinterpret compressed media.
        }
        TrackKind::Audio {
            channels,
            sample_rate,
        } => {
            ensure!(
                sample_rate == MOVIE_TIMESCALE
                    && track.timescale == sample_rate
                    && (1..=2).contains(&channels),
                "AAC LC requires 48000 Hz and one or two channels"
            );
            // This version uses a generated, two-byte AAC-LC ASC. Reject a
            // description which advertises a different codec/rate/layout.
            let expected = aac_sample_entry(Container::Mp4, channels, sample_rate)?;
            if entry != &expected {
                ensure!(
                    entry == &aac_sample_entry(Container::Mov, channels, sample_rate)?,
                    "unsupported AAC sample description"
                );
            }
        }
    }
    Ok(())
}

fn track_end(track: &Track, samples: &[Sample]) -> Result<u64> {
    match samples.last() {
        Some(last) => Ok(add(last.time, u64::from(last.duration))?.saturating_sub(track.skip)),
        None => Ok(0),
    }
}

/// Round once at movie precision. u128 avoids multiplying u64 timestamps
/// by the movie timescale before division.
fn movie_ticks(ticks: u64, timescale: u32) -> Result<u64> {
    ensure!(timescale > 0, "zero timescale");
    let value = (u128::from(ticks) * u128::from(MOVIE_TIMESCALE) + u128::from(timescale / 2))
        / u128::from(timescale);
    u64::try_from(value).context("movie duration overflow")
}

// Box builders. Index callers bound their counts and byte sizes before
// allocating; these boxes are all below MAX_INDEX, hence use 32-bit sizes.

fn box_header(kind: &[u8; 4], size: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    if size > u64::from(u32::MAX) {
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

fn placeholder() -> [u8; PLACEHOLDER] {
    let mut out = [0u8; PLACEHOLDER];
    out[..4].copy_from_slice(&(PLACEHOLDER as u32).to_be_bytes());
    out[4..8].copy_from_slice(b"free");
    out
}

fn ftyp(container: Container) -> Vec<u8> {
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
            body.extend_from_slice(b"mp42mp41isomiso6");
        }
    }
    body.extend_from_slice(ISOCAST_BRAND);
    boxed(b"ftyp", &body)
}

fn moov(
    container: Container,
    tracks: &[Track],
    samples: Option<&[Vec<Sample>]>,
    created: u64,
) -> Result<Vec<u8>> {
    let mut duration = 0;
    let mut fragment_end = 0;
    if let Some(samples) = samples {
        ensure!(
            samples.len() == tracks.len(),
            "sample table track count changed"
        );
        let count = samples
            .iter()
            .try_fold(0usize, |count, track| count.checked_add(track.len()))
            .context("sample count overflow")?;
        ensure!(count <= MAX_SAMPLES, "sample index exceeds budget");
        // Worst case: duration, sync, chunk run, size and 64-bit offset
        // for every sample, plus bounded descriptions and track headers.
        let estimate = count
            .checked_mul(40)
            .and_then(|n| n.checked_add(MAX_ENTRIES + MAX_TRACKS * 1024))
            .context("index size overflow")?;
        ensure!(estimate <= MAX_INDEX, "final index exceeds byte budget");
        for (track, samples) in tracks.iter().zip(samples) {
            duration = duration.max(movie_ticks(track_end(track, samples)?, track.timescale)?);
            if let Some(last) = samples.last() {
                fragment_end = fragment_end.max(add(last.offset, u64::from(last.size))?);
            }
        }
        if fragment_end > 0 {
            // Sample ranges cover each complete mdat exactly. The last
            // sample's end is followed by the fixed-size commit trailer,
            // which is part of the committed prefix, not media payload.
            fragment_end = add(fragment_end, FRAGMENT_COMMIT as u64)?;
        }
    }
    let mut body = mvhd(duration, tracks.len() as u32 + 1, created);
    for (i, track) in tracks.iter().enumerate() {
        let samples = samples.map(|s| s[i].as_slice());
        if samples.is_some_and(<[Sample]>::is_empty) {
            continue;
        }
        body.extend(trak(container, i as u32 + 1, track, samples, created)?);
    }
    if samples.is_none() {
        let mut mvex = Vec::new();
        for i in 0..tracks.len() {
            let mut trex = (i as u32 + 1).to_be_bytes().to_vec();
            trex.extend_from_slice(&1u32.to_be_bytes());
            trex.extend_from_slice(&[0u8; 12]);
            mvex.extend(full(b"trex", 0, 0, &trex));
        }
        body.extend(boxed(b"mvex", &mvex));
    } else {
        let size = body.len() + 8 + FINAL_FOOTER;
        let mut footer = (size as u64).to_be_bytes().to_vec();
        footer.extend_from_slice(&fragment_end.to_be_bytes());
        body.extend(full(b"isfn", 1, 0, &footer));
    }
    ensure!(
        body.len() + 8 <= MAX_INDEX,
        "movie index exceeds byte budget"
    );
    Ok(boxed(b"moov", &body))
}

const MATRIX: [u32; 9] = [0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x4000_0000];

/// Seconds since the QuickTime/ISO epoch (1904-01-01).
pub fn movie_time(time: SystemTime) -> u64 {
    const UNIX_FROM_1904: u64 = 2_082_844_800;
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().saturating_add(UNIX_FROM_1904))
}

fn wide(duration: u64, created: u64) -> bool {
    duration > u64::from(u32::MAX) || created > u64::from(u32::MAX)
}

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
    body.extend_from_slice(&MOVIE_TIMESCALE.to_be_bytes());
    if v1 {
        body.extend_from_slice(&duration.to_be_bytes());
    } else {
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
    created: u64,
) -> Result<Vec<u8>> {
    let mut media_duration = 0;
    let mut first = 0;
    let mut presented = 0;
    if let Some(samples) = samples {
        first = samples.first().context("empty final track")?.time;
        let last = samples.last().context("empty final track")?;
        media_duration = add(last.time, u64::from(last.duration))?
            .checked_sub(first)
            .context("track time reversed")?;
        presented = movie_ticks(track_end(track, samples)?, track.timescale)?;
    }
    let mut body = tkhd(id, track, presented, created);
    let mut timing = track.skip.to_be_bytes().to_vec();
    timing.extend_from_slice(&track.fragment_shift.to_be_bytes());
    body.extend(full(b"isct", 1, 0, &timing));
    if samples.is_some() {
        // stts always begins at zero. Keep a late first DTS as an empty
        // edit, then skip only priming that precedes the real media start.
        let leading = movie_ticks(first.saturating_sub(track.skip), track.timescale)?;
        let skip = track.skip.saturating_sub(first);
        let active = presented
            .checked_sub(leading)
            .context("invalid track edit extent")?;
        let v1 = presented > u64::from(u32::MAX) || skip > i32::MAX as u64;
        let mut elst = (if leading > 0 { 2u32 } else { 1u32 })
            .to_be_bytes()
            .to_vec();
        if leading > 0 {
            edit(&mut elst, v1, leading, -1);
        }
        edit(&mut elst, v1, active, skip as i64);
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
    minf.extend(stbl(track, samples)?);
    mdia.extend(boxed(b"minf", &minf));
    body.extend(boxed(b"mdia", &mdia));
    Ok(boxed(b"trak", &body))
}

fn edit(body: &mut Vec<u8>, v1: bool, duration: u64, time: i64) {
    if v1 {
        body.extend_from_slice(&duration.to_be_bytes());
        body.extend_from_slice(&time.to_be_bytes());
    } else {
        body.extend_from_slice(&(duration as u32).to_be_bytes());
        body.extend_from_slice(&(time as i32).to_be_bytes());
    }
    body.extend_from_slice(&0x1_0000u32.to_be_bytes());
}

fn tkhd(id: u32, track: &Track, duration: u64, created: u64) -> Vec<u8> {
    let mut body = Vec::new();
    let v1 = wide(duration, created);
    times(&mut body, v1, created);
    body.extend_from_slice(&id.to_be_bytes());
    body.extend_from_slice(&[0u8; 4]);
    if v1 {
        body.extend_from_slice(&duration.to_be_bytes());
    } else {
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
    body.extend_from_slice(&timescale.to_be_bytes());
    if v1 {
        body.extend_from_slice(&duration.to_be_bytes());
    } else {
        body.extend_from_slice(&(duration as u32).to_be_bytes());
    }
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

fn stbl(track: &Track, samples: Option<&[Sample]>) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut stsd = 1u32.to_be_bytes().to_vec();
    stsd.extend_from_slice(&track.sample_entry);
    body.extend(full(b"stsd", 0, 0, &stsd));
    let samples = samples.unwrap_or(&[]);
    // AAC roll -1 lets readers decode the preceding packet at an edit.
    if matches!(track.kind, TrackKind::Audio { .. }) && track.skip > 0 && !samples.is_empty() {
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
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for sample in samples {
        match runs.last_mut() {
            Some((count, duration)) if *duration == sample.duration => *count += 1,
            _ => runs.push((1, sample.duration)),
        }
    }
    let mut stts = (runs.len() as u32).to_be_bytes().to_vec();
    for (count, duration) in runs {
        stts.extend_from_slice(&count.to_be_bytes());
        stts.extend_from_slice(&duration.to_be_bytes());
    }
    body.extend(full(b"stts", 0, 0, &stts));
    if matches!(track.kind, TrackKind::Video { .. }) && samples.iter().any(|s| !s.sync) {
        let count = samples.iter().filter(|s| s.sync).count();
        let mut stss = (count as u32).to_be_bytes().to_vec();
        for (i, sample) in samples.iter().enumerate() {
            if sample.sync {
                stss.extend_from_slice(&(i as u32 + 1).to_be_bytes());
            }
        }
        body.extend(full(b"stss", 0, 0, &stss));
    }
    let mut chunks: Vec<(u64, u32)> = Vec::new();
    let mut expected = None;
    for sample in samples {
        if expected == Some(sample.offset) {
            let last = chunks.last_mut().context("missing sample chunk")?;
            last.1 += 1;
        } else {
            chunks.push((sample.offset, 1));
        }
        expected = Some(add(sample.offset, u64::from(sample.size))?);
    }
    let mut stsc_runs: Vec<(u32, u32)> = Vec::new();
    for (i, (_, count)) in chunks.iter().enumerate() {
        match stsc_runs.last() {
            Some((_, last)) if last == count => {}
            _ => stsc_runs.push((i as u32 + 1, *count)),
        }
    }
    let mut stsc = (stsc_runs.len() as u32).to_be_bytes().to_vec();
    for (first, count) in stsc_runs {
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
    if chunks
        .iter()
        .all(|(offset, _)| *offset <= u64::from(u32::MAX))
    {
        let mut stco = (chunks.len() as u32).to_be_bytes().to_vec();
        for (offset, _) in chunks {
            stco.extend_from_slice(&(offset as u32).to_be_bytes());
        }
        body.extend(full(b"stco", 0, 0, &stco));
    } else {
        let mut co64 = (chunks.len() as u32).to_be_bytes().to_vec();
        for (offset, _) in chunks {
            co64.extend_from_slice(&offset.to_be_bytes());
        }
        body.extend(full(b"co64", 0, 0, &co64));
    }
    Ok(boxed(b"stbl", &body))
}

fn sample_flags(sync: bool) -> u32 {
    if sync { 0x0200_0000 } else { 0x0101_0000 }
}

fn moof(
    sequence: u32,
    tracks: &[Track],
    parts: &[FragmentTrack<'_>],
    data_start: u64,
) -> Result<Vec<u8>> {
    let mut body = full(b"mfhd", 0, 0, &sequence.to_be_bytes());
    let mut data_offset = data_start;
    for (i, part) in parts.iter().enumerate() {
        if part.samples.is_empty() {
            continue;
        }
        let track = &tracks[i];
        let video = matches!(track.kind, TrackKind::Video { .. });
        let mut traf = Vec::new();
        let mut tfhd = (i as u32 + 1).to_be_bytes().to_vec();
        tfhd.extend_from_slice(&1u32.to_be_bytes());
        traf.extend(full(b"tfhd", 0, 0x2_0002, &tfhd));
        let first_time = add(part.samples[0].1, track.fragment_shift)?;
        traf.extend(full(b"tfdt", 1, 0, &first_time.to_be_bytes()));
        let flags = if video { 0x701 } else { 0x301 };
        let mut trun = (part.samples.len() as u32).to_be_bytes().to_vec();
        let offset = i32::try_from(data_offset).context("fragment data offset exceeds i32")?;
        trun.extend_from_slice(&offset.to_be_bytes());
        for (j, &(size, time, duration, sync)) in part.samples.iter().enumerate() {
            let duration = match part.samples.get(j + 1) {
                Some(next) => next_duration(time, next.1)?,
                None => duration,
            };
            trun.extend_from_slice(&duration.to_be_bytes());
            trun.extend_from_slice(&size.to_be_bytes());
            if video {
                trun.extend_from_slice(&sample_flags(sync).to_be_bytes());
            }
        }
        traf.extend(full(b"trun", 0, flags, &trun));
        body.extend(boxed(b"traf", &traf));
        data_offset = add(data_offset, part.data.len() as u64)?;
    }
    ensure!(
        body.len() + 8 <= MAX_INDEX,
        "fragment index exceeds byte budget"
    );
    Ok(boxed(b"moof", &body))
}

fn rec709() -> Vec<u8> {
    let mut colour = b"nclc".to_vec();
    for _ in 0..3 {
        colour.extend_from_slice(&1u16.to_be_bytes());
    }
    colour
}

/// Four-byte length-prefixed avc1/hvc1, with producer-supplied avcC/hvcC
/// and Rec. 709 tags. This does not convert Annex B or inspect pictures.
pub fn video_sample_entry(
    fourcc: [u8; 4],
    width: u16,
    height: u16,
    depth: u16,
    name: &str,
    atoms: &[([u8; 4], Vec<u8>)],
) -> Result<Vec<u8>> {
    ensure!(
        atoms.len() < MAX_CHILDREN,
        "too many video configuration atoms"
    );
    let mut bytes = 86usize;
    for (_, data) in atoms {
        bytes = bytes
            .checked_add(data.len())
            .and_then(|n| n.checked_add(8))
            .context("video entry overflow")?;
    }
    ensure!(
        bytes <= MAX_ENTRY - 18,
        "video sample entry exceeds byte budget"
    );
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
        body.extend(boxed(b"colr", &rec709()));
    }
    let entry = boxed(&fourcc, &body);
    let track = Track {
        kind: TrackKind::Video { width, height },
        timescale: 1,
        sample_entry: entry,
        skip: 0,
        fragment_shift: 0,
    };
    validate_entry(&track)?;
    Ok(track.sample_entry)
}

/// AAC LC only: generated ASC for 48000 Hz, mono or stereo. The producer
/// supplies priming separately in Track; no Apple encoder delay is assumed.
pub fn aac_sample_entry(container: Container, channels: u16, sample_rate: u32) -> Result<Vec<u8>> {
    ensure!(
        sample_rate == 48_000 && (1..=2).contains(&channels),
        "AAC LC requires 48000 Hz and one or two channels"
    );
    let mut body = Vec::new();
    body.extend_from_slice(&[0u8; 6]);
    body.extend_from_slice(&1u16.to_be_bytes());
    let esds = esds(channels);
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
    Ok(boxed(b"mp4a", &body))
}

fn esds(channels: u16) -> Vec<u8> {
    // AudioSpecificConfig: AAC LC (2), 48000 Hz (3), channel layout.
    let config = ((2u16 << 11) | (3u16 << 7) | (channels << 3)).to_be_bytes();
    let descriptor = |tag: u8, body: &[u8]| {
        let mut out = vec![tag, 0x80, 0x80, 0x80, body.len() as u8];
        out.extend_from_slice(body);
        out
    };
    let specific = descriptor(0x05, &config);
    let mut decoder = vec![0x40, 0x15, 0, 0, 0];
    let bitrate: u32 = 128_000 * u32::from(channels);
    decoder.extend_from_slice(&bitrate.to_be_bytes());
    decoder.extend_from_slice(&bitrate.to_be_bytes());
    decoder.extend(specific);
    let decoder = descriptor(0x04, &decoder);
    let mut es = vec![0, 0, 0];
    es.extend(decoder);
    es.extend(descriptor(0x06, &[0x02]));
    full(b"esds", 0, 0, &descriptor(0x03, &es))
}
