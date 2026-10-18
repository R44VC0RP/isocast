//! FileWriter contracts exercised through the same Platform API on every OS.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail, ensure};
use isocast_engine::synthetic::{TestPattern, Tone};
use isocast_engine::{
    AudioTrackSpec, FileWriter, FramePool, Platform, SAMPLE_RATE, VideoTrackSpec, WriterSpec,
};
use isocast_protocol::{Codec, Container, Encoder, EncoderQuality};
use serde_json::{Value, json};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;
const FPS: u32 = 30;
const AUDIO_TRACKS: usize = 2;
const AUDIO_FRAMES: u64 = (SAMPLE_RATE / FPS) as u64;

fn spec(path: &Path) -> WriterSpec {
    WriterSpec {
        path: path.to_owned(),
        container: Container::Mp4,
        video: VideoTrackSpec {
            width: WIDTH,
            height: HEIGHT,
            fps: FPS,
            encoder: Encoder {
                codec: Codec::H264,
                quality: EncoderQuality::Value(70),
                container: Container::Mp4,
                bit_depth: 8,
            },
        },
        audio: vec![
            AudioTrackSpec {
                channels: Tone::CHANNELS,
            };
            AUDIO_TRACKS
        ],
        created: SystemTime::now(),
    }
}

fn require_absent(path: &Path) -> Result<()> {
    match path.symlink_metadata() {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
        Ok(_) => bail!("{} already exists; refusing to change it", path.display()),
    }
}

fn drain_lost<W: FileWriter>(writer: &mut W, lost: &mut u64) -> Result<()> {
    *lost = lost
        .checked_add(writer.lost_frames())
        .context("encoder_lost overflow")?;
    Ok(())
}

/// Record one fresh file, or emit a commit receipt and keep its writer alive
/// until the parent kills this worker. The parent probes and decodes the file.
pub fn record<P: Platform>(platform: &P, folder: &Path, mode: &str) -> Result<Value> {
    let frames: u64 = match mode {
        "normal" => 90,
        "pressure" => 120,
        "commit" => 30,
        _ => bail!("mode: expected normal, pressure, or commit; got {mode:?}"),
    };
    let path = folder.join("take.mp4");
    require_absent(&path)?;
    let mut pool = platform.frame_pool(WIDTH, HEIGHT)?;
    let pattern = TestPattern::new("contracts", WIDTH, HEIGHT, FPS);
    let tones = [Tone::new(440), Tone::new(880)];
    let mut samples = vec![0.0; AUDIO_FRAMES as usize * Tone::CHANNELS as usize];
    let mut writer = platform.create_writer(&spec(&path))?;
    let crash_safe = writer.crash_safe();
    if mode == "commit" {
        ensure!(crash_safe, "commit: writer does not advertise crash_safe");
    }

    let origin = platform.now() + Duration::from_millis(200);
    ensure!(origin.as_nanos() != 0, "timeline origin must be nonzero");
    let end = origin.add_ticks(frames, FPS);
    // start-before-append is a caller precondition, not a promise that invalid
    // appends return Err (the native implementations may panic before start).
    writer.start(origin)?;
    let mut accepted = 0u64;
    let mut refused = 0u64;
    let mut lost = 0u64;
    let mut video_first = None;
    let mut video_end = None;
    let mut audio_accepted = [0u64; AUDIO_TRACKS];
    let mut audio_refused = [0u64; AUDIO_TRACKS];
    let mut audio_first = [None; AUDIO_TRACKS];
    let mut audio_end = [None; AUDIO_TRACKS];
    let mut pool_replacements = 0u64;

    for frame in 0..frames {
        let pts = origin.add_ticks(frame, FPS);
        if mode != "pressure" {
            platform.sleep_until(pts);
        }
        let mut draw = |pixels: &mut [u8], stride| {
            pattern.draw(frame, pts.saturating_since(origin), pixels, stride);
        };
        let picture = match pool.draw(&mut draw)? {
            Some(picture) => picture,
            None => {
                // This is writer pressure, not a pool exhaustion test: every
                // slot must attempt append_video. Replace a full pool instead
                // of waiting or misreporting its exhaustion as writer refusal.
                // At most this run's 120 small frames can be retained.
                pool = platform.frame_pool(WIDTH, HEIGHT)?;
                pool_replacements += 1;
                pool.draw(&mut draw)?
                    .context("a fresh frame pool refused its first frame")?
            }
        };
        if writer.append_video(&picture, pts)? {
            accepted += 1;
            video_first.get_or_insert(pts);
            video_end = Some(origin.add_ticks(frame + 1, FPS));
        } else {
            refused += 1;
        }
        drain_lost(&mut writer, &mut lost)?;

        let first_sample = frame * AUDIO_FRAMES;
        let audio_pts = origin.add_ticks(first_sample, SAMPLE_RATE);
        for (track, tone) in tones.iter().enumerate() {
            tone.fill(first_sample, &mut samples);
            if writer.append_audio(track, &samples, audio_pts)? {
                audio_accepted[track] += 1;
                audio_first[track].get_or_insert(audio_pts);
                audio_end[track] = Some(origin.add_ticks(first_sample + AUDIO_FRAMES, SAMPLE_RATE));
            } else {
                audio_refused[track] += 1;
            }
            drain_lost(&mut writer, &mut lost)?;
        }
    }
    if mode != "pressure" {
        platform.sleep_until(end);
    }
    drain_lost(&mut writer, &mut lost)?;
    let audio_attempted = [frames; AUDIO_TRACKS];
    let mut result = json!({
        "path": path,
        "mode": mode,
        "attempted": frames,
        "accepted": accepted,
        "refused": refused,
        "encoder_lost": lost,
        // Only finish establishes these; a commit must not invent them.
        "repeated": null,
        "expected_frames": null,
        "audio_lost_tracks": null,
        "audio_attempted": audio_attempted,
        "audio_accepted": audio_accepted,
        "audio_refused": audio_refused,
        "audio_accepted_sample_frames": audio_accepted.map(|n| n * AUDIO_FRAMES),
        "audio_refused_sample_frames": audio_refused.map(|n| n * AUDIO_FRAMES),
        "audio_sample_frames_per_append": AUDIO_FRAMES,
        "video_first_accepted_pts_ns": video_first.map(|t| t.as_nanos()),
        "video_accepted_end_ns": video_end.map(|t| t.as_nanos()),
        "audio_first_accepted_pts_ns": audio_first.map(|t| t.map(|t| t.as_nanos())),
        "audio_accepted_end_ns": audio_end.map(|t| t.map(|t| t.as_nanos())),
        "timeline_origin_ns": origin.as_nanos(),
        "timeline_end_ns": end.as_nanos(),
        "duration_seconds": end.saturating_since(origin).as_secs_f64(),
        "codec": "h264",
        "container": "mp4",
        "fps": FPS,
        "width": WIDTH,
        "height": HEIGHT,
        "audio_codec": "aac",
        "audio_tracks": AUDIO_TRACKS,
        "audio_sample_rate": SAMPLE_RATE,
        "audio_channels": Tone::CHANNELS,
        "crash_safe": crash_safe,
        "pool_replacements": pool_replacements,
    });

    if mode == "commit" {
        let before = writer.committed();
        let promised = writer
            .commit()?
            .context("crash_safe commit returned None")?;
        let reported = writer
            .committed()
            .context("crash_safe committed returned None after commit")?;
        drain_lost(&mut writer, &mut lost)?;
        ensure!(lost <= accepted, "commit: encoder lost more than accepted");
        // An upper bound from actual accepted inputs, not an AAC priming or
        // packet-size assumption. The parent's recovery checks the on-disk
        // track ends against the positive promise, even when it is < 1 s.
        let delivered_end = video_end
            .into_iter()
            .chain(audio_end.into_iter().flatten())
            .min()
            .context("commit: no accepted media")?;
        ensure!(
            video_first.is_some() && audio_first.iter().all(Option::is_some),
            "commit: not every track accepted media"
        );
        ensure!(
            !promised.is_zero(),
            "commit: no recoverable duration promised"
        );
        ensure!(
            reported >= promised,
            "committed() is behind commit() promise"
        );
        ensure!(
            before.is_none_or(|before| promised >= before),
            "commit() regressed the previous committed() promise"
        );
        ensure!(
            reported <= delivered_end.saturating_since(origin),
            "committed() exceeds the shortest track's accepted input end"
        );
        result["encoder_lost"] = json!(lost);
        result["committed_seconds"] = json!(promised.as_secs_f64());
        result["reported_committed_seconds"] = json!(reported.as_secs_f64());
        result["delivered_end_seconds"] =
            json!(delivered_end.saturating_since(origin).as_secs_f64());
        let mut stdout = io::stdout().lock();
        serde_json::to_writer(&mut stdout, &json!({ "checkpoint": result }))?;
        writeln!(stdout)?;
        stdout.flush()?;
        drop(stdout);
        // Do not finish, cancel, drop or forget the writer: only the parent's
        // bounded process kill may release its ownership of this live file.
        loop {
            std::hint::black_box(&writer);
            std::thread::park();
        }
    }

    let finished = writer.finish(end)?;
    lost = lost
        .checked_add(finished.lost_frames)
        .context("encoder_lost overflow at finish")?;
    let expected_frames = accepted
        .checked_sub(lost)
        .context("encoder lost more frames than append_video accepted")?
        .checked_add(finished.repeated)
        .context("expected_frames overflow")?;
    result["encoder_lost"] = json!(lost);
    result["repeated"] = json!(finished.repeated);
    result["expected_frames"] = json!(expected_frames);
    result["audio_lost_tracks"] = json!(finished.lost_audio_tracks);
    let metadata = fs::metadata(&path).context("finished file does not exist")?;
    ensure!(
        metadata.is_file() && metadata.len() > 0,
        "finished file is not a nonempty regular file"
    );
    result["file_bytes"] = json!(metadata.len());
    if mode == "normal" {
        ensure!(
            refused == 0 && lost == 0 && finished.repeated == 0,
            "idle 320x180@30 qualification lost original pictures: {result}"
        );
        ensure!(
            audio_refused == [0; AUDIO_TRACKS] && finished.lost_audio_tracks == 0,
            "idle 320x180@30 qualification lost audio: {result}"
        );
    }
    Ok(result)
}

/// Check exclusive creation and cancellation at unmodified, freshly owned
/// paths. This does not claim protection against replacing a live file's path.
pub fn ownership<P: Platform>(platform: &P, folder: &Path) -> Result<Value> {
    let path = folder.join("owned.mp4");
    let sentinel_path = folder.join("existing.mp4");
    require_absent(&path)?;
    require_absent(&sentinel_path)?;
    let owned_spec = spec(&path);
    let writer = platform.create_writer(&owned_spec)?;
    let before = fs::read(&path).context("created writer did not create its file")?;
    let duplicate = platform.create_writer(&owned_spec);
    let unchanged = fs::read(&path)? == before;
    // Never cancel an unexpectedly successful duplicate: that could remove
    // the first writer's evidence. Error paths leave the files in place.
    ensure!(
        unchanged,
        "duplicate create changed the active writer's file"
    );
    let duplicate_error = match duplicate {
        Err(error) => format!("{error:#}"),
        Ok(_) => bail!("duplicate create succeeded at an active writer's path"),
    };
    writer.cancel();
    require_absent(&path).context("cancel did not remove its own empty file")?;
    let recreated = platform.create_writer(&owned_spec)?;
    ensure!(
        path.is_file(),
        "fresh create after cancel did not create a file"
    );
    recreated.cancel();
    require_absent(&path).context("recreated writer did not cancel its own file")?;

    let sentinel = b"platform contracts: this existing file must not be overwritten\n";
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&sentinel_path)?;
    file.write_all(sentinel)?;
    file.sync_all()?;
    drop(file);
    let existing = platform.create_writer(&spec(&sentinel_path));
    ensure!(
        fs::read(&sentinel_path)? == sentinel.as_slice(),
        "create_writer changed the existing sentinel file"
    );
    let existing_error = match existing {
        Err(error) => format!("{error:#}"),
        Ok(_) => bail!("create_writer accepted an existing sentinel file"),
    };
    Ok(json!({
        "path": path,
        "duplicate_create_refused": true,
        "duplicate_bytes_unchanged": true,
        "duplicate_error": duplicate_error,
        "cancelled_path_removed": true,
        "fresh_create_after_cancel": true,
        "recreated_cancel_removed": true,
        "sentinel_path": sentinel_path,
        "existing_file_refused": true,
        "sentinel_bytes_unchanged": true,
        "existing_error": existing_error,
    }))
}
