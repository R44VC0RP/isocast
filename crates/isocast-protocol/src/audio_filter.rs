//! Audio filters on a source's sound, as OBS has them, by `kind`. The
//! engine runs a source's enabled audio filters in order before the program
//! mix (and in ISO files with `recording.iso.filtered`).
//!
//! To add one: a settings struct whose doc comments describe each field
//! (they become the JSON Schema), an `impl Check`, and a line in
//! `audio_filter_kinds!` at the bottom; then its processor in the engine's
//! `audio_filter` module.

use std::collections::HashSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::filter::{Check, is_true, within, yes};
use crate::{Issues, Source, is_valid_id};

/// One filter in a source's audio chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(remote = "Self")]
pub struct AudioFilter {
    /// Identifies the filter for patches; its kind if absent. Unique within
    /// its source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// A filter that is off is skipped.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(flatten)]
    pub kind: AudioFilterKind,
    /// Fields its kind doesn't have, kept for validation to report.
    #[serde(flatten)]
    #[schemars(skip)]
    pub unknown: crate::UnknownFields,
}

impl serde::Serialize for AudioFilter {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        AudioFilter::serialize(self, serializer)
    }
}

/// As [`crate::fields::keep_unknown_fields`], except that a plugin's kind
/// keeps all its fields as its params (the setup's checks know none).
impl<'de> serde::Deserialize<'de> for AudioFilter {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <serde_json::Value as serde::Deserialize>::deserialize(deserializer)?;
        let mut read =
            crate::located::read(&value, |de| AudioFilter::deserialize(de)).map_err(|err| {
                crate::located::kinded(err, &value, AudioFilterKind::NAMES, AudioFilterKind::locate)
                    .into_error()
            })?;
        match &read.kind {
            AudioFilterKind::Plugin(_) => read.unknown.clear(),
            kind => crate::fields::keep_unknown(&mut read.unknown, kind.fields()),
        }
        Ok(read)
    }
}

impl AudioFilter {
    /// What patches call it: its id, else its kind.
    pub fn key(&self) -> &str {
        self.id.as_deref().unwrap_or(self.kind.label())
    }

    /// The source whose sound drives it instead of its own, if any.
    pub fn sidechain(&self) -> Option<&str> {
        match &self.kind {
            AudioFilterKind::Compressor(compressor) => compressor.sidechain_source.as_deref(),
            AudioFilterKind::Plugin(plugin) => plugin
                .params
                .get(crate::plugin::SIDECHAIN_PARAM)
                .and_then(serde_json::Value::as_str),
            _ => None,
        }
    }
}

/// Raises or lowers the volume (OBS's Gain).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Gain {
    /// −30 to 30 dB.
    #[serde(default)]
    pub gain_db: f32,
}

impl Check for Gain {
    fn check(&self, path: &str, issues: &mut Issues) {
        within(issues, path, "gain_db", self.gain_db, -30.0, 30.0);
    }
}

/// Flips the waveform upside down, to fix a microphone that cancels
/// another out (OBS's Invert Polarity).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct InvertPolarity {}

impl Check for InvertPolarity {}

/// Silences the sound while it is quieter than a threshold, cutting
/// background noise between words (OBS's Noise Gate).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NoiseGate {
    /// −96 to 0 dBFS: an open gate closes when the sound falls below this.
    /// Set it above the background noise.
    #[serde(default = "gate_close")]
    pub close_threshold_db: f32,
    /// −96 to 0 dBFS: a closed gate opens when the sound rises above this.
    /// Set it a little below your voice.
    #[serde(default = "gate_open")]
    pub open_threshold_db: f32,
    /// 1 to 10000 ms to fade in when opening.
    #[serde(default = "gate_attack")]
    pub attack_ms: f32,
    /// 0 to 10000 ms to stay open after the sound falls below the close
    /// threshold, so pauses between words don't close it.
    #[serde(default = "gate_hold")]
    pub hold_ms: f32,
    /// 1 to 10000 ms to fade out when closing.
    #[serde(default = "gate_release")]
    pub release_ms: f32,
}

impl Default for NoiseGate {
    fn default() -> Self {
        Self {
            close_threshold_db: gate_close(),
            open_threshold_db: gate_open(),
            attack_ms: gate_attack(),
            hold_ms: gate_hold(),
            release_ms: gate_release(),
        }
    }
}

impl Check for NoiseGate {
    fn check(&self, path: &str, issues: &mut Issues) {
        let (close, open) = (self.close_threshold_db, self.open_threshold_db);
        within(issues, path, "close_threshold_db", close, -96.0, 0.0);
        within(issues, path, "open_threshold_db", open, -96.0, 0.0);
        within(issues, path, "attack_ms", self.attack_ms, 1.0, 10_000.0);
        within(issues, path, "hold_ms", self.hold_ms, 0.0, 10_000.0);
        within(issues, path, "release_ms", self.release_ms, 1.0, 10_000.0);
        if close > open {
            issues.add(
                format!("{path}.close_threshold_db"),
                format!(
                    "{close} dB is above open_threshold_db ({open} dB), so the gate would \
                     flutter open and shut; make it lower"
                ),
            );
        }
    }
}

/// Removes background noise (fans, hum, hiss) while keeping voices, with
/// RNNoise, a small neural network (OBS's Noise Suppression, RNNoise
/// method). It works 20 ms behind, which the program mix and filtered ISO
/// files make up for, so the sound stays in sync.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NoiseSuppression {}

impl Check for NoiseSuppression {}

/// Turns down the sound above a threshold, evening out loud and quiet
/// parts; or, with a sidechain source, turns this source down while that
/// one makes sound (ducking music under a voice) (OBS's Compressor).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Compressor {
    /// 1 to 32: how much sound above the threshold is reduced; at 4, 8 dB
    /// over comes out 2 dB over.
    #[serde(default = "compressor_ratio")]
    pub ratio: f32,
    /// −60 to 0 dBFS: where reduction starts.
    #[serde(default = "compressor_threshold")]
    pub threshold_db: f32,
    /// 1 to 500 ms to react when the sound gets louder.
    #[serde(default = "compressor_attack")]
    pub attack_ms: f32,
    /// 1 to 1000 ms to recover when it gets quieter.
    #[serde(default = "compressor_release")]
    pub release_ms: f32,
    /// −32 to 32 dB added afterwards to bring the level back up; not
    /// applied when ducking with a sidechain source.
    #[serde(default)]
    pub output_gain_db: f32,
    /// The id of another audio source whose sound drives the reduction
    /// instead of this one's (ducking).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidechain_source: Option<String>,
}

impl Default for Compressor {
    fn default() -> Self {
        Self {
            ratio: compressor_ratio(),
            threshold_db: compressor_threshold(),
            attack_ms: compressor_attack(),
            release_ms: compressor_release(),
            output_gain_db: 0.0,
            sidechain_source: None,
        }
    }
}

impl Check for Compressor {
    fn check(&self, path: &str, issues: &mut Issues) {
        within(issues, path, "ratio", self.ratio, 1.0, 32.0);
        within(issues, path, "threshold_db", self.threshold_db, -60.0, 0.0);
        within(issues, path, "attack_ms", self.attack_ms, 1.0, 500.0);
        within(issues, path, "release_ms", self.release_ms, 1.0, 1000.0);
        within(
            issues,
            path,
            "output_gain_db",
            self.output_gain_db,
            -32.0,
            32.0,
        );
    }
}

/// Keeps peaks from ever going over a level (OBS's Limiter). Best last in
/// the chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Limiter {
    /// −60 to 0 dBFS: no sample comes out louder than this.
    #[serde(default = "limiter_threshold")]
    pub threshold_db: f32,
    /// 1 to 1000 ms to recover after a peak.
    #[serde(default = "limiter_release")]
    pub release_ms: f32,
}

impl Default for Limiter {
    fn default() -> Self {
        Self {
            threshold_db: limiter_threshold(),
            release_ms: limiter_release(),
        }
    }
}

impl Check for Limiter {
    fn check(&self, path: &str, issues: &mut Issues) {
        within(issues, path, "threshold_db", self.threshold_db, -60.0, 0.0);
        within(issues, path, "release_ms", self.release_ms, 1.0, 1000.0);
    }
}

/// How a level-driven filter measures the sound.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Detection {
    /// The average power over the last 10 ms: steadier, a tone reads 3 dB
    /// under its peak.
    #[default]
    Rms,
    /// The highest sample: quicker to react.
    Peak,
}

/// Turns down the sound below a threshold, more the quieter it is: a
/// gentler noise gate (OBS's Expander).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Expander {
    /// 1 to 20: at 2, sound 10 dB under the threshold comes out 20 dB
    /// under; high ratios act like a gate.
    #[serde(default = "expander_ratio")]
    pub ratio: f32,
    /// −60 to 0 dBFS: sound above this is left alone.
    #[serde(default = "expander_threshold")]
    pub threshold_db: f32,
    /// 1 to 100 ms to open up when the sound gets louder.
    #[serde(default = "expander_attack")]
    pub attack_ms: f32,
    /// 1 to 1000 ms to turn down when it gets quieter.
    #[serde(default = "expander_release")]
    pub release_ms: f32,
    /// −32 to 32 dB added afterwards.
    #[serde(default)]
    pub output_gain_db: f32,
    #[serde(default)]
    pub detection: Detection,
}

impl Default for Expander {
    fn default() -> Self {
        Self {
            ratio: expander_ratio(),
            threshold_db: expander_threshold(),
            attack_ms: expander_attack(),
            release_ms: expander_release(),
            output_gain_db: 0.0,
            detection: Detection::default(),
        }
    }
}

impl Check for Expander {
    fn check(&self, path: &str, issues: &mut Issues) {
        within(issues, path, "ratio", self.ratio, 1.0, 20.0);
        within(issues, path, "threshold_db", self.threshold_db, -60.0, 0.0);
        within(issues, path, "attack_ms", self.attack_ms, 1.0, 100.0);
        within(issues, path, "release_ms", self.release_ms, 1.0, 1000.0);
        within(
            issues,
            path,
            "output_gain_db",
            self.output_gain_db,
            -32.0,
            32.0,
        );
    }
}

/// Turns up the sound below a threshold, bringing quiet parts closer to
/// loud ones (OBS's Upward Compressor). Sound under −60 dBFS is raised no
/// more than sound at −60 dBFS, so silence and hiss aren't raised without
/// limit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct UpwardCompressor {
    /// 1 to 20: at 2, sound 20 dB under the threshold comes out 10 dB
    /// under.
    #[serde(default = "upward_ratio")]
    pub ratio: f32,
    /// −60 to 0 dBFS: sound above this is left alone.
    #[serde(default = "upward_threshold")]
    pub threshold_db: f32,
    /// 1 to 100 ms to back off when the sound gets louder.
    #[serde(default = "expander_attack")]
    pub attack_ms: f32,
    /// 1 to 1000 ms to raise it when it gets quieter.
    #[serde(default = "expander_release")]
    pub release_ms: f32,
    /// −32 to 32 dB added afterwards.
    #[serde(default)]
    pub output_gain_db: f32,
    #[serde(default)]
    pub detection: Detection,
}

impl Default for UpwardCompressor {
    fn default() -> Self {
        Self {
            ratio: upward_ratio(),
            threshold_db: upward_threshold(),
            attack_ms: expander_attack(),
            release_ms: expander_release(),
            output_gain_db: 0.0,
            detection: Detection::default(),
        }
    }
}

impl Check for UpwardCompressor {
    fn check(&self, path: &str, issues: &mut Issues) {
        within(issues, path, "ratio", self.ratio, 1.0, 20.0);
        within(issues, path, "threshold_db", self.threshold_db, -60.0, 0.0);
        within(issues, path, "attack_ms", self.attack_ms, 1.0, 100.0);
        within(issues, path, "release_ms", self.release_ms, 1.0, 1000.0);
        within(
            issues,
            path,
            "output_gain_db",
            self.output_gain_db,
            -32.0,
            32.0,
        );
    }
}

/// Turns the lows, mids and highs up or down (OBS's 3-Band Equalizer).
/// The bands meet at 250 Hz and 4 kHz; at 0 dB each, the sound is
/// unchanged.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Equalizer {
    /// −20 to 20 dB below 250 Hz.
    #[serde(default)]
    pub low_db: f32,
    /// −20 to 20 dB from 250 Hz to 4 kHz.
    #[serde(default)]
    pub mid_db: f32,
    /// −20 to 20 dB above 4 kHz.
    #[serde(default)]
    pub high_db: f32,
}

impl Check for Equalizer {
    fn check(&self, path: &str, issues: &mut Issues) {
        within(issues, path, "low_db", self.low_db, -20.0, 20.0);
        within(issues, path, "mid_db", self.mid_db, -20.0, 20.0);
        within(issues, path, "high_db", self.high_db, -20.0, 20.0);
    }
}

/// Check source `source`'s audio filters (at `path`, like `sources[2]`):
/// that it has sound, ids, settings, and sidechain sources among `audio`.
pub(crate) fn check_chain(path: &str, source: &Source, audio: &[&str], issues: &mut Issues) {
    if !source.audio_filters.is_empty() && !source.kind.has_audio() {
        issues.add(
            format!("{path}.audio_filters"),
            format!(
                "audio filters change sound, and {:?} has none; remove them",
                source.id
            ),
        );
    }
    let mut keys = HashSet::new();
    for (j, filter) in source.audio_filters.iter().enumerate() {
        let path = format!("{path}.audio_filters[{j}]");
        if let Some(id) = &filter.id
            && !is_valid_id(id)
        {
            issues.add(
                format!("{path}.id"),
                format!("{id:?} must use lowercase letters, digits and dashes"),
            );
        }
        if !keys.insert(filter.key()) {
            issues.add(
                format!("{path}.id"),
                format!(
                    "another audio filter of this source is already called {:?}; give this \
                     one its own id",
                    filter.key()
                ),
            );
        }
        filter.kind.check(&path, issues);
        crate::fields::report(
            issues,
            &path,
            &filter.unknown,
            &format!("a {} audio filter", filter.kind.label()),
            filter.kind.fields(),
            crate::filter::FILTER_FIELDS,
        );
        if let Some(sidechain) = filter.sidechain() {
            let others: Vec<&str> = audio
                .iter()
                .copied()
                .filter(|id| *id != source.id)
                .collect();
            if !others.contains(&sidechain) {
                issues.add(
                    format!("{path}.sidechain_source"),
                    format!(
                        "{sidechain:?} is not another audio source; valid: {}",
                        others.join(", ")
                    ),
                );
            }
        }
    }
}

fn gate_close() -> f32 {
    -32.0
}

fn gate_open() -> f32 {
    -26.0
}

fn gate_attack() -> f32 {
    25.0
}

fn gate_hold() -> f32 {
    200.0
}

fn gate_release() -> f32 {
    150.0
}

fn compressor_ratio() -> f32 {
    10.0
}

fn compressor_threshold() -> f32 {
    -18.0
}

fn compressor_attack() -> f32 {
    6.0
}

fn compressor_release() -> f32 {
    60.0
}

fn limiter_threshold() -> f32 {
    -6.0
}

fn limiter_release() -> f32 {
    60.0
}

fn expander_ratio() -> f32 {
    2.0
}

fn expander_threshold() -> f32 {
    -40.0
}

fn expander_attack() -> f32 {
    10.0
}

fn expander_release() -> f32 {
    50.0
}

fn upward_ratio() -> f32 {
    2.0
}

fn upward_threshold() -> f32 {
    -20.0
}

macro_rules! audio_filter_kinds {
    ($($name:literal => $variant:ident($settings:ident),)*) => {
        /// What an audio filter does, by `kind`: a built-in kind, or an
        /// installed plugin's (`<plugin id>/<kind id>`, its params beside
        /// it; `isocast plugin list` lists them).
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
        #[serde(remote = "Self", tag = "kind")]
        pub enum AudioFilterKind {
            $(#[serde(rename = $name)] $variant($settings),)*
            /// A plugin's audio filter (a WebAssembly component), kept as
            /// given: bypassed where the plugin isn't installed.
            #[serde(skip)]
            #[schemars(skip)]
            Plugin(crate::plugin::PluginKind),
        }

        impl serde::Serialize for AudioFilterKind {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                match self {
                    Self::Plugin(plugin) => plugin.serialize(serializer),
                    builtin => AudioFilterKind::serialize(builtin, serializer),
                }
            }
        }

        impl<'de> serde::Deserialize<'de> for AudioFilterKind {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                use serde::de::Error;
                let value = <serde_json::Value as serde::Deserialize>::deserialize(deserializer)?;
                if let Some(plugin) = crate::plugin::PluginKind::from_value(&value) {
                    return Ok(Self::Plugin(plugin));
                }
                AudioFilterKind::deserialize(value).map_err(|err| {
                    let message = err.to_string();
                    match message.starts_with("unknown variant") {
                        true => D::Error::custom(format!(
                            "{message}, or a plugin's audio filter, `<plugin id>/<kind id>`"
                        )),
                        false => D::Error::custom(message),
                    }
                })
            }
        }

        impl AudioFilterKind {
            /// Every built-in kind's name, as setups write it.
            pub const NAMES: &[&str] = &[$($name),*];

            /// Where `value`, an object of the kind named `name`, is wrong
            /// in that kind's fields (see [`crate::located`]).
            pub(crate) fn locate(
                name: &str,
                value: &serde_json::Value,
            ) -> Option<crate::located::Located> {
                match name {
                    $($name => crate::located::from_value::<$settings>(value).err(),)*
                    _ => None,
                }
            }

            /// The kind's name, as setups write it.
            pub fn label(&self) -> &str {
                match self {
                    $(Self::$variant(_) => $name,)*
                    Self::Plugin(plugin) => &plugin.kind,
                }
            }

            /// Check the kind's settings for the filter at `path` (a
            /// plugin's only by its name: isocastd checks its params
            /// against the plugin's manifest).
            pub(crate) fn check(&self, path: &str, issues: &mut Issues) {
                match self {
                    $(Self::$variant(settings) => settings.check(path, issues),)*
                    Self::Plugin(plugin) => {
                        if let Some(message) = plugin.name_problem() {
                            issues.add(format!("{path}.kind"), message);
                        }
                    }
                }
            }

            /// The kind's own fields, as setups write them (none known for
            /// a plugin's).
            pub fn fields(&self) -> &'static [String] {
                static FIELDS: std::sync::OnceLock<Vec<Vec<String>>> = std::sync::OnceLock::new();
                let table = FIELDS.get_or_init(|| vec![$(crate::fields::names::<$settings>()),*]);
                match Self::NAMES.iter().position(|name| *name == self.label()) {
                    Some(index) => &table[index],
                    None => &[],
                }
            }
        }
    };
}

audio_filter_kinds! {
    "gain" => Gain(Gain),
    "invert_polarity" => InvertPolarity(InvertPolarity),
    "noise_gate" => NoiseGate(NoiseGate),
    "noise_suppression" => NoiseSuppression(NoiseSuppression),
    "compressor" => Compressor(Compressor),
    "limiter" => Limiter(Limiter),
    "expander" => Expander(Expander),
    "upward_compressor" => UpwardCompressor(UpwardCompressor),
    "equalizer" => Equalizer(Equalizer),
}

#[cfg(test)]
mod tests {
    use crate::Setup;

    #[test]
    fn audio_filters_are_checked_for_sound_ranges_and_sidechains() {
        let setup: Setup = toml::from_str(
            r##"
            version = 1
            name = "t"
            canvas = { width = 16, height = 16, fps = 30 }
            [[sources]]
            id = "pattern"
            kind = "test_pattern"
            audio_filters = [{ kind = "gain" }]
            [[sources]]
            id = "music"
            kind = "tone"
            audio_filters = [
              { kind = "compressor", sidechain_source = "pattern" },
              { id = "duck", kind = "compressor", sidechain_source = "voice", ratio = 40 },
              { kind = "limiter" },
            ]
            [[sources]]
            id = "voice"
            kind = "tone"
            [recording]
            folder = "."
            program = { enabled = false, encoder = { codec = "hevc" } }
            "##,
        )
        .unwrap();
        let found = setup.validate();
        let at = |path: &str| found.iter().find(|i| i.path == path).map(|i| &i.message);
        assert!(at("sources[0].audio_filters").is_some(), "{found:?}");
        let sidechain = at("sources[1].audio_filters[0].sidechain_source").unwrap();
        assert!(sidechain.ends_with("valid: voice"), "{sidechain}");
        assert!(at("sources[1].audio_filters[1].ratio").is_some());
        assert_eq!(found.len(), 3, "{found:?}");
    }
}
