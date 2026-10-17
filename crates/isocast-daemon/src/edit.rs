//! The Edit view's edits (C.3): trim, split, ripple delete and angle
//! selection on a timeline's clips, crops and rolls (#87), and undo/redo. Each edit changes only
//! the timeline (its clips' in and out points, order and angles) and is one
//! revision; the takes' files are never touched.
//!
//! Every point is kept on its take's frame grid (the program's frame rate,
//! [`take_length`]): a time on the timeline is rounded to the nearest frame
//! of the clip there, so cuts always fall between two frames of the
//! program.

use std::collections::HashMap;
use std::path::PathBuf;

use isocast_protocol::PROGRAM;
use isocast_protocol::api::RpcError;
use isocast_protocol::timeline::{
    Clip, Crop, RemovedReason, RemovedTake, Revision, Timeline, TimelineAction,
    TimelineAngleParams, TimelineCropParams, TimelineInfo, TimelineRangeParams, TimelineRollParams,
    TimelineSplitParams, TimelineTarget, TimelineTrimParams, UNDO_LIMIT, fit_aspect, take_length,
};

use crate::Daemon;
use crate::timeline::{invalid, now_text, read_manifest, wrong_state};

/// What edits need to know of a take: its frame rate and length in frames
/// (the program's grid), and the sources it recorded.
#[derive(Debug, Clone)]
pub struct TakeGrid {
    pub fps: u32,
    pub frames: i64,
    pub sources: Vec<String>,
    /// Each source's picture size, in the order of `sources`.
    pub sizes: Vec<(u32, u32)>,
}

impl TakeGrid {
    /// The picture size of `source` (the program's, else the first's).
    fn size(&self, source: &str) -> (u32, u32) {
        self.sources
            .iter()
            .position(|s| s == source)
            .or_else(|| self.sources.iter().position(|s| s == PROGRAM))
            .and_then(|i| self.sizes.get(i).copied())
            .unwrap_or((1920, 1080))
    }
}

/// Each clip's take's grid, by manifest path.
pub type Grids = HashMap<PathBuf, TakeGrid>;

/// The grids of `timeline`'s takes, read from their manifests; an error
/// names a clip whose take can't be read.
pub fn grids(timeline: &Timeline) -> Result<Grids, String> {
    let mut grids = Grids::new();
    for clip in &timeline.clips {
        if grids.contains_key(&clip.take) {
            continue;
        }
        let manifest =
            read_manifest(&clip.take).map_err(|err| format!("clip {}: {err}", clip.id))?;
        let (frames, fps) = take_length(&manifest).ok_or_else(|| {
            format!(
                "clip {}: its take {} has no finished file, so it has no length to edit",
                clip.id,
                clip.take.display()
            )
        })?;
        grids.insert(
            clip.take.clone(),
            TakeGrid {
                fps,
                frames: frames as i64,
                sources: manifest.files.iter().map(|f| f.source.clone()).collect(),
                sizes: manifest.files.iter().map(|f| (f.width, f.height)).collect(),
            },
        );
    }
    Ok(grids)
}

fn frame(seconds: f64, fps: u32) -> i64 {
    (seconds * f64::from(fps)).round() as i64
}

fn seconds(frame: i64, fps: u32) -> f64 {
    frame as f64 / f64::from(fps)
}

/// A clip's in and out points in frames of its take.
fn span(clip: &Clip, grid: &TakeGrid) -> (i64, i64) {
    (
        frame(clip.in_seconds, grid.fps),
        frame(clip.out_seconds, grid.fps),
    )
}

fn grid<'a>(grids: &'a Grids, clip: &Clip) -> &'a TakeGrid {
    grids.get(&clip.take).expect("every clip's grid is read")
}

/// Where a time on the timeline falls, rounded to a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Point {
    /// Between clips: before clip `index` (the number of clips: the end).
    Boundary(usize),
    /// Inside clip `index`, `frames` into it (never 0 or its length).
    Inside(usize, i64),
}

fn locate(timeline: &Timeline, grids: &Grids, at: f64) -> Point {
    let mut start = 0.0;
    for (index, clip) in timeline.clips.iter().enumerate() {
        let grid = grid(grids, clip);
        let (a, b) = span(clip, grid);
        let length = seconds(b - a, grid.fps);
        if at < start + length {
            let k = frame(at - start, grid.fps).clamp(0, b - a);
            return match k {
                0 => Point::Boundary(index),
                k if k == b - a => Point::Boundary(index + 1),
                k => Point::Inside(index, k),
            };
        }
        start += length;
    }
    Point::Boundary(timeline.clips.len())
}

/// Cut the clip at `at` (if it isn't already a boundary) and return the
/// boundary's index: the clip after the cut.
fn cut(timeline: &mut Timeline, grids: &Grids, at: f64) -> usize {
    match locate(timeline, grids, at) {
        Point::Boundary(index) => index,
        Point::Inside(index, k) => {
            let fps = grid(grids, &timeline.clips[index]).fps;
            let (a, b) = span(&timeline.clips[index], grid(grids, &timeline.clips[index]));
            let mut second = timeline.clips[index].clone();
            second.id = format!("c{}", timeline.next_clip);
            timeline.next_clip += 1;
            second.in_seconds = seconds(a + k, fps);
            second.out_seconds = seconds(b, fps);
            timeline.clips[index].in_seconds = seconds(a, fps);
            timeline.clips[index].out_seconds = seconds(a + k, fps);
            timeline.clips.insert(index + 1, second);
            index + 1
        }
    }
}

/// The timeline's length, for messages.
fn total(timeline: &Timeline) -> f64 {
    isocast_protocol::timeline::duration(&timeline.clips)
}

fn check_time(field: &str, value: f64, timeline: &Timeline) -> Result<(), String> {
    let total = total(timeline);
    if !value.is_finite() || value < 0.0 || value > total + 1e-6 {
        return Err(format!(
            "{field}: {value} is outside the timeline, which runs from 0 to {total:.3} s"
        ));
    }
    Ok(())
}

/// The range's two boundaries (cutting clips there); an error names the
/// field at fault.
fn range(
    timeline: &mut Timeline,
    grids: &Grids,
    start: f64,
    end: f64,
) -> Result<(usize, usize), String> {
    if timeline.clips.is_empty() {
        return Err(format!("timeline {:?} has no clips yet", timeline.id));
    }
    check_time("start_seconds", start, timeline)?;
    check_time("end_seconds", end, timeline)?;
    if end <= start {
        return Err(format!(
            "end_seconds: {end} must be after start_seconds ({start})"
        ));
    }
    // Cutting at the start doesn't move what comes after it.
    let first = cut(timeline, grids, start);
    let last = cut(timeline, grids, end);
    if last <= first {
        return Err(format!(
            "start_seconds, end_seconds: {start}–{end} s is less than a frame once rounded to \
             the frames there; choose a longer span"
        ));
    }
    Ok((first, last))
}

/// `timeline_trim`: the clip's new points, on its take's frame grid.
pub fn trim(
    timeline: &mut Timeline,
    grids: &Grids,
    params: &TimelineTrimParams,
) -> Result<(), String> {
    let index = crate::timeline::find(timeline, &params.clip).map_err(|err| err.message)?;
    let clip = &timeline.clips[index];
    let grid = grid(grids, clip).clone();
    let (mut a, mut b) = span(clip, &grid);
    let length = seconds(grid.frames, grid.fps);
    for (field, value) in [
        ("in_seconds", params.in_seconds),
        ("out_seconds", params.out_seconds),
    ] {
        if let Some(value) = value
            && (!value.is_finite() || value < 0.0 || value > length + 1e-6)
        {
            return Err(format!(
                "{field}: {value} is outside clip {}'s take, which runs from 0 to {length:.3} s",
                params.clip
            ));
        }
    }
    if params.in_seconds.is_none() && params.out_seconds.is_none() {
        return Err("in_seconds, out_seconds: give one or both".into());
    }
    if let Some(value) = params.in_seconds {
        a = frame(value, grid.fps);
    }
    if let Some(value) = params.out_seconds {
        b = frame(value, grid.fps).min(grid.frames);
    }
    if b <= a {
        return Err(format!(
            "in_seconds, out_seconds: clip {} would run from {:.3} to {:.3} s of its take; the \
             out point must be at least one frame ({:.4} s) after the in point",
            params.clip,
            seconds(a, grid.fps),
            seconds(b, grid.fps),
            1.0 / f64::from(grid.fps)
        ));
    }
    let clip = &mut timeline.clips[index];
    clip.in_seconds = seconds(a, grid.fps);
    clip.out_seconds = seconds(b, grid.fps);
    Ok(())
}

/// `timeline_split`: returns the new clip's id, or `None` if `at` was
/// already between two clips.
pub fn split(
    timeline: &mut Timeline,
    grids: &Grids,
    params: &TimelineSplitParams,
) -> Result<Option<String>, String> {
    if timeline.clips.is_empty() {
        return Err(format!("timeline {:?} has no clips yet", timeline.id));
    }
    check_time("at_seconds", params.at_seconds, timeline)?;
    let before = timeline.clips.len();
    let index = cut(timeline, grids, params.at_seconds);
    Ok((timeline.clips.len() > before).then(|| timeline.clips[index].id.clone()))
}

/// `timeline_delete_range`: returns the clips taken off whole.
pub fn delete_range(
    timeline: &mut Timeline,
    grids: &Grids,
    params: &TimelineRangeParams,
) -> Result<Vec<String>, String> {
    let (first, last) = range(timeline, grids, params.start_seconds, params.end_seconds)?;
    let gone: Vec<Clip> = timeline.clips.drain(first..last).collect();
    for clip in &gone {
        // A take with no clip left is listed with the takes taken off.
        let still = timeline.clips.iter().any(|c| c.take == clip.take);
        let listed = timeline.removed.iter().any(|r| r.take == clip.take);
        if !still && !listed {
            timeline.removed.push(RemovedTake {
                take: clip.take.clone(),
                clip: Some(clip.id.clone()),
                reason: RemovedReason::Removed,
                at: now_text(),
            });
        }
    }
    Ok(gone.into_iter().map(|clip| clip.id).collect())
}

/// `timeline_angle`: returns the clips that now show `source`.
pub fn angle(
    timeline: &mut Timeline,
    grids: &Grids,
    params: &TimelineAngleParams,
) -> Result<Vec<String>, String> {
    let source = params.source.trim();
    // The clips the span covers, before any is cut, so errors name them.
    let mut covered: Vec<&Clip> = Vec::new();
    let mut start = 0.0;
    for clip in &timeline.clips {
        let end = start + clip.duration_seconds();
        if start < params.end_seconds && end > params.start_seconds {
            covered.push(clip);
        }
        start = end;
    }
    if source != PROGRAM && !covered.is_empty() {
        let missing: Vec<&str> = covered
            .iter()
            .filter(|clip| !grid(grids, clip).sources.iter().any(|s| s == source))
            .map(|clip| clip.id.as_str())
            .collect();
        if !missing.is_empty() {
            // The sources every clip in the span recorded.
            let mut common: Vec<String> = grid(grids, covered[0]).sources.clone();
            for clip in &covered {
                let sources = &grid(grids, clip).sources;
                common.retain(|s| sources.contains(s));
            }
            if !common.iter().any(|s| s == PROGRAM) {
                common.insert(0, PROGRAM.into());
            }
            return Err(format!(
                "source: {source:?} isn't recorded in clip{} {} in that span; choose one of: {}",
                if missing.len() == 1 { "" } else { "s" },
                missing.join(", "),
                common.join(", ")
            ));
        }
    }
    let (first, last) = range(timeline, grids, params.start_seconds, params.end_seconds)?;
    let mut changed = Vec::new();
    for clip in &mut timeline.clips[first..last] {
        let was = clip.angle.clone().unwrap_or_else(|| PROGRAM.to_string());
        clip.angle = (source != PROGRAM).then(|| source.to_string());
        // A crop stays on the new angle's 2-pixel grid (#94).
        let sizes = (grid(grids, clip).size(&was), grid(grids, clip).size(source));
        if let Some(crop) = clip.crop
            && sizes.0 != sizes.1
        {
            clip.crop = snap_crop(crop, sizes.1).map_err(|err| {
                format!(
                    "clip {}: its crop on {source:?}'s {}×{} picture: {err}; clear or enlarge the \
                     crop first",
                    clip.id, sizes.1.0, sizes.1.1
                )
            })?;
        }
        changed.push(clip.id.clone());
    }
    Ok(changed)
}

/// The smallest crop, in pixels each way.
const MIN_CROP_PIXELS: f64 = 32.0;

/// `crop` on a `width`×`height` picture, checked (fields named) and moved
/// to the picture's 2-pixel grid; `None` for the whole picture.
fn snap_crop(crop: Crop, (width, height): (u32, u32)) -> Result<Option<Crop>, String> {
    for (field, value) in [
        ("crop.x", crop.x),
        ("crop.y", crop.y),
        ("crop.width", crop.width),
        ("crop.height", crop.height),
    ] {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            return Err(format!(
                "{field}: {value} is outside the picture; crops are fractions of its width and \
                 height, from 0 to 1"
            ));
        }
    }
    if crop.x + crop.width > 1.0 + 1e-6 {
        return Err(format!(
            "crop.x, crop.width: {} + {} runs past the picture's right edge (at most 1)",
            crop.x, crop.width
        ));
    }
    if crop.y + crop.height > 1.0 + 1e-6 {
        return Err(format!(
            "crop.y, crop.height: {} + {} runs past the picture's bottom edge (at most 1)",
            crop.y, crop.height
        ));
    }
    let (w, h) = (f64::from(width.max(2)), f64::from(height.max(2)));
    // Even pixels (4:2:0 pictures), inside the picture.
    let even = |v: f64, size: f64| ((v * size / 2.0).round() * 2.0).clamp(0.0, size);
    let (x0, y0) = (even(crop.x, w), even(crop.y, h));
    let (x1, y1) = (even(crop.x + crop.width, w), even(crop.y + crop.height, h));
    if x1 - x0 < MIN_CROP_PIXELS || y1 - y0 < MIN_CROP_PIXELS {
        return Err(format!(
            "crop.width, crop.height: {:.0}×{:.0} pixels of a {width}×{height} picture is too \
             small; a crop is at least {MIN_CROP_PIXELS:.0} pixels each way",
            x1 - x0,
            y1 - y0
        ));
    }
    let snapped = Crop {
        x: x0 / w,
        y: y0 / h,
        width: (x1 - x0) / w,
        height: (y1 - y0) / h,
    };
    Ok((!snapped.is_full()).then_some(snapped))
}

/// `timeline_crop`: returns the clips whose crop was set (or cleared).
pub fn crop(
    timeline: &mut Timeline,
    grids: &Grids,
    params: &TimelineCropParams,
) -> Result<Vec<String>, String> {
    if params.clear && (params.crop.is_some() || params.preset.is_some()) {
        return Err("clear: give clear alone, or a crop and/or preset".into());
    }
    if !params.clear && params.crop.is_none() && params.preset.is_none() {
        return Err(
            "crop, preset, clear: give a crop (x, y, width, height), a preset (16:9, 9:16, 1:1, \
             4:5), or clear"
                .into(),
        );
    }
    // Check the rectangle before cutting anything.
    if let Some(rect) = params.crop {
        snap_crop(rect, (1 << 16, 1 << 16))?;
    }
    let (first, last) = range(timeline, grids, params.start_seconds, params.end_seconds)?;
    let mut changed = Vec::new();
    for index in first..last {
        let clip = &timeline.clips[index];
        let source = clip.angle.clone().unwrap_or_else(|| PROGRAM.to_string());
        let size = grid(grids, clip).size(&source);
        let value = match params.clear {
            true => None,
            false => {
                let mut rect = params.crop.unwrap_or(Crop::FULL);
                if let Some(preset) = params.preset {
                    rect = fit_aspect(rect, preset.aspect(), size);
                }
                snap_crop(rect, size).map_err(|err| format!("clip {}: {err}", clip.id))?
            }
        };
        let clip = &mut timeline.clips[index];
        clip.crop = value;
        changed.push(clip.id.clone());
    }
    Ok(changed)
}

/// `timeline_roll`: moves the cut after `params.clip`; returns the clip
/// after it.
pub fn roll(
    timeline: &mut Timeline,
    grids: &Grids,
    params: &TimelineRollParams,
) -> Result<String, String> {
    let index = crate::timeline::find(timeline, &params.clip).map_err(|err| err.message)?;
    if index + 1 >= timeline.clips.len() {
        return Err(format!(
            "clip: {} is the last clip, so there is no cut after it to move; choose one of: {}",
            params.clip,
            timeline.clips[..timeline.clips.len() - 1]
                .iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !params.by_seconds.is_finite() {
        return Err(format!("by_seconds: {} isn't a number", params.by_seconds));
    }
    let (a, b) = (&timeline.clips[index], &timeline.clips[index + 1]);
    let (grid_a, grid_b) = (grid(grids, a).clone(), grid(grids, b).clone());
    let (a_in, a_out) = span(a, &grid_a);
    let (b_in, b_out) = span(b, &grid_b);
    // Both clips move by the same time, on the first's frame grid; the
    // second's must land on its own grid too.
    let frames_a = frame(params.by_seconds, grid_a.fps);
    let moved = seconds(frames_a, grid_a.fps);
    let frames_b = frame(moved, grid_b.fps);
    if (seconds(frames_b, grid_b.fps) - moved).abs() > 1e-6 {
        return Err(format!(
            "by_seconds: clips {} and {} have frame rates ({} and {} fps) whose frames don't line \
             up {moved:.4} s from the cut; trim them one by one",
            a.id, b.id, grid_a.fps, grid_b.fps
        ));
    }
    let (new_a_out, new_b_in) = (a_out + frames_a, b_in + frames_b);
    let low = (a_in + 1 - a_out).max(-b_in);
    let high = (grid_a.frames - a_out).min(b_out - 1 - b_in);
    if new_a_out <= a_in || new_a_out > grid_a.frames || new_b_in < 0 || new_b_in >= b_out {
        return Err(format!(
            "by_seconds: {:.4} s moves the cut between {} and {} out of reach; it can move from \
             {:.4} to {:.4} s (each clip keeps a frame and stays within its take)",
            params.by_seconds,
            a.id,
            b.id,
            seconds(low, grid_a.fps),
            seconds(high, grid_a.fps)
        ));
    }
    let next = b.id.clone();
    timeline.clips[index].out_seconds = seconds(new_a_out, grid_a.fps);
    timeline.clips[index + 1].in_seconds = seconds(new_b_in, grid_b.fps);
    Ok(next)
}

/// The timeline's clips and removed takes now, for its history.
fn snapshot(timeline: &Timeline, action: TimelineAction) -> Revision {
    Revision {
        action,
        clips: timeline.clips.clone(),
        removed: timeline.removed.clone(),
    }
}

/// Apply `edit` to `timeline` as one revision (undoable, clearing redo).
/// An edit that fails leaves the timeline as it was; one that changes
/// nothing (a split on a boundary, an angle already shown) is no revision.
pub fn revise<T>(
    timeline: &mut Timeline,
    action: TimelineAction,
    edit: impl FnOnce(&mut Timeline) -> Result<T, String>,
) -> Result<T, String> {
    let before = snapshot(timeline, action);
    let next_clip = timeline.next_clip;
    let result = match edit(timeline) {
        Ok(result) => result,
        Err(err) => {
            timeline.clips = before.clips;
            timeline.removed = before.removed;
            timeline.next_clip = next_clip;
            return Err(err);
        }
    };
    if timeline.clips == before.clips && timeline.removed == before.removed {
        return Ok(result);
    }
    timeline.undo.push(before);
    if timeline.undo.len() > UNDO_LIMIT {
        let extra = timeline.undo.len() - UNDO_LIMIT;
        timeline.undo.drain(..extra);
    }
    timeline.redo.clear();
    timeline.revision += 1;
    Ok(result)
}

/// A change that recording made (a take added or retaken): a new revision
/// that clears the history, since the takes' order no longer matches it.
pub fn recorded(timeline: &mut Timeline) {
    timeline.undo.clear();
    timeline.redo.clear();
    timeline.revision += 1;
}

/// Undo (`redo` false) or redo the last edit: returns what it was.
pub fn step(timeline: &mut Timeline, redo: bool) -> Result<TimelineAction, String> {
    let (from, to) = match redo {
        false => (&mut timeline.undo, "undo"),
        true => (&mut timeline.redo, "redo"),
    };
    let Some(revision) = from.pop() else {
        return Err(format!(
            "timeline {:?} has nothing to {to}{}",
            timeline.id,
            if redo {
                ""
            } else {
                " (undo goes back through the edits since the last take was recorded)"
            }
        ));
    };
    let now = snapshot(timeline, revision.action);
    timeline.clips = revision.clips;
    // Takes taken off by recording meanwhile (dropped while recording,
    // interrupted) stay listed: only edits are undone.
    let mut removed = revision.removed;
    for entry in std::mem::take(&mut timeline.removed) {
        if entry.reason != RemovedReason::Removed && !removed.contains(&entry) {
            removed.push(entry);
        }
    }
    timeline.removed = removed;
    match redo {
        false => timeline.redo.push(now),
        true => timeline.undo.push(now),
    }
    timeline.revision += 1;
    Ok(revision.action)
}

impl Daemon {
    /// Apply one edit to the timeline `target` names (or the current one)
    /// as one revision, publish `timeline_changed`, and return the
    /// timeline.
    fn edit_timeline(
        &self,
        target: Option<&str>,
        action: TimelineAction,
        edit: impl FnOnce(&mut Timeline, &Grids) -> Result<(Option<String>, Vec<String>), String>,
    ) -> Result<TimelineInfo, RpcError> {
        self.heal_timelines();
        let (id, clip, notes) = {
            let mut timelines = self.timelines.lock();
            let id = timelines.resolve(target)?;
            let (clip, notes) = timelines.change(&id, |timeline| {
                let grids = grids(timeline).map_err(wrong_state)?;
                revise(timeline, action, |timeline| edit(timeline, &grids)).map_err(invalid)
            })?;
            (id, clip, notes)
        };
        self.timeline_changed(&id, action, clip);
        self.timeline_info(&id, notes)
    }

    pub(crate) fn timeline_trim(
        &self,
        params: TimelineTrimParams,
    ) -> Result<TimelineInfo, RpcError> {
        self.edit_timeline(
            params.timeline.as_deref(),
            TimelineAction::Trimmed,
            |timeline, grids| {
                trim(timeline, grids, &params)?;
                Ok((Some(params.clip.clone()), Vec::new()))
            },
        )
    }

    pub(crate) fn timeline_split(
        &self,
        params: TimelineSplitParams,
    ) -> Result<TimelineInfo, RpcError> {
        self.edit_timeline(
            params.timeline.as_deref(),
            TimelineAction::Split,
            |timeline, grids| {
                let new = split(timeline, grids, &params)?;
                let notes = match &new {
                    Some(id) => vec![format!("the clip after the cut is {id}")],
                    None => vec![format!(
                        "{} s is already between two clips; nothing was cut",
                        params.at_seconds
                    )],
                };
                Ok((new, notes))
            },
        )
    }

    pub(crate) fn timeline_delete_range(
        &self,
        params: TimelineRangeParams,
    ) -> Result<TimelineInfo, RpcError> {
        self.edit_timeline(
            params.timeline.as_deref(),
            TimelineAction::Deleted,
            |timeline, grids| {
                let gone = delete_range(timeline, grids, &params)?;
                let notes = vec![format!(
                    "took out {} (clip{} {}); the takes' files are untouched",
                    describe_span(params.start_seconds, params.end_seconds),
                    if gone.len() == 1 { "" } else { "s" },
                    gone.join(", ")
                )];
                Ok((gone.first().cloned(), notes))
            },
        )
    }

    pub(crate) fn timeline_angle(
        &self,
        params: TimelineAngleParams,
    ) -> Result<TimelineInfo, RpcError> {
        self.edit_timeline(
            params.timeline.as_deref(),
            TimelineAction::Angle,
            |timeline, grids| {
                let changed = angle(timeline, grids, &params)?;
                let notes = vec![format!(
                    "the edited video shows {} over {} (clip{} {})",
                    params.source.trim(),
                    describe_span(params.start_seconds, params.end_seconds),
                    if changed.len() == 1 { "" } else { "s" },
                    changed.join(", ")
                )];
                Ok((changed.first().cloned(), notes))
            },
        )
    }

    pub(crate) fn timeline_crop(
        &self,
        params: TimelineCropParams,
    ) -> Result<TimelineInfo, RpcError> {
        self.edit_timeline(
            params.timeline.as_deref(),
            TimelineAction::Cropped,
            |timeline, grids| {
                let changed = crop(timeline, grids, &params)?;
                let what = match (params.clear, params.preset) {
                    (true, _) => "the whole picture".to_string(),
                    (false, Some(preset)) => format!("a {} crop", preset.name()),
                    (false, None) => "a crop".to_string(),
                };
                let notes = vec![format!(
                    "the edited video shows {what} over {} (clip{} {})",
                    describe_span(params.start_seconds, params.end_seconds),
                    if changed.len() == 1 { "" } else { "s" },
                    changed.join(", ")
                )];
                Ok((changed.first().cloned(), notes))
            },
        )
    }

    pub(crate) fn timeline_roll(
        &self,
        params: TimelineRollParams,
    ) -> Result<TimelineInfo, RpcError> {
        self.edit_timeline(
            params.timeline.as_deref(),
            TimelineAction::Rolled,
            |timeline, grids| {
                let next = roll(timeline, grids, &params)?;
                let notes = vec![format!(
                    "moved the cut between {} and {next} by {:+.3} s; the timeline's length is \
                     unchanged",
                    params.clip, params.by_seconds
                )];
                Ok((Some(params.clip.clone()), notes))
            },
        )
    }

    pub(crate) fn timeline_step(
        &self,
        target: TimelineTarget,
        redo: bool,
    ) -> Result<TimelineInfo, RpcError> {
        self.heal_timelines();
        let (id, undone) = {
            let mut timelines = self.timelines.lock();
            let id = timelines.resolve(target.timeline.as_deref())?;
            let undone =
                timelines.change(&id, |timeline| step(timeline, redo).map_err(wrong_state))?;
            (id, undone)
        };
        let action = match redo {
            false => TimelineAction::Undone,
            true => TimelineAction::Redone,
        };
        self.timeline_changed(&id, action, None);
        let what = serde_json::to_value(undone)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let note = match redo {
            false => format!("undid the last edit ({what})"),
            true => format!("redid the edit undone ({what})"),
        };
        self.timeline_info(&id, vec![note])
    }
}

fn describe_span(start: f64, end: f64) -> String {
    format!("{start:.3}–{end:.3} s")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timeline(takes: &[(&str, i64)]) -> (Timeline, Grids) {
        let mut grids = Grids::new();
        let mut clips = Vec::new();
        for (n, (take, frames)) in takes.iter().enumerate() {
            let take = PathBuf::from(format!("/r/{take}.session.json"));
            grids.insert(
                take.clone(),
                TakeGrid {
                    fps: 60,
                    frames: *frames,
                    sources: vec!["camera".into(), PROGRAM.into()],
                    sizes: vec![(1280, 720), (1920, 1080)],
                },
            );
            clips.push(Clip {
                id: format!("c{}", n + 1),
                take,
                in_seconds: 0.0,
                out_seconds: seconds(*frames, 60),
                project: None,
                angle: None,
                crop: None,
                extra: Default::default(),
            });
        }
        let timeline = Timeline {
            version: 1,
            id: "t".into(),
            uid: None,
            name: "T".into(),
            created_at: String::new(),
            next_clip: clips.len() as u32 + 1,
            clips,
            removed: Vec::new(),
            recording: Vec::new(),
            revision: 0,
            undo: Vec::new(),
            redo: Vec::new(),
        };
        (timeline, grids)
    }

    fn layout(timeline: &Timeline) -> Vec<(String, i64, i64, Option<String>)> {
        timeline
            .clips
            .iter()
            .map(|c| {
                (
                    c.id.clone(),
                    frame(c.in_seconds, 60),
                    frame(c.out_seconds, 60),
                    c.angle.clone(),
                )
            })
            .collect()
    }

    #[test]
    fn edits_cut_on_frames_and_undo_back() {
        // Two takes: 3 s and 2 s at 60 fps.
        let (mut t, grids) = timeline(&[("a", 180), ("b", 120)]);
        // Split 1.004 s in: rounds to frame 60.
        let new = revise(&mut t, TimelineAction::Split, |t| {
            split(
                t,
                &grids,
                &TimelineSplitParams {
                    at_seconds: 1.004,
                    timeline: None,
                },
            )
        })
        .unwrap();
        assert_eq!(new.as_deref(), Some("c3"));
        assert_eq!(
            layout(&t),
            [
                ("c1".into(), 0, 60, None),
                ("c3".into(), 60, 180, None),
                ("c2".into(), 0, 120, None)
            ]
        );
        // Splitting on a boundary does nothing.
        let same = split(
            &mut t,
            &grids,
            &TimelineSplitParams {
                at_seconds: 3.0,
                timeline: None,
            },
        )
        .unwrap();
        assert_eq!(same, None);
        // Ripple delete 2.5–3.5 s: the end of c3 and the start of c2.
        revise(&mut t, TimelineAction::Deleted, |t| {
            delete_range(
                t,
                &grids,
                &TimelineRangeParams {
                    start_seconds: 2.5,
                    end_seconds: 3.5,
                    timeline: None,
                },
            )
        })
        .unwrap();
        assert_eq!(
            layout(&t),
            [
                ("c1".into(), 0, 60, None),
                ("c3".into(), 60, 150, None),
                ("c5".into(), 30, 120, None)
            ]
        );
        assert!((total(&t) - 4.0).abs() < 1e-9);
        // The camera angle over 0.5–1.5 s.
        revise(&mut t, TimelineAction::Angle, |t| {
            angle(
                t,
                &grids,
                &TimelineAngleParams {
                    start_seconds: 0.5,
                    end_seconds: 1.5,
                    source: "camera".into(),
                    timeline: None,
                },
            )
        })
        .unwrap();
        let cam = Some("camera".to_string());
        assert_eq!(
            layout(&t),
            [
                ("c1".into(), 0, 30, None),
                ("c6".into(), 30, 60, cam.clone()),
                ("c3".into(), 60, 90, cam.clone()),
                ("c7".into(), 90, 150, None),
                ("c5".into(), 30, 120, None)
            ]
        );
        // Unknown angles say which there are.
        let err = angle(
            &mut t.clone(),
            &grids,
            &TimelineAngleParams {
                start_seconds: 0.0,
                end_seconds: 1.0,
                source: "screen".into(),
                timeline: None,
            },
        )
        .unwrap_err();
        assert!(err.starts_with("source: \"screen\""), "{err}");
        // It names the clips there are, not pieces of a cut never made.
        assert!(err.contains("clips c1, c6"), "{err}");
        assert!(err.contains("camera, program"), "{err}");
        // Trim c5's out point; a trim past the take is refused by field.
        revise(&mut t, TimelineAction::Trimmed, |t| {
            trim(
                t,
                &grids,
                &TimelineTrimParams {
                    clip: "c5".into(),
                    in_seconds: None,
                    out_seconds: Some(1.5),
                    timeline: None,
                },
            )
        })
        .unwrap();
        assert_eq!(layout(&t)[4], ("c5".into(), 30, 90, None));
        let err = trim(
            &mut t.clone(),
            &grids,
            &TimelineTrimParams {
                clip: "c5".into(),
                in_seconds: Some(9.0),
                out_seconds: None,
                timeline: None,
            },
        )
        .unwrap_err();
        assert!(err.starts_with("in_seconds:"), "{err}");
        // Undo all four edits, then redo one.
        assert_eq!(t.undo.len(), 4);
        for _ in 0..4 {
            step(&mut t, false).unwrap();
        }
        assert_eq!(
            layout(&t),
            [("c1".into(), 0, 180, None), ("c2".into(), 0, 120, None)]
        );
        assert!(step(&mut t, false).is_err());
        // An edit that changes nothing is no revision.
        revise(&mut t, TimelineAction::Split, |t| {
            split(
                t,
                &grids,
                &TimelineSplitParams {
                    at_seconds: 3.0,
                    timeline: None,
                },
            )
        })
        .unwrap();
        assert!(t.undo.is_empty());
        assert_eq!(step(&mut t, true).unwrap(), TimelineAction::Split);
        assert_eq!(layout(&t).len(), 3);
        // A new edit clears redo; ids are never reused.
        revise(&mut t, TimelineAction::Split, |t| {
            split(
                t,
                &grids,
                &TimelineSplitParams {
                    at_seconds: 4.0,
                    timeline: None,
                },
            )
        })
        .unwrap();
        assert!(t.redo.is_empty());
        assert_eq!(t.clips.last().unwrap().id, "c8");
    }

    #[test]
    fn deleting_a_whole_take_lists_it_as_removed() {
        let (mut t, grids) = timeline(&[("a", 60), ("b", 60)]);
        let gone = delete_range(
            &mut t,
            &grids,
            &TimelineRangeParams {
                start_seconds: 0.0,
                end_seconds: 1.0,
                timeline: None,
            },
        )
        .unwrap();
        assert_eq!(gone, ["c1"]);
        assert_eq!(t.removed.len(), 1);
        assert_eq!(t.removed[0].take, PathBuf::from("/r/a.session.json"));
        let err = delete_range(
            &mut t,
            &grids,
            &TimelineRangeParams {
                start_seconds: 0.5,
                end_seconds: 0.501,
                timeline: None,
            },
        )
        .unwrap_err();
        assert!(err.contains("less than a frame"), "{err}");
    }

    #[test]
    fn crops_snap_to_pixels_follow_angles_and_undo() {
        let (mut t, grids) = timeline(&[("a", 120), ("b", 120)]);
        let params = |crop, preset, clear| TimelineCropParams {
            start_seconds: 1.0,
            end_seconds: 3.0,
            crop,
            preset,
            clear,
            timeline: None,
        };
        // A 9:16 preset on the 1920x1080 program: 608x1080, centred.
        revise(&mut t, TimelineAction::Cropped, |t| {
            crop(
                t,
                &grids,
                &params(
                    None,
                    Some(isocast_protocol::timeline::CropPreset::Vertical),
                    false,
                ),
            )
        })
        .unwrap();
        assert_eq!(t.clips.len(), 4);
        let c = t.clips[1].crop.unwrap();
        assert!((c.width * 1920.0 - 608.0).abs() < 1e-6, "{c:?}");
        assert!((c.height - 1.0).abs() < 1e-9 && (c.x * 1920.0 - 656.0).abs() < 1e-6);
        assert_eq!(t.clips[0].crop, None);
        assert_eq!(t.clips[3].crop, None);
        // A rectangle off the 2-pixel grid lands on it.
        let r = Crop {
            x: 0.1001,
            y: 0.2,
            width: 0.5,
            height: 0.5,
        };
        crop(&mut t, &grids, &params(Some(r), None, false)).unwrap();
        let c = t.clips[1].crop.unwrap();
        assert_eq!((c.x * 1920.0).round() % 2.0, 0.0);
        // Another angle's picture size: the crop lands on its grid (#94),
        // and a crop too small for it refuses the angle, naming the clip.
        let mut moved = t.clone();
        moved.clips[1].crop = Some(Crop {
            x: 0.1003,
            y: 0.2,
            width: 0.5,
            height: 0.5,
        });
        let angle_params = TimelineAngleParams {
            start_seconds: 1.0,
            end_seconds: 3.0,
            source: "camera".into(),
            timeline: None,
        };
        angle(&mut moved, &grids, &angle_params).unwrap();
        let c = moved.clips[1].crop.unwrap();
        for v in [
            c.x * 1280.0,
            c.width * 1280.0,
            c.y * 720.0,
            c.height * 720.0,
        ] {
            assert!((v / 2.0 - (v / 2.0).round()).abs() < 1e-6, "{c:?}");
        }
        let mut tiny = t.clone();
        tiny.clips[1].crop = Some(Crop {
            width: 34.0 / 1920.0,
            ..r
        });
        let err = angle(&mut tiny, &grids, &angle_params).unwrap_err();
        assert!(
            err.starts_with(&format!("clip {}:", tiny.clips[1].id)) && err.contains("too small"),
            "{err}"
        );
        // Errors name the field.
        let bad = Crop { x: 0.8, ..r };
        let err = crop(&mut t.clone(), &grids, &params(Some(bad), None, false)).unwrap_err();
        assert!(err.starts_with("crop.x, crop.width:"), "{err}");
        let tiny = Crop { width: 0.01, ..r };
        let err = crop(&mut t.clone(), &grids, &params(Some(tiny), None, false)).unwrap_err();
        assert!(err.contains("too small"), "{err}");
        let err = crop(&mut t.clone(), &grids, &params(None, None, false)).unwrap_err();
        assert!(err.starts_with("crop, preset, clear:"), "{err}");
        // The whole picture is no crop; clear takes it off.
        crop(&mut t, &grids, &params(Some(Crop::FULL), None, false)).unwrap();
        assert_eq!(t.clips[1].crop, None);
        crop(&mut t, &grids, &params(Some(r), None, false)).unwrap();
        crop(&mut t, &grids, &params(None, None, true)).unwrap();
        assert!(t.clips.iter().all(|c| c.crop.is_none()));
        // Undo goes back to before the first crop (the cuts too).
        step(&mut t, false).unwrap();
        assert_eq!(t.clips.len(), 2);
    }

    #[test]
    fn rolls_move_a_cut_and_keep_the_length() {
        let (mut t, grids) = timeline(&[("a", 120), ("b", 120)]);
        // Trim both so the cut can move either way.
        t.clips[0].out_seconds = 1.5;
        t.clips[1].in_seconds = 0.5;
        let before = total(&t);
        let roll_by = |t: &mut Timeline, by| {
            roll(
                t,
                &grids,
                &TimelineRollParams {
                    clip: "c1".into(),
                    by_seconds: by,
                    timeline: None,
                },
            )
        };
        assert_eq!(roll_by(&mut t, -0.25).unwrap(), "c2");
        assert_eq!(layout(&t)[0], ("c1".into(), 0, 75, None));
        assert_eq!(layout(&t)[1], ("c2".into(), 15, 120, None));
        assert!((total(&t) - before).abs() < 1e-9);
        // Past the next clip's take start: refused, naming the reach.
        let err = roll_by(&mut t.clone(), -0.5).unwrap_err();
        assert!(
            err.starts_with("by_seconds:") && err.contains("can move from"),
            "{err}"
        );
        let err = roll(
            &mut t.clone(),
            &grids,
            &TimelineRollParams {
                clip: "c2".into(),
                by_seconds: 0.1,
                timeline: None,
            },
        )
        .unwrap_err();
        assert!(err.contains("last clip") && err.contains("c1"), "{err}");
    }
}
