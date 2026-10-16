//! The data store (scenes as code, 5.5): JSON values by key, apart from
//! the setup, that bound text shows (`data = "key"`). The daemon owns one,
//! answers `data_*` calls from it and saves its kept keys; the engine reads
//! it and listens for changes, so a text showing a key is drawn again when
//! its value changes, never per frame.

use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Instant;

use isocast_protocol::api::Percentiles;
use isocast_protocol::data::{
    DataGetParams, DataRevision, DataValues, MAX_TOTAL_BYTES, MAX_VALUE_BYTES, key_problem,
    text_of, under, value_bytes,
};
use serde_json::Value;

/// How many `set` timings are kept for their percentiles.
const TIMINGS: usize = 1000;

/// Called with the keys that changed, after they did (outside the store's
/// lock), on the thread that changed them: it should only note them and
/// wake whoever draws them.
pub type OnChange = dyn Fn(&[Arc<str>]) + Send + Sync;

/// The data store. Cheap to read; every change is one short lock.
#[derive(Default)]
pub struct DataStore {
    values: Mutex<Values>,
    listeners: Mutex<Vec<(u64, Weak<OnChange>)>>,
    next_listener: AtomicU64,
}

#[derive(Default)]
struct Values {
    map: BTreeMap<Arc<str>, Entry>,
    revision: u64,
    /// What every key and value takes, as JSON.
    bytes: usize,
    persisted: usize,
    /// Calls that changed something.
    writes: u64,
    /// The last [`TIMINGS`] `set` calls' times in the store, in
    /// nanoseconds (a ring).
    timings: Vec<u32>,
    next_timing: usize,
}

struct Entry {
    value: Arc<Value>,
    /// The key's and the value's size, as JSON.
    bytes: usize,
    persist: bool,
}

/// Why a change was refused: the field it is about (`values["title"]`)
/// and the rule or limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

impl std::error::Error for Refused {}

/// A listener; it stops listening when dropped.
pub struct Listening {
    store: Weak<DataStore>,
    id: u64,
    _callback: Arc<OnChange>,
}

impl Drop for Listening {
    fn drop(&mut self) {
        if let Some(store) = self.store.upgrade() {
            store.listeners.lock().retain(|(id, _)| *id != self.id);
        }
    }
}

/// What a change did: the store's revision after it, the keys whose value
/// or keeping changed, and whether any kept key did (to save).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changed {
    pub revision: u64,
    pub keys: Vec<Arc<str>>,
    pub persisted: bool,
}

impl Changed {
    pub fn reply(&self) -> DataRevision {
        DataRevision {
            revision: self.revision,
            changed: self.keys.iter().map(|key| key.to_string()).collect(),
        }
    }
}

/// The path that names `key` in a request's `values` or `keys`.
fn path_of(field: &str, key: &str) -> String {
    format!("{field}[{key:?}]")
}

impl DataStore {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// Call `on_change` with the keys that change from now on, until the
    /// returned guard is dropped.
    pub fn listen(self: &Arc<Self>, on_change: Arc<OnChange>) -> Listening {
        let id = self.next_listener.fetch_add(1, Ordering::Relaxed);
        self.listeners.lock().push((id, Arc::downgrade(&on_change)));
        Listening {
            store: Arc::downgrade(self),
            id,
            _callback: on_change,
        }
    }

    fn tell(&self, keys: &[Arc<str>]) {
        if keys.is_empty() {
            return;
        }
        let listeners: Vec<Arc<OnChange>> = self
            .listeners
            .lock()
            .iter()
            .filter_map(|(_, listener)| listener.upgrade())
            .collect();
        for listener in listeners {
            listener(keys);
        }
    }

    /// The store's revision: it counts changes.
    pub fn revision(&self) -> u64 {
        self.values.lock().revision
    }

    /// Count changes on from `revision` (if it is higher), so revisions
    /// never repeat across runs (the daemon starts them at the time).
    pub fn start_at(&self, revision: u64) {
        let mut values = self.values.lock();
        values.revision = values.revision.max(revision);
    }

    /// The value of `key`, if it is set.
    pub fn get(&self, key: &str) -> Option<Arc<Value>> {
        self.values
            .lock()
            .map
            .get(key)
            .map(|entry| entry.value.clone())
    }

    /// What text bound to `key` shows (see [`text_of`]).
    pub fn text(&self, key: &str) -> String {
        text_of(self.get(key).as_deref())
    }

    /// The values `params` ask for.
    pub fn read(&self, params: &DataGetParams) -> Result<DataValues, Refused> {
        for (i, key) in params.keys.iter().enumerate() {
            if let Some(problem) = key_problem(key) {
                return Err(Refused {
                    path: format!("keys[{i}]"),
                    message: problem,
                });
            }
        }
        let values = self.values.lock();
        let mut out = DataValues {
            revision: values.revision,
            ..DataValues::default()
        };
        let mut add = |key: &Arc<str>, entry: &Entry| {
            out.values.insert(key.to_string(), (*entry.value).clone());
            if entry.persist {
                out.persisted.push(key.to_string());
            }
        };
        if !params.keys.is_empty() {
            for key in &params.keys {
                match values.map.get_key_value(key.as_str()) {
                    Some((key, entry)) => add(key, entry),
                    None => out.missing.push(key.clone()),
                }
            }
            if let Some(prefix) = &params.prefix {
                out.values.retain(|key, _| under(key, prefix));
            }
            return Ok(out);
        }
        let prefix = params.prefix.as_deref().unwrap_or("");
        // Keys are sorted, so those under a prefix are together.
        let start = prefix.trim_end_matches('.');
        let from = (Bound::Included(start), Bound::Unbounded);
        for (key, entry) in values.map.range::<str, _>(from) {
            if !key.starts_with(start) {
                break;
            }
            if under(key, prefix) {
                add(key, entry);
            }
        }
        Ok(out)
    }

    /// Set `values` together (all, or none if one is refused), keeping
    /// them (`persist` true), no longer keeping them (false), or as they
    /// were (absent; new keys aren't kept). Listeners hear of the keys
    /// whose value or keeping changed.
    pub fn set(
        &self,
        values: BTreeMap<String, Value>,
        persist: Option<bool>,
    ) -> Result<Changed, Refused> {
        let began = Instant::now();
        let mut sizes = Vec::with_capacity(values.len());
        for (key, value) in &values {
            if let Some(problem) = key_problem(key) {
                return Err(Refused {
                    path: path_of("values", key),
                    message: problem,
                });
            }
            let size = value_bytes(value);
            if size > MAX_VALUE_BYTES {
                return Err(Refused {
                    path: path_of("values", key),
                    message: format!(
                        "the value is {size} bytes as JSON, over the limit of {MAX_VALUE_BYTES} \
                         bytes (64 KB) for one value"
                    ),
                });
            }
            sizes.push(key.len() + size);
        }
        let changed = {
            let mut store = self.values.lock();
            let mut total = store.bytes;
            for ((key, _), size) in values.iter().zip(&sizes) {
                let old = store.map.get(key.as_str()).map_or(0, |entry| entry.bytes);
                total = total - old + size;
            }
            if total > MAX_TOTAL_BYTES {
                return Err(Refused {
                    path: "values".into(),
                    message: format!(
                        "the data store would hold {total} bytes as JSON, over its limit of \
                         {MAX_TOTAL_BYTES} bytes (16 MB); delete keys it no longer needs \
                         (data_delete)"
                    ),
                });
            }
            let mut changed = Changed::default();
            for ((key, value), size) in values.into_iter().zip(sizes) {
                let (unchanged, was_kept) = match store.map.get(key.as_str()) {
                    Some(entry) => (*entry.value == value, Some(entry.persist)),
                    None => (false, None),
                };
                let keep = persist.or(was_kept).unwrap_or(false);
                if unchanged && was_kept == Some(keep) {
                    continue;
                }
                if keep || was_kept == Some(true) {
                    changed.persisted = true;
                }
                let key: Arc<str> = key.into();
                let old = store.map.insert(
                    key.clone(),
                    Entry {
                        value: Arc::new(value),
                        bytes: size,
                        persist: keep,
                    },
                );
                if let Some(old) = old {
                    store.bytes -= old.bytes;
                    store.persisted -= usize::from(old.persist);
                }
                store.bytes += size;
                store.persisted += usize::from(keep);
                changed.keys.push(key);
            }
            if !changed.keys.is_empty() {
                store.revision += 1;
                store.writes += 1;
            }
            changed.revision = store.revision;
            let nanos = began.elapsed().as_nanos().min(u128::from(u32::MAX)) as u32;
            if store.timings.len() < TIMINGS {
                store.timings.push(nanos);
            } else {
                let at = store.next_timing;
                store.timings[at] = nanos;
            }
            store.next_timing = (store.next_timing + 1) % TIMINGS;
            changed
        };
        self.tell(&changed.keys);
        Ok(changed)
    }

    /// Delete `keys` (those not set are skipped).
    pub fn delete(&self, keys: &[String]) -> Result<Changed, Refused> {
        for (i, key) in keys.iter().enumerate() {
            if let Some(problem) = key_problem(key) {
                return Err(Refused {
                    path: format!("keys[{i}]"),
                    message: problem,
                });
            }
        }
        let changed = {
            let mut store = self.values.lock();
            let mut changed = Changed::default();
            for key in keys {
                if let Some((key, entry)) = store.map.remove_entry(key.as_str()) {
                    store.bytes -= entry.bytes;
                    store.persisted -= usize::from(entry.persist);
                    changed.persisted |= entry.persist;
                    changed.keys.push(key);
                }
            }
            if !changed.keys.is_empty() {
                store.revision += 1;
                store.writes += 1;
            }
            changed.revision = store.revision;
            changed
        };
        self.tell(&changed.keys);
        Ok(changed)
    }

    /// The kept keys and their values, to save.
    pub fn persisted(&self) -> BTreeMap<String, Value> {
        self.values
            .lock()
            .map
            .iter()
            .filter(|(_, entry)| entry.persist)
            .map(|(key, entry)| (key.to_string(), (*entry.value).clone()))
            .collect()
    }

    /// Replace the kept keys with `values` (as loaded from a scene
    /// collection's file): kept keys not in it are deleted, and keys that
    /// were set without keeping take the loaded value. Values that break a
    /// rule are skipped, with why. Listeners hear of every key that
    /// changed.
    pub fn replace_persisted(&self, values: BTreeMap<String, Value>) -> (Changed, Vec<String>) {
        let mut skipped = Vec::new();
        let changed = {
            let mut store = self.values.lock();
            let mut changed = Changed::default();
            let gone: Vec<Arc<str>> = store
                .map
                .iter()
                .filter(|(key, entry)| entry.persist && !values.contains_key(&***key))
                .map(|(key, _)| key.clone())
                .collect();
            for key in gone {
                if let Some(entry) = store.map.remove(&key) {
                    store.bytes -= entry.bytes;
                    store.persisted -= 1;
                    changed.keys.push(key);
                }
            }
            for (key, value) in values {
                if let Some(problem) = key_problem(&key) {
                    skipped.push(problem);
                    continue;
                }
                let size = key.len() + value_bytes(&value);
                if size > MAX_VALUE_BYTES + key.len() || store.bytes + size > MAX_TOTAL_BYTES {
                    skipped.push(format!("{key:?}: over the data store's size limits"));
                    continue;
                }
                if let Some(entry) = store.map.get(key.as_str())
                    && *entry.value == value
                    && entry.persist
                {
                    continue;
                }
                let key: Arc<str> = key.into();
                if let Some(old) = store.map.insert(
                    key.clone(),
                    Entry {
                        value: Arc::new(value),
                        bytes: size,
                        persist: true,
                    },
                ) {
                    store.bytes -= old.bytes;
                    store.persisted -= usize::from(old.persist);
                }
                store.bytes += size;
                store.persisted += 1;
                changed.keys.push(key);
            }
            if !changed.keys.is_empty() {
                store.revision += 1;
            }
            changed.revision = store.revision;
            changed
        };
        self.tell(&changed.keys);
        (changed, skipped)
    }

    /// Its size and counters, for `status` (the daemon adds events and
    /// its file).
    pub fn status(&self) -> isocast_protocol::data::DataStatus {
        let (mut status, mut timings) = {
            let values = self.values.lock();
            let status = isocast_protocol::data::DataStatus {
                revision: values.revision,
                keys: values.map.len() as u64,
                bytes: values.bytes as u64,
                persisted: values.persisted as u64,
                writes: values.writes,
                ..Default::default()
            };
            (status, values.timings.clone())
        };
        timings.sort_unstable();
        let at = |q: f64| {
            if timings.is_empty() {
                return 0.0;
            }
            let index = ((timings.len() - 1) as f64 * q).round() as usize;
            (f64::from(timings[index]) / 100.0).round() / 10.0
        };
        status.set_us = Percentiles {
            p50: at(0.5),
            p99: at(0.99),
        };
        status
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn values(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.clone()))
            .collect()
    }

    #[test]
    fn sets_gets_and_deletes_by_key() {
        let store = DataStore::new();
        let heard = Arc::new(Mutex::new(Vec::<String>::new()));
        let _listening = store.listen({
            let heard = heard.clone();
            Arc::new(move |keys: &[Arc<str>]| {
                heard.lock().extend(keys.iter().map(|k| k.to_string()))
            })
        });
        let changed = store
            .set(
                values(&[("title", json!("Hello")), ("score.home", json!(3))]),
                None,
            )
            .unwrap();
        assert_eq!(changed.revision, 1);
        assert_eq!(changed.keys.len(), 2);
        assert!(!changed.persisted);
        assert_eq!(store.text("title"), "Hello");
        assert_eq!(store.text("score.home"), "3");
        assert_eq!(store.text("nothing"), "");
        // The same value again changes nothing.
        let again = store
            .set(values(&[("title", json!("Hello"))]), None)
            .unwrap();
        assert_eq!(again.revision, 1);
        assert!(again.keys.is_empty());
        // Keeping it is a change, and stays until said otherwise.
        let kept = store
            .set(values(&[("title", json!("Hello"))]), Some(true))
            .unwrap();
        assert!(kept.persisted);
        store.set(values(&[("title", json!("Hi"))]), None).unwrap();
        assert_eq!(store.persisted(), values(&[("title", json!("Hi"))]));
        // Reads by key and by prefix.
        let read = store
            .read(&DataGetParams {
                keys: vec!["title".into(), "gone".into()],
                prefix: None,
            })
            .unwrap();
        assert_eq!(read.values.len(), 1);
        assert_eq!(read.persisted, ["title"]);
        assert_eq!(read.missing, ["gone"]);
        store
            .set(values(&[("scoreboard", json!(1))]), None)
            .unwrap();
        let score = store
            .read(&DataGetParams {
                keys: vec![],
                prefix: Some("score".into()),
            })
            .unwrap();
        assert_eq!(score.values.keys().collect::<Vec<_>>(), ["score.home"]);
        let deleted = store.delete(&["title".into(), "gone".into()]).unwrap();
        assert_eq!(deleted.keys.len(), 1);
        assert!(deleted.persisted);
        assert_eq!(store.status().persisted, 0);
        assert_eq!(
            *heard.lock(),
            [
                "score.home",
                "title",
                "title",
                "title",
                "scoreboard",
                "title"
            ]
        );
    }

    #[test]
    fn bad_keys_and_big_values_are_refused_whole() {
        let store = DataStore::new();
        let err = store
            .set(
                values(&[("ok", json!(1)), ("Not.OK", json!(2))]),
                Some(true),
            )
            .unwrap_err();
        assert_eq!(err.path, r#"values["Not.OK"]"#);
        assert!(err.message.contains("lowercase"), "{err}");
        // Nothing was set.
        assert_eq!(store.revision(), 0);
        let big = "x".repeat(MAX_VALUE_BYTES);
        let err = store.set(values(&[("big", json!(big))]), None).unwrap_err();
        assert!(err.message.contains("65536"), "{err}");
        // Many values within their own limit, over the store's.
        let chunk = json!("y".repeat(MAX_VALUE_BYTES - 100));
        let mut total = 0;
        let err = loop {
            match store.set(values(&[(&format!("k{total}"), chunk.clone())]), None) {
                Ok(_) => total += 1,
                Err(err) => break err,
            }
        };
        assert!(err.message.contains("16 MB"), "{err}");
        assert!(store.status().bytes <= MAX_TOTAL_BYTES as u64);
        assert!(total > 200);
    }

    #[test]
    fn loading_a_collections_keys_replaces_the_kept_ones() {
        let store = DataStore::new();
        store
            .set(values(&[("a", json!(1)), ("b", json!(2))]), Some(true))
            .unwrap();
        store.set(values(&[("c", json!(3))]), None).unwrap();
        let (changed, skipped) =
            store.replace_persisted(values(&[("b", json!(2)), ("d", json!(4))]));
        assert!(skipped.is_empty());
        let mut keys: Vec<String> = changed.keys.iter().map(|k| k.to_string()).collect();
        keys.sort();
        assert_eq!(keys, ["a", "d"]);
        assert_eq!(
            store.persisted(),
            values(&[("b", json!(2)), ("d", json!(4))])
        );
        assert_eq!(store.text("c"), "3");
    }
}
