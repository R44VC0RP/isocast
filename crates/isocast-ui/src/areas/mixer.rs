//! Area B, audio: the Audio Mixer dock, Advanced Audio Properties, the
//! meters and the mixer's menus (ui/areas/mixer/). The web app's mixer.js
//! and audio-props.js are the behaviour matched here.
//!
//! The mixer shows, as OBS does, the audio-only sources (they play in
//! every scene) and the sources with sound that the scene on air (or, in
//! Studio Mode, in preview) shows, less the ones this viewer hid (kept
//! with the layout, like the web app's `mixerHidden`). Meters rise at once
//! and fall smoothly (RMS ~24 dB/s, peak ~32 dB/s) with the peak held for
//! 1.5 s, drawn by a timer that runs only while some meter moves: silence
//! costs nothing. Levels older than 1.5 s count as silence.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

use isocast_protocol::api::{Event, SourceRun};
use isocast_protocol::{Monitoring, SceneItem, Setup};
use serde_json::json;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use super::{Area, Changed};
use crate::app::App;
use crate::kit::{self, MenuBuilder};
use crate::model as setup_model;
use crate::session::EditOptions;
use crate::{AppWindow, AudioRow, Level, MixerArea, Strip, Ui};

/// Snapshot states (`--state NAME[:ARG]`).
pub const STATES: &[&str] = &[
    "mixer-menu[:SOURCE]",
    "mixer-choose:SOURCE:ITEM",
    "mixer-mute:SOURCE",
];

/// The meters' floor, in dBFS.
const FLOOR: f64 = -60.0;
/// The program mix's levels come as this source.
const PROGRAM: &str = "program";
/// How often moving meters are drawn (30 fps). Each change redraws the
/// whole window (the Skia wgpu renderer has no partial rendering), so this
/// sets the meters' cost: docs/notes.md has the measurements.
const TICK: Duration = Duration::from_millis(33);
/// A level this old is silence (the source stopped sending).
const STALE: Duration = Duration::from_millis(1500);
/// How long a peak is held.
const HOLD: Duration = Duration::from_millis(1500);
/// A meter is redrawn when a bar or the hold moves by this much (about a
/// pixel of a wide meter).
const STEP_DB: f64 = 0.1;
/// The control bar's master level moves in whole device pixels of its
/// bars (Theme.master-meter-w less its 1px edge and 3px padding each
/// side): 60 dB over 112 points is ~0.54 dB a pixel at 1x. A smaller
/// change would redraw the window for nothing.
const MASTER_INSET: f64 = 8.0;
/// Live edits from a fader or a balance slider: at most one per this.
const LIVE_INTERVAL: Duration = Duration::from_millis(50);

/// Audio Monitoring's choices (value, label), as OBS words them.
const MONITORING: [(&str, &str); 3] = [
    ("off", "Monitor Off"),
    ("monitor_only", "Monitor Only (mute output)"),
    ("monitor_and_output", "Monitor and Output"),
];

/// One meter's smoothed state.
#[derive(Clone, Copy)]
struct MeterState {
    rms: f64,
    peak: f64,
    hold: f64,
    hold_at: Instant,
}

impl MeterState {
    fn new(now: Instant) -> Self {
        Self {
            rms: FLOOR,
            peak: FLOOR,
            hold: FLOOR,
            hold_at: now,
        }
    }

    /// Ease towards the target levels over `dt` seconds.
    fn step(&mut self, (rms, peak): (f64, f64), dt: f64, now: Instant) {
        self.rms = if rms > self.rms {
            rms
        } else {
            rms.max(self.rms - 24.0 * dt)
        };
        self.peak = if peak > self.peak {
            peak
        } else {
            peak.max(self.peak - 32.0 * dt)
        };
        if self.peak >= self.hold || now.duration_since(self.hold_at) > HOLD {
            self.hold = self.peak;
            self.hold_at = now;
        }
    }

    /// Still showing something (or falling back).
    fn active(&self, target_peak: f64) -> bool {
        self.peak > FLOOR + 0.5 || self.hold > FLOOR + 0.5 || target_peak > FLOOR
    }
}

pub struct Mixer {
    /// The shown strips' source ids, and their meters' rows (apart from
    /// the strips, so a moving meter changes nothing else).
    shown: RefCell<Vec<String>>,
    // The models on MixerArea, changed row by row: a row that is replaced
    // rebuilds its strip, which would end a fader drag at its first step.
    strips: Rc<VecModel<Strip>>,
    levels: Rc<VecModel<Level>>,
    rows: Rc<VecModel<AudioRow>>,
    /// Each source's meter, by id (kept while strips are rebuilt).
    meters: RefCell<HashMap<String, MeterState>>,
    /// The program mix's meter (the control bar's master level), left and
    /// right, and what each last showed.
    master: RefCell<[(Option<MeterState>, Level); 2]>,
    /// With no strip meter showing, the master level moves on every other
    /// tick (15 a second): this flips each tick.
    master_skip: Cell<bool>,
    /// macOS, layers preview: the master level is a native layer (see
    /// layers.rs), which moves without redrawing the window.
    #[cfg(target_os = "macos")]
    native: RefCell<Option<Rc<RefCell<crate::layers::Layers>>>>,
    /// When each source's levels last came.
    arrived: RefCell<HashMap<String, Instant>>,
    timer: slint::Timer,
    last_tick: Cell<Option<Instant>>,
}

impl Default for Mixer {
    fn default() -> Self {
        Self {
            shown: RefCell::default(),
            strips: Rc::default(),
            levels: Rc::default(),
            rows: Rc::default(),
            meters: RefCell::default(),
            master: RefCell::default(),
            master_skip: Cell::new(false),
            #[cfg(target_os = "macos")]
            native: RefCell::default(),
            arrived: RefCell::default(),
            timer: slint::Timer::default(),
            last_tick: Cell::new(None),
        }
    }
}

impl Mixer {
    /// Every strip, from the setup (and the meters as they are).
    fn show_strips(&self, app: &Rc<App>, window: &AppWindow) {
        let area = window.global::<MixerArea>();
        let hidden: HashSet<String> = area.get_hidden().iter().map(|x| x.to_string()).collect();
        let now = crate::clock::now();
        let (strips, levels, count) = {
            let s = app.session.borrow();
            let Some(setup) = s.setup.clone() else {
                drop(s);
                kit::update_rows(&self.strips, Vec::new());
                kit::update_rows(&self.levels, Vec::new());
                area.set_source_count(0);
                self.shown.borrow_mut().clear();
                return;
            };
            let preview = s.studio.then_some(s.preview.as_deref()).flatten();
            let sources = mixer_sources(&setup, s.scene.as_deref(), preview);
            let count = sources.len();
            let meters = self.meters.borrow();
            let shown: Vec<_> = sources
                .into_iter()
                .filter(|source| !hidden.contains(&source.id))
                .collect();
            let levels: Vec<Level> = shown
                .iter()
                .map(|source| {
                    let meter = meters
                        .get(&source.id)
                        .copied()
                        .unwrap_or_else(|| MeterState::new(now));
                    let mut level = Level::default();
                    show_meter(&mut level, &meter);
                    level
                })
                .collect();
            *self.shown.borrow_mut() = shown.iter().map(|x| x.id.clone()).collect();
            let strips: Vec<Strip> = shown
                .into_iter()
                .map(|source| {
                    let audio = source.audio;
                    let run = s.running.get(&source.id);
                    Strip {
                        id: source.id.as_str().into(),
                        name: setup_model::source_name(source).into(),
                        gain_db: audio.gain_db as f32,
                        gain_text: setup_model::db_text(audio.gain_db).into(),
                        muted: audio.muted,
                        not_in_mix: !setup.recording.program.audio.contains(&source.id),
                        monitored: audio.monitor != Monitoring::Off,
                        monitor_only: audio.monitor == Monitoring::MonitorOnly,
                        opening: run.is_some_and(|r| r.state == SourceRun::Opening),
                        failed: run.is_some_and(|r| r.state == SourceRun::Failed),
                        error: run.and_then(|r| r.error.clone()).unwrap_or_default().into(),
                    }
                })
                .collect();
            (strips, levels, count)
        };
        kit::update_rows(&self.levels, levels);
        kit::update_rows(&self.strips, strips);
        area.set_source_count(count as i32);
    }

    /// Advanced Audio Properties' rows: every audio source.
    fn show_rows(&self, app: &Rc<App>, window: &AppWindow) {
        let area = window.global::<MixerArea>();
        let s = app.session.borrow();
        let Some(setup) = s.setup.clone() else {
            drop(s);
            kit::update_rows(&self.rows, Vec::new());
            return;
        };
        let rows: Vec<AudioRow> = setup
            .sources
            .iter()
            .filter(|x| x.kind.has_audio())
            .map(|source| {
                let audio = source.audio;
                AudioRow {
                    id: source.id.as_str().into(),
                    name: setup_model::source_name(source).into(),
                    gain_db: audio.gain_db as f32,
                    muted: audio.muted,
                    balance: audio.balance as f32,
                    balance_text: balance_text(audio.balance).into(),
                    offset_ms: audio.offset_ms as f32,
                    monitor: monitoring_value(audio.monitor).into(),
                    program: setup.recording.program.audio.contains(&source.id),
                    iso: setup
                        .recording
                        .iso
                        .audio_for(
                            source,
                            source.kind.has_audio()
                                && setup.recording.iso.sources.contains(&source.id),
                        )
                        .contains(&source.id),
                }
            })
            .collect();
        kit::update_rows(&self.rows, rows);
        area.set_iso_enabled(setup.recording.iso.enabled);
    }

    /// What `id`'s meter is heading for: its newest level after its fader
    /// (a muted source too, greyed), or silence once levels stop.
    fn target(&self, app: &Rc<App>, setup: &Setup, id: &str, now: Instant) -> (f64, f64) {
        let fresh = self
            .arrived
            .borrow()
            .get(id)
            .is_some_and(|at| now.duration_since(*at) <= STALE);
        let s = app.session.borrow();
        let level = s.levels.get(id).filter(|_| fresh);
        let audio = setup_model::find_source(setup, id).map(|x| x.audio);
        // A muted source's meter still shows what it hears (drawn in
        // `meter-muted`: Paper's muted strip), as its fader sets it.
        match (level, audio) {
            (Some(level), Some(audio)) => (
                (level.rms_db + audio.gain_db).max(FLOOR),
                (level.peak_db + audio.gain_db).max(FLOOR),
            ),
            _ => (FLOOR, FLOOR),
        }
    }

    /// What the program's meter is heading for, left and right: the
    /// mix's newest level (both bars the same from an engine that sends
    /// one level), or silence once levels stop.
    fn program_target(&self, app: &Rc<App>, now: Instant) -> [(f64, f64); 2] {
        let fresh = self
            .arrived
            .borrow()
            .get(PROGRAM)
            .is_some_and(|at| now.duration_since(*at) <= STALE);
        match app.session.borrow().levels.get(PROGRAM).filter(|_| fresh) {
            Some(level) => match &level.channels {
                Some(channels) => channels.map(|c| (c.rms_db.max(FLOOR), c.peak_db.max(FLOOR))),
                None => [(level.rms_db.max(FLOOR), level.peak_db.max(FLOOR)); 2],
            },
            None => [(FLOOR, FLOOR); 2],
        }
    }

    /// Run the meters' timer if any shown meter has something to show.
    fn wake(&self, app: &Rc<App>) {
        if self.timer.running() {
            return;
        }
        let Some(setup) = app.session.borrow().setup.clone() else {
            return;
        };
        let now = crate::clock::now();
        let meters = self.meters.borrow();
        let any = self.shown.borrow().iter().any(|id| {
            let target = self.target(app, &setup, id, now);
            meters.get(id.as_str()).is_some_and(|m| m.active(target.1)) || target.1 > FLOOR
        });
        drop(meters);
        let program = self.program_target(app, now);
        let any = any
            || self
                .master
                .borrow()
                .iter()
                .zip(program)
                .any(|((meter, _), target)| {
                    target.1 > FLOOR || meter.is_some_and(|m| m.active(target.1))
                });
        if !any {
            return;
        }
        self.last_tick.set(Some(now));
        let weak = Rc::downgrade(app);
        self.timer.start(slint::TimerMode::Repeated, TICK, move || {
            if let Some(app) = weak.upgrade() {
                app.mixer.tick(&app);
            }
        });
    }

    /// One frame of the meters' timer; it stops when every meter rests.
    fn tick(&self, app: &Rc<App>) {
        if !self.step_meters(app) {
            self.timer.stop();
            self.last_tick.set(None);
        }
    }

    /// Ease each meter towards its target since the last step, and change
    /// the rows whose bars moved. True while some meter still moves.
    fn step_meters(&self, app: &Rc<App>) -> bool {
        let now = crate::clock::now();
        let dt = self
            .last_tick
            .replace(Some(now))
            .map_or(0.0, |last| now.duration_since(last).as_secs_f64().min(0.1));
        let (Some(rows), Some(setup)) = (
            Some(self.levels.clone()),
            app.session.borrow().setup.clone(),
        ) else {
            return false;
        };
        let mut active = false;
        let shown = self.shown.borrow().clone();
        for (i, id) in shown.iter().enumerate() {
            let Some(mut level) = rows.row_data(i) else {
                continue;
            };
            let target = self.target(app, &setup, id, now);
            let meter = {
                let mut meters = self.meters.borrow_mut();
                let meter = meters
                    .entry(id.clone())
                    .or_insert_with(|| MeterState::new(now));
                meter.step(target, dt, now);
                active |= meter.active(target.1);
                *meter
            };
            if show_meter(&mut level, &meter) {
                rows.set_row_data(i, level);
            }
        }
        // The program's, left and right. With no strip meter showing it is
        // the only thing redrawing the window while sound plays, so it
        // moves 15 times a second then, and only by whole pixels.
        let Some(window) = app.window() else {
            return active;
        };
        #[cfg(target_os = "macos")]
        if let Some(layers) = self.native.borrow().as_ref() {
            return self.step_native_master(app, &window, layers, dt, now) || active;
        }
        let ui = window.global::<Ui>();
        let strips_drawn = ui.get_show_docks() && ui.get_show_mixer() && !shown.is_empty();
        let skip = !strips_drawn && !self.master_skip.replace(!self.master_skip.get());
        let bar = f64::from(window.global::<crate::Theme>().get_master_meter_w()) - MASTER_INSET;
        let pixel_db = 60.0 / (bar * f64::from(window.window().scale_factor())).max(1.0);
        let targets = self.program_target(app, now);
        let mut master = self.master.borrow_mut();
        let mut moved = [false; 2];
        for ((meter, level), (target, moved)) in master
            .iter_mut()
            .zip(targets.into_iter().zip(moved.iter_mut()))
        {
            let meter = meter.get_or_insert_with(|| MeterState::new(now));
            meter.step(target, dt, now);
            let still = meter.active(target.1);
            active |= still;
            // A meter coming to rest is drawn at rest, skip or not.
            if !skip || !still {
                *moved = show_meter_by(level, meter, pixel_db);
            }
        }
        let area = window.global::<MixerArea>();
        if moved[0] {
            area.set_master_left(master[0].1.clone());
        }
        if moved[1] {
            area.set_master_right(master[1].1.clone());
        }
        active
    }

    /// Draw the master level in its native layer (from now on, macOS with
    /// the layers preview): the control bar leaves its inside unpainted.
    #[cfg(target_os = "macos")]
    pub fn use_native_master(
        &self,
        window: &AppWindow,
        layers: Rc<RefCell<crate::layers::Layers>>,
    ) {
        let silent = crate::layers::MeterLevel {
            rms: FLOOR as f32,
            peak: FLOOR as f32,
            hold: FLOOR as f32,
        };
        layers
            .borrow_mut()
            .show_meter(crate::preview::meter_rect(window), [silent; 2]);
        *self.native.borrow_mut() = Some(layers);
        window.global::<MixerArea>().set_master_native(true);
    }

    /// The native master level's step: by whole device pixels, at the
    /// meters' 30 a second (a layer moves without redrawing the window).
    #[cfg(target_os = "macos")]
    fn step_native_master(
        &self,
        app: &Rc<App>,
        window: &AppWindow,
        layers: &Rc<RefCell<crate::layers::Layers>>,
        dt: f64,
        now: Instant,
    ) -> bool {
        let rect = crate::preview::meter_rect(window);
        let bar = rect[2] - 2.0 * 3.0;
        let pixel_db = 60.0 / (bar * f64::from(window.window().scale_factor())).max(1.0);
        let targets = self.program_target(app, now);
        let mut master = self.master.borrow_mut();
        let mut active = false;
        let mut moved = false;
        for ((meter, level), target) in master.iter_mut().zip(targets) {
            let meter = meter.get_or_insert_with(|| MeterState::new(now));
            meter.step(target, dt, now);
            active |= meter.active(target.1);
            moved |= show_meter_by(level, meter, pixel_db);
        }
        if moved {
            let level = |l: &Level| crate::layers::MeterLevel {
                rms: l.rms,
                peak: l.peak,
                hold: l.hold,
            };
            layers
                .borrow_mut()
                .show_meter(rect, [level(&master[0].1), level(&master[1].1)]);
        }
        active
    }

    /// Hide `id` from the mixer (Unhide All brings it back), or with
    /// `None` unhide every source.
    fn set_hidden(&self, app: &Rc<App>, id: Option<&str>) {
        let Some(window) = app.window() else { return };
        let area = window.global::<MixerArea>();
        let mut hidden: Vec<SharedString> = match id {
            Some(_) => area.get_hidden().iter().collect(),
            None => Vec::new(),
        };
        if let Some(id) = id
            && !hidden.iter().any(|x| x == id)
        {
            hidden.push(id.into());
        }
        area.set_hidden(kit::model(hidden));
        // Kept with the layout.
        window.global::<Ui>().invoke_layout_changed();
        self.show_strips(app, &window);
        self.wake(app);
    }

    fn hidden_count(app: &Rc<App>) -> usize {
        app.window()
            .map_or(0, |w| w.global::<MixerArea>().get_hidden().row_count())
    }
}

/// Put a meter's state on its row (clamped to the scale); true if a bar
/// or the hold moved enough to draw.
fn show_meter(strip: &mut Level, meter: &MeterState) -> bool {
    show_meter_by(strip, meter, STEP_DB)
}

/// `show_meter`, drawing only moves of at least `step` dB.
fn show_meter_by(strip: &mut Level, meter: &MeterState, step: f64) -> bool {
    let shown = |db: f64| db.clamp(FLOOR, 0.0) as f32;
    let (rms, peak, hold) = (shown(meter.rms), shown(meter.peak), shown(meter.hold));
    let moved =
        |a: f32, b: f32| f64::from((a - b).abs()) >= step || (a != b && (a == -60.0 || b == -60.0));
    let changed = moved(rms, strip.rms) || moved(peak, strip.peak) || moved(hold, strip.hold);
    if changed {
        strip.rms = rms;
        strip.peak = peak;
        strip.hold = hold;
    }
    changed
}

/// The sources scene `id` shows, through groups and nested scenes.
fn shown_by(setup: &Setup, id: &str, out: &mut HashSet<String>, depth: usize) {
    let Some(scene) = setup_model::find_scene(setup, id) else {
        return;
    };
    if depth > 8 {
        return;
    }
    fn walk(setup: &Setup, items: &[SceneItem], out: &mut HashSet<String>, depth: usize) {
        for item in items {
            if let Some(source) = &item.source {
                out.insert(source.clone());
            }
            if let Some(scene) = &item.scene {
                shown_by(setup, scene, out, depth + 1);
            }
            walk(setup, &item.children, out, depth);
        }
    }
    walk(setup, &scene.items, out, depth);
}

/// The mixer's sources, as OBS has them: audio-only sources, and sources
/// with sound that the scene on air (or `preview`, in Studio Mode) shows.
fn mixer_sources<'a>(
    setup: &'a Setup,
    scene: Option<&str>,
    preview: Option<&str>,
) -> Vec<&'a isocast_protocol::Source> {
    let mut shown = HashSet::new();
    for id in [scene, preview].into_iter().flatten() {
        shown_by(setup, id, &mut shown, 0);
    }
    setup
        .sources
        .iter()
        .filter(|x| x.kind.has_audio() && (!x.kind.has_video() || shown.contains(&x.id)))
        .collect()
}

/// Balance as Advanced Audio Properties writes it: "C", "L20", "R100".
fn balance_text(balance: f64) -> String {
    if balance.abs() < 0.005 {
        return "C".into();
    }
    let side = if balance < 0.0 { "L" } else { "R" };
    format!("{side}{}", (balance.abs() * 100.0).round())
}

fn monitoring_value(monitor: Monitoring) -> &'static str {
    match monitor {
        Monitoring::Off => "off",
        Monitoring::MonitorOnly => "monitor_only",
        Monitoring::MonitorAndOutput => "monitor_and_output",
    }
}

/// Change `id`'s audio settings (`fields`, a merge patch of them).
fn set_audio(app: &Rc<App>, id: &str, fields: serde_json::Value, options: EditOptions) {
    app.edit(
        json!({ "sources": [{ "id": id, "audio": fields }] }),
        options,
    );
}

/// A live step of a gesture on `id`'s `field` (a fader, a balance slider).
fn live(id: &str, field: &str) -> EditOptions {
    EditOptions {
        interval: LIVE_INTERVAL,
        ..EditOptions::live(format!("audio:{id}:{field}"))
    }
}

/// Put `id` in, or take it out of, a track list: the program mix
/// ("program") or every ISO file ("iso").
fn set_track(app: &Rc<App>, track: &str, id: &str, on: bool) {
    if track == "iso"
        && app.session.borrow().setup.as_ref().is_some_and(|s| {
            s.recording.iso.audio.is_none()
                && s.source(id).is_some_and(|source| source.kind.has_video())
        })
    {
        app.edit(
            json!({"sources": [{"id": id, "audio": {"iso": on}}]}),
            EditOptions::default(),
        );
        return;
    }
    let list = {
        let s = app.session.borrow();
        let Some(setup) = s.setup.as_ref() else {
            return;
        };
        let list = match track {
            "program" => setup.recording.program.audio.as_slice(),
            _ => setup.recording.iso.audio.as_deref().unwrap_or(&[]),
        };
        let mut list: Vec<String> = list.iter().filter(|x| *x != id).cloned().collect();
        if on {
            list.push(id.to_string());
        }
        list
    };
    app.edit(
        json!({ "recording": { track: { "audio": list } } }),
        EditOptions::default(),
    );
}

/// The strip's mute button.
fn toggle_mute(app: &Rc<App>, id: &str) {
    let muted = {
        let s = app.session.borrow();
        s.setup
            .as_ref()
            .and_then(|setup| setup_model::find_source(setup, id))
            .map(|source| source.audio.muted)
    };
    if let Some(muted) = muted {
        set_audio(app, id, json!({ "muted": !muted }), EditOptions::default());
    }
}

fn in_program_mix(app: &Rc<App>, id: &str) -> bool {
    app.session
        .borrow()
        .setup
        .as_ref()
        .is_some_and(|x| x.recording.program.audio.iter().any(|s| s == id))
}

fn round_to(value: f32, step: f64) -> f64 {
    (f64::from(value) / step).round() * step
}

impl Area for Mixer {
    fn connect(&self, app: &Rc<App>, window: &AppWindow) {
        let area = window.global::<MixerArea>();
        area.set_strips(ModelRc::from(self.strips.clone()));
        area.set_levels(ModelRc::from(self.levels.clone()));
        area.set_rows(ModelRc::from(self.rows.clone()));
        area.set_monitoring(kit::model(
            MONITORING.map(|(value, label)| kit::option(value, label, "")),
        ));

        // A callback that takes the app and a source id (and a value).
        macro_rules! on {
            ($set:ident, |$app:ident, $id:ident $(, $v:ident : $t:ty)?| $body:expr) => {{
                let weak = Rc::downgrade(app);
                area.$set(move |$id: SharedString $(, $v: $t)?| {
                    if let Some($app) = weak.upgrade() {
                        let $id = $id.as_str();
                        $body;
                    }
                });
            }};
        }

        // The mixer.
        on!(on_gain_moved, |app, id, db: f32| set_audio(
            &app,
            id,
            json!({ "gain_db": round_to(db, 0.1) }),
            live(id, "gain")
        ));
        on!(on_gain_changed, |app, id, db: f32| set_audio(
            &app,
            id,
            json!({ "gain_db": round_to(db, 0.1) }),
            EditOptions::default()
        ));
        on!(on_gain_reset, |app, id| set_audio(
            &app,
            id,
            json!({ "gain_db": 0.0 }),
            EditOptions::default()
        ));
        on!(on_toggle_mute, |app, id| toggle_mute(&app, id));

        // Advanced Audio Properties.
        on!(on_set_gain, |app, id, db: f32| set_audio(
            &app,
            id,
            json!({ "gain_db": round_to(db, 0.1) }),
            EditOptions::default()
        ));
        on!(on_set_muted, |app, id, on: bool| set_audio(
            &app,
            id,
            json!({ "muted": on }),
            EditOptions::default()
        ));
        on!(on_balance_moved, |app, id, b: f32| set_audio(
            &app,
            id,
            json!({ "balance": round_to(b, 0.01) }),
            live(id, "balance")
        ));
        on!(on_balance_changed, |app, id, b: f32| set_audio(
            &app,
            id,
            json!({ "balance": round_to(b, 0.01) }),
            EditOptions::default()
        ));
        on!(on_set_offset, |app, id, ms: f32| set_audio(
            &app,
            id,
            json!({ "offset_ms": ms.round() as i32 }),
            EditOptions::default()
        ));
        on!(on_set_monitor, |app, id, monitor: SharedString| set_audio(
            &app,
            id,
            json!({ "monitor": monitor.as_str() }),
            EditOptions::default()
        ));
        on!(on_set_program, |app, id, on: bool| set_track(
            &app, "program", id, on
        ));
        on!(on_set_iso, |app, id, on: bool| set_track(
            &app, "iso", id, on
        ));
    }

    fn update(&self, app: &Rc<App>, window: &AppWindow, changed: Changed) {
        if changed.any(Changed::SETUP | Changed::RUNNING | Changed::SCENES) {
            self.show_strips(app, window);
        }
        if changed.any(Changed::SETUP) {
            self.show_rows(app, window);
        }
        // New levels show at once when the meters rest (the timer steps
        // them while they move, so each level isn't a redraw of its own),
        // and always offscreen, whose timers run only between frames.
        if changed.any(Changed::LEVELS) && (!self.timer.running() || app.mode.offscreen) {
            self.step_meters(app);
        }
        if changed.any(Changed::SETUP | Changed::RUNNING | Changed::SCENES | Changed::LEVELS) {
            self.wake(app);
        }
    }

    fn event(&self, _app: &Rc<App>, event: &Event) {
        let levels = match event {
            Event::Levels { levels } => levels,
            Event::Status { live } => &live.levels,
            _ => return,
        };
        let now = crate::clock::now();
        let mut arrived = self.arrived.borrow_mut();
        for level in levels {
            arrived.insert(level.source.clone(), now);
        }
    }

    fn menu(&self, app: &Rc<App>, id: &str) -> bool {
        match id {
            "advanced_audio" => app.open_dialog("advanced-audio", ""),
            _ => return false,
        }
        true
    }

    fn context_menu(&self, app: &Rc<App>, owner: &str) -> Option<MenuBuilder> {
        let unhide = Self::hidden_count(app) > 0;
        if owner == "mixer:dock" {
            return Some(
                MenuBuilder::new()
                    .item("unhide_all", "Unhide All")
                    .disabled(!unhide)
                    .item("advanced_audio", "Advanced Audio Properties"),
            );
        }
        let id = owner.strip_prefix("mixer:strip:")?;
        let monitor = {
            let s = app.session.borrow();
            let setup = s.setup.as_ref()?;
            setup_model::find_source(setup, id)?.audio.monitor
        };
        let mut monitoring = MenuBuilder::new();
        for (value, label) in MONITORING {
            monitoring = monitoring
                .item(&format!("monitor:{value}"), label)
                .checked(monitoring_value(monitor) == value);
        }
        Some(
            MenuBuilder::new()
                .item("hide", "Hide")
                .item("unhide_all", "Unhide All")
                .disabled(!unhide)
                .separator()
                .item("rename", "Rename")
                .submenu("Audio Monitoring", monitoring)
                .item("program_mix", "Include in Program Mix")
                .checked(in_program_mix(app, id))
                .separator()
                .item("filters", "Filters")
                .item("properties", "Properties")
                .item("advanced_audio", "Advanced Audio Properties"),
        )
    }

    fn menu_chosen(&self, app: &Rc<App>, owner: &str, id: &str) -> bool {
        let source = match owner.strip_prefix("mixer:strip:") {
            Some(source) => source,
            None if owner == "mixer:dock" => "",
            None => return false,
        };
        if let Some(value) = id.strip_prefix("monitor:") {
            set_audio(
                app,
                source,
                json!({ "monitor": value }),
                EditOptions::default(),
            );
            return true;
        }
        match id {
            "hide" => self.set_hidden(app, Some(source)),
            "unhide_all" => self.set_hidden(app, None),
            "rename" | "filters" | "properties" => app.open_dialog(id, source),
            "program_mix" => set_track(app, "program", source, !in_program_mix(app, source)),
            "advanced_audio" => app.open_dialog("advanced-audio", ""),
            _ => {}
        }
        true
    }

    fn state(
        &self,
        app: &Rc<App>,
        window: &AppWindow,
        name: &str,
        arg: &str,
    ) -> Option<anyhow::Result<()>> {
        let area = window.global::<MixerArea>();
        match name {
            // A strip's ⋮ menu (the first strip's without a source).
            "mixer-menu" => {
                let id = match arg {
                    "" => match area.get_strips().row_data(0) {
                        Some(strip) => strip.id.to_string(),
                        None => return Some(Err(anyhow::anyhow!("the mixer shows no strips"))),
                    },
                    id => id.to_string(),
                };
                let Some(index) = area.get_strips().iter().position(|s| s.id == id.as_str()) else {
                    return Some(Err(anyhow::anyhow!(
                        "the mixer shows no strip {id:?}; it shows {}",
                        area.get_strips()
                            .iter()
                            .map(|s| s.id.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                };
                area.set_menu_for(id.into());
                area.set_menu_index(index as i32);
                area.set_menu_serial(area.get_menu_serial() + 1);
            }
            // An item of a strip's menu, as if chosen ("tone:hide",
            // "tone:monitor:monitor_only", "tone:program_mix").
            "mixer-choose" => {
                let Some((source, item)) = arg.split_once(':') else {
                    return Some(Err(anyhow::anyhow!(
                        "mixer-choose takes SOURCE:ITEM (hide, unhide_all, rename, monitor:VALUE, program_mix, filters, properties, advanced_audio)"
                    )));
                };
                self.menu_chosen(app, &format!("mixer:strip:{source}"), item);
            }
            // The strip's mute button.
            "mixer-mute" => toggle_mute(app, arg),
            _ => return None,
        }
        Some(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meters_rise_at_once_and_fall_smoothly() {
        let start = crate::clock::now();
        let mut meter = MeterState::new(start);
        meter.step((-20.0, -10.0), 0.016, start);
        assert_eq!((meter.rms, meter.peak, meter.hold), (-20.0, -10.0, -10.0));
        // Half a second of silence: RMS falls 12 dB, the peak 16, the hold
        // stays.
        let later = start + Duration::from_millis(500);
        meter.step((FLOOR, FLOOR), 0.5, later);
        assert!((meter.rms + 32.0).abs() < 1e-9);
        assert!((meter.peak + 26.0).abs() < 1e-9);
        assert_eq!(meter.hold, -10.0);
        // After 1.5 s the hold follows the peak down.
        let after = start + Duration::from_millis(1600);
        meter.step((FLOOR, FLOOR), 0.1, after);
        assert_eq!(meter.hold, meter.peak);
        assert!(meter.active(FLOOR));
    }

    #[test]
    fn balance_is_written_as_obs_does() {
        assert_eq!(balance_text(0.0), "C");
        assert_eq!(balance_text(-0.2), "L20");
        assert_eq!(balance_text(1.0), "R100");
    }
}
