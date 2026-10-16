//! The data store, the daemon's side (docs/scenes-as-code.md, 5.5): it
//! answers `data_get`, `data_set` and `data_delete`, saves kept keys beside
//! the scene collection's file (`collections/<id>.data.json`, debounced
//! and atomic, as setups are saved), loads them at start and when another
//! collection is switched to, and announces changes as `data_changed`
//! events, at most 20 a second however often keys change.

use parking_lot::{Condvar, Mutex};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use isocast_engine::data::{DataStore, Listening};
use isocast_protocol::api::Event;
use isocast_protocol::data::DataStatus;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// At most one `data_changed` event this often.
const EVENT_INTERVAL: Duration = Duration::from_millis(50);
/// Kept keys are saved once no change has come for this long, or at the
/// latest this long after the first unsaved one.
const SAVE_QUIET: Duration = Duration::from_millis(300);
const SAVE_AT_MOST: Duration = Duration::from_secs(1);

/// The file kept keys of the scene collection whose file is `collection`
/// are saved in: beside it (`<id>.toml` → `<id>.data.json`).
pub fn data_path(collection: &Path) -> PathBuf {
    collection.with_extension("data.json")
}

/// What a data file holds.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DataFile {
    version: u32,
    /// The kept keys and their values.
    #[serde(default)]
    values: BTreeMap<String, Value>,
}

/// The data store and what the daemon does around it.
pub struct Data {
    pub store: Arc<DataStore>,
    events: Arc<Events>,
    saving: Arc<Saving>,
    _listening: Listening,
}

/// Keys changed and not yet announced.
#[derive(Default)]
struct Events {
    pending: Mutex<BTreeSet<Arc<str>>>,
    wake: Condvar,
    sent: AtomicU64,
}

/// Kept keys waiting to be saved, and where.
#[derive(Default)]
struct Saving {
    state: Mutex<SaveState>,
    wake: Condvar,
}

#[derive(Default)]
struct SaveState {
    /// Kept keys changed since they were last saved.
    dirty: bool,
    /// Where they go: beside the scene collection in use (none before
    /// there is one; they are saved once there is).
    path: Option<PathBuf>,
    /// Why the last save failed.
    error: Option<String>,
}

impl Default for Data {
    fn default() -> Self {
        let store = DataStore::new();
        let events = Arc::new(Events::default());
        let listening = store.listen({
            let events = events.clone();
            Arc::new(move |keys: &[Arc<str>]| {
                events.pending.lock().extend(keys.iter().cloned());
                events.wake.notify_all();
            })
        });
        Self {
            store,
            events,
            saving: Arc::default(),
            _listening: listening,
        }
    }
}

impl Data {
    /// Start announcing changes with `publish` and saving kept keys.
    pub fn start(&self, publish: Box<dyn Fn(Event) + Send>) {
        let (events, store) = (self.events.clone(), self.store.clone());
        std::thread::Builder::new()
            .name("data events".into())
            .spawn(move || announce(&events, &store, &*publish))
            .expect("could not start the data events thread");
        let (saving, store) = (self.saving.clone(), self.store.clone());
        std::thread::Builder::new()
            .name("data saver".into())
            .spawn(move || save_when_quiet(&saving, &store))
            .expect("could not start the data saver thread");
    }

    /// Where kept keys are saved, once there is a scene collection.
    pub fn path(&self) -> Option<PathBuf> {
        self.saving.state.lock().path.clone()
    }

    /// Kept keys changed: save them soon.
    pub fn changed_kept(&self) {
        self.saving.state.lock().dirty = true;
        self.saving.wake.notify_all();
    }

    /// The scene collection in use has its file at `collection` (or there
    /// is none yet): save kept keys beside it from now on. Kept keys
    /// waiting to be saved go to the file before, first.
    pub fn use_collection(&self, collection: Option<&Path>) {
        self.flush();
        let mut state = self.saving.state.lock();
        state.path = collection.map(data_path);
        if state.dirty {
            self.saving.wake.notify_all();
        }
    }

    /// Load the kept keys of the scene collection whose file is
    /// `collection` (replacing the kept keys in the store), and save beside
    /// it from now on. Returns notes on what couldn't be loaded.
    pub fn load_collection(&self, collection: &Path) -> Vec<String> {
        self.flush();
        let path = data_path(collection);
        let mut notes = Vec::new();
        let values = match read(&path) {
            Ok(values) => values,
            Err(err) => {
                // Kept, never overwritten: moved aside for kept keys set
                // from now on.
                let aside = path.with_extension("broken.json");
                let moved = match aside.exists() {
                    true => Err(std::io::Error::other("one is already there")),
                    false => std::fs::rename(&path, &aside),
                };
                notes.push(match moved {
                    Ok(()) => format!(
                        "the kept data keys in {} couldn't be loaded ({err:#}); the file was moved \
                         to {}",
                        path.display(),
                        aside.display()
                    ),
                    Err(move_err) => format!(
                        "the kept data keys in {} couldn't be loaded ({err:#}), nor the file moved \
                         to {} ({move_err}); kept keys set now will replace it",
                        path.display(),
                        aside.display()
                    ),
                });
                BTreeMap::new()
            }
        };
        let (_, skipped) = self.store.replace_persisted(values);
        for skipped in skipped {
            notes.push(format!("{}: skipped {skipped}", path.display()));
        }
        let mut state = self.saving.state.lock();
        state.path = Some(path);
        state.dirty = false;
        notes
    }

    /// Save kept keys now if any wait to be saved (before exiting, or
    /// before another collection's are loaded).
    pub fn flush(&self) {
        let mut state = self.saving.state.lock();
        if state.dirty
            && let Some(path) = state.path.clone()
        {
            let result = write(&path, self.store.persisted());
            state.dirty = result.is_err();
            state.error = result.err().map(|err| format!("{err:#}"));
        }
    }

    /// The store's counters, events and file, for `status`.
    pub fn status(&self) -> DataStatus {
        let mut status = self.store.status();
        status.events = self.events.sent.load(Ordering::Relaxed);
        let state = self.saving.state.lock();
        status.file = state.path.clone();
        status.save_error = state.error.clone();
        status
    }
}

/// Announce changed keys as `data_changed` events, at most one every
/// [`EVENT_INTERVAL`]: a lone change at once, a flood in batches.
fn announce(events: &Events, store: &DataStore, publish: &dyn Fn(Event)) {
    let mut last: Option<Instant> = None;
    loop {
        let keys = {
            let mut pending = events.pending.lock();
            while pending.is_empty() {
                events.wake.wait(&mut pending);
            }
            if let Some(last) = last {
                let due = last + EVENT_INTERVAL;
                loop {
                    let now = Instant::now();
                    if now >= due {
                        break;
                    }
                    events.wake.wait_for(&mut pending, due - now);
                }
            }
            std::mem::take(&mut *pending)
        };
        last = Some(Instant::now());
        publish(Event::DataChanged {
            revision: store.revision(),
            keys: keys.iter().map(|key| key.to_string()).collect(),
        });
        events.sent.fetch_add(1, Ordering::Relaxed);
    }
}

/// Save kept keys once changes pause, where the collection in use keeps
/// them.
fn save_when_quiet(saving: &Saving, store: &DataStore) {
    loop {
        let mut state = saving.state.lock();
        while !(state.dirty && state.path.is_some()) {
            saving.wake.wait(&mut state);
        }
        let first = Instant::now();
        loop {
            let wait = saving.wake.wait_for(&mut state, SAVE_QUIET);
            if wait.timed_out() || first.elapsed() >= SAVE_AT_MOST {
                break;
            }
        }
        // Flushed meanwhile, or the collection changed.
        let Some(path) = state.path.clone().filter(|_| state.dirty) else {
            continue;
        };
        state.dirty = false;
        drop(state);
        // Written outside the lock: setting keys never waits for the disk.
        let result = write(&path, store.persisted());
        let mut state = saving.state.lock();
        match result {
            Ok(()) => state.error = None,
            Err(err) => {
                isocast_diagnostics::diagnostic!("error"; "isocastd: {err:#}");
                state.error = Some(format!("{err:#}"));
                state.dirty = true;
                drop(state);
                // Not again at once: tried again a second later.
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

/// The kept keys in `path`; none if it doesn't exist.
fn read(path: &Path) -> Result<BTreeMap<String, Value>> {
    let text = match std::fs::read(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(err) => return Err(err).with_context(|| format!("could not read {}", path.display())),
    };
    let file: DataFile = serde_json::from_slice(&text)
        .with_context(|| format!("{} isn't a data file", path.display()))?;
    if file.version != 1 {
        bail!("{} is version {}; expected 1", path.display(), file.version);
    }
    Ok(file.values)
}

/// Write `values` to `path` atomically (the file before kept as .bak).
fn write(path: &Path, values: BTreeMap<String, Value>) -> Result<()> {
    let mut text = serde_json::to_vec_pretty(&DataFile { version: 1, values })?;
    text.push(b'\n');
    if std::fs::read(path).is_ok_and(|old| old == text) {
        return Ok(());
    }
    crate::store::write_atomically(path, &text)
        .with_context(|| format!("could not save the kept data keys to {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn kept_keys_are_saved_beside_the_collection_and_loaded_back() {
        let folder = std::env::temp_dir().join(format!("isocast-data-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        let collection = folder.join("collections/default.toml");
        let data = Data::default();
        data.use_collection(Some(&collection));
        let values = |pairs: &[(&str, Value)]| -> BTreeMap<String, Value> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect()
        };
        data.store
            .set(values(&[("kept", json!("yes"))]), Some(true))
            .unwrap();
        data.store
            .set(values(&[("gone", json!("no"))]), None)
            .unwrap();
        data.changed_kept();
        data.flush();
        assert_eq!(
            read(&data_path(&collection)).unwrap(),
            values(&[("kept", json!("yes"))])
        );
        // As after a restart.
        let again = Data::default();
        assert!(again.load_collection(&collection).is_empty());
        assert_eq!(again.store.text("kept"), "yes");
        assert_eq!(again.store.text("gone"), "");
        assert_eq!(again.status().persisted, 1);
        // Another collection's kept keys replace these.
        let other = folder.join("collections/other.toml");
        again.load_collection(&other);
        assert_eq!(again.store.text("kept"), "");
        let _ = std::fs::remove_dir_all(&folder);
    }
}
