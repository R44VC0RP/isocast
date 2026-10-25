//! The live setup on disk: the current scene collection and profile
//! (`isocast_protocol::library`), kept in a folder named after the setup
//! file (`api::setup_path`: `setup.toml` → `setup/`):
//!
//! ```text
//! setup/current.toml            which collection and profile are in use
//! setup/collections/<id>.toml   scenes, sources, transitions, canvas size
//! setup/profiles/<id>.toml      frame rate, recording, stream, audio
//! setup/backups/                setup.toml as taken in from an older Isocast
//! setup/deleted/                deleted collections and profiles
//! ```
//!
//! They are loaded when the daemon starts and saved after every applied
//! change (only a part whose text changed is written). Saving is atomic:
//! the new file goes to a temporary file that is synced and renamed over
//! the old one, and the previous version is kept as `<file>.bak`. A file
//! that can't be loaded is never overwritten: the daemon starts without a
//! setup and reports why, and the next save first moves it aside.
//!
//! Before the library, Isocast kept the whole setup in `setup.toml`. The
//! first start with the library takes it in as the "default" collection and
//! profile (with a copy in `backups/`) and leaves it as it was, so an older
//! Isocast still finds it and never touches the library. If `setup.toml`
//! changes afterwards (an older Isocast saved it, or someone edited it), the
//! next start takes it in again, as a new collection and profile that
//! become current; nothing in the library is overwritten.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use isocast_protocol::Setup;
use isocast_protocol::api::{Current, SetupFile, SetupLoadError};
use isocast_protocol::library::{self, Collection, LibraryEntry, LibraryKind, Profile};
use serde::{Deserialize, Serialize};

/// Written at the top of a scene collection's file.
const COLLECTION_HEADER: &str = "\
# An Isocast scene collection: scenes, sources, transitions and the canvas
# size they are laid out on (`isocast collection list`). While it is current,
# isocastd saves it here after every change; edits made here by hand are read
# when isocastd next starts or switches to it. The previous version is kept as
# <this file>.bak.

";

/// Written at the top of a profile's file.
const PROFILE_HEADER: &str = "\
# An Isocast profile: frame rate, recording, stream and audio settings
# (`isocast profile list`). While it is current, isocastd saves it here after
# every change; edits made here by hand are read when isocastd next starts or
# switches to it. The previous version is kept as <this file>.bak.

";

const CURRENT_HEADER: &str = "\
# Which scene collection and profile isocastd uses (`isocast collection
# switch`, `isocast profile switch`). `legacy` remembers setup.toml as last
# taken in, so a change to it (by an older Isocast or by hand) is noticed.

";

/// `current.toml`: the collection and profile in use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurrentFile {
    pub version: u32,
    pub collection: String,
    pub profile: String,
    /// setup.toml's hash when it was last taken in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy: Option<String>,
}

/// Last on-air/Studio preview selection when a collection was left.
/// Kept outside the scene document so older engines can still load it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CollectionView {
    pub scene: Option<String>,
    pub preview: Option<String>,
}

impl CollectionView {
    pub fn restore(self, setup: &Setup) -> Self {
        let exists = |id: &String| setup.scenes.iter().any(|s| &s.id == id);
        let scene = self
            .scene
            .filter(exists)
            .or_else(|| setup.scenes.first().map(|s| s.id.clone()));
        // Retain Studio Mode if its old preview was removed.
        let preview = self
            .preview
            .and_then(|id| if exists(&id) { Some(id) } else { scene.clone() });
        Self { scene, preview }
    }
}

pub fn view_path(collection: &Path) -> PathBuf {
    collection.with_extension("view.json")
}

pub fn read_view(collection: &Path) -> Result<CollectionView> {
    let path = view_path(collection);
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("couldn't read {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(CollectionView::default()),
        Err(err) => Err(err).with_context(|| format!("couldn't read {}", path.display())),
    }
}

pub fn write_view(collection: &Path, view: &CollectionView) -> Result<()> {
    write_document(&view_path(collection), view, "").map(|_| ())
}

/// Where a change to the live setup is saved: the files of the pair that
/// was current when it was applied.
#[derive(Debug, Clone)]
pub struct Target {
    pub collection: PathBuf,
    pub profile: PathBuf,
    /// The profile's name, if its file has none to keep.
    profile_name: String,
    /// The library's folder, and the `current.toml` naming this pair,
    /// written once the files exist if there is none yet (a new library,
    /// or a save that failed to make one).
    current: (PathBuf, CurrentFile),
}

/// A file that couldn't be loaded at start.
#[derive(Debug, Clone)]
struct LoadFailure {
    /// The file it is about.
    path: PathBuf,
    error: SetupLoadError,
    /// The file is unreadable as it is, so the next save moves it aside
    /// rather than overwrite it.
    move_aside: bool,
}

pub struct Store {
    /// `setup.toml`: the whole setup, as Isocast kept it before the
    /// library.
    legacy: PathBuf,
    /// The library's folder.
    root: PathBuf,
    /// The pair in use, once there is one.
    current: Option<CurrentFile>,
    /// The names of the pair in use, for `get` and `status`.
    names: (String, String),
    load_error: Option<LoadFailure>,
    save_error: Option<String>,
    /// What happened at start (setup.toml taken in), for the log and the
    /// first watchers.
    notes: Vec<String>,
}

impl Default for Store {
    fn default() -> Self {
        Self::new(isocast_protocol::api::setup_path())
    }
}

impl Store {
    /// The library beside the setup file `legacy`.
    pub fn new(legacy: PathBuf) -> Self {
        let root = library_folder(&legacy);
        Self {
            legacy,
            root,
            current: None,
            names: Default::default(),
            load_error: None,
            save_error: None,
            notes: Vec::new(),
        }
    }

    /// The library's folder.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The setup file of an older Isocast (`setup.toml`), which tune
    /// results are still kept beside.
    pub fn legacy(&self) -> &Path {
        &self.legacy
    }

    /// The file of collection or profile `id`.
    pub fn path(&self, kind: LibraryKind, id: &str) -> PathBuf {
        document_path(&self.root, kind, id)
    }

    /// The ids in use, if any.
    pub fn current(&self) -> Option<&CurrentFile> {
        self.current.as_ref()
    }

    /// The pair in use, with names, for `get` and `status`. The
    /// collection's name is the live setup's, if there is one.
    pub fn current_names(&self, live: Option<&Setup>) -> (Option<Current>, Option<Current>) {
        match &self.current {
            None => (None, None),
            Some(current) => (
                Some(Current {
                    id: current.collection.clone(),
                    name: live.map_or_else(|| self.names.0.clone(), |setup| setup.name.clone()),
                }),
                Some(Current {
                    id: current.profile.clone(),
                    name: self.names.1.clone(),
                }),
            ),
        }
    }

    /// Where the setup lives and whether it loaded, for `get` and `status`.
    pub fn info(&self) -> SetupFile {
        let (mut path, profile_path) = match &self.current {
            Some(current) => (
                self.path(LibraryKind::Collection, &current.collection),
                Some(self.path(LibraryKind::Profile, &current.profile)),
            ),
            None => (self.legacy.clone(), None),
        };
        // The file that couldn't be loaded, so warnings name it.
        if let Some(failure) = &self.load_error {
            path = failure.path.clone();
        }
        SetupFile {
            path,
            profile_path,
            load_error: self.load_error.as_ref().map(|f| f.error.clone()),
            save_error: self.save_error.clone(),
            notes: self.notes.clone(),
        }
    }

    /// Why the setup couldn't be loaded, with the file's path, if it
    /// couldn't.
    pub fn load_warning(&self) -> Option<String> {
        self.load_error
            .as_ref()
            .map(|failure| failure.error.describe(&failure.path))
    }

    /// What happened at start, like setup.toml being taken in.
    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    /// Load the current pair as the live setup: none if there is no setup
    /// yet or it can't be loaded (see [`Store::load_warning`]). Takes in
    /// setup.toml when there is no library yet, or when it changed since it
    /// was last taken in.
    pub fn load(&mut self) -> Option<Setup> {
        let current_path = self.root.join("current.toml");
        let current: CurrentFile = match fs::read_to_string(&current_path) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return self.migrate(),
            Err(err) => {
                self.fail(&current_path, format!("it can't be read: {err}"), false);
                return None;
            }
            Ok(text) => match library::parse::<CurrentFile>(&text, false) {
                Ok(current) => current,
                Err(err) => {
                    self.fail(&current_path, format!("it is not valid: {err}"), true);
                    return None;
                }
            },
        };
        let loaded = self.load_pair(&current);
        self.current = Some(current);
        // setup.toml changed since it was last taken in: it holds newer
        // work (from an older Isocast, or a hand edit).
        if let Some(setup) = self.take_in_legacy() {
            return Some(setup);
        }
        loaded
    }

    /// Read and merge a pair, recording why if it can't be.
    fn load_pair(&mut self, current: &CurrentFile) -> Option<Setup> {
        let collection_path = self.path(LibraryKind::Collection, &current.collection);
        let profile_path = self.path(LibraryKind::Profile, &current.profile);
        let collection = match read_document::<Collection>(&collection_path) {
            Ok(collection) => collection,
            Err((message, unreadable)) => {
                self.fail(&collection_path, message, unreadable);
                return None;
            }
        };
        let profile = match read_profile(&profile_path) {
            Ok(profile) => profile,
            Err((message, unreadable)) => {
                self.fail(&profile_path, message, unreadable);
                return None;
            }
        };
        self.names = (collection.name.clone(), profile.name.clone());
        let (_, hotkey_issues) = profile.hotkeys.usable();
        if !hotkey_issues.is_empty() {
            self.notes.push(format!(
                "some hotkeys are disabled; recording settings still loaded: {}",
                isocast_protocol::hotkeys::describe(&hotkey_issues)
            ));
        }
        let setup = Setup::from_parts(collection, profile);
        let issues = setup.validate();
        if !issues.is_empty() {
            // Each file reads, but together they don't make a valid setup;
            // nothing is moved aside, and the next change replaces both.
            self.load_error = Some(LoadFailure {
                path: collection_path,
                error: SetupLoadError {
                    message: format!(
                        "with profile {:?} ({}) it has {} problem(s)",
                        current.profile,
                        profile_path.display(),
                        issues.len()
                    ),
                    issues,
                },
                move_aside: false,
            });
            return None;
        }
        Some(setup)
    }

    fn fail(&mut self, path: &Path, message: String, move_aside: bool) {
        self.load_error = Some(LoadFailure {
            path: path.to_path_buf(),
            error: SetupLoadError {
                message,
                issues: Vec::new(),
            },
            move_aside,
        });
    }

    /// No library yet: take in setup.toml as the "default" pair, if there
    /// is one.
    fn migrate(&mut self) -> Option<Setup> {
        let text = match fs::read(&self.legacy) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
            Err(err) => {
                let legacy = self.legacy.clone();
                self.fail(&legacy, format!("it can't be read: {err}"), false);
                return None;
            }
        };
        let setup = match self.read_legacy(&text) {
            Ok(setup) => setup,
            Err(error) => {
                self.load_error = Some(LoadFailure {
                    path: self.legacy.clone(),
                    error,
                    move_aside: true,
                });
                return None;
            }
        };
        let ids = (
            self.free_id(LibraryKind::Collection, "default"),
            self.free_id(LibraryKind::Profile, "default"),
        );
        let result = self.take_in(&setup, &text, ids, "Default");
        match result {
            Ok(current) => {
                self.notes.push(format!(
                    "took in {} as scene collection {:?} ({}) and profile \"Default\" ({}), now kept \
                     in {}; setup.toml is left as it was, for older versions",
                    self.legacy.display(),
                    setup.name,
                    current.collection,
                    current.profile,
                    self.root.display()
                ));
                self.names = (setup.name.clone(), "Default".into());
                self.current = Some(current);
            }
            // The setup still loads from setup.toml; the next change tries
            // to make the library again.
            Err(err) => self.notes.push(format!(
                "loaded {}, but couldn't make the library in {} ({err:#}); the next change tries \
                 again",
                self.legacy.display(),
                self.root.display()
            )),
        }
        Some(setup)
    }

    /// setup.toml changed since it was last taken in: take it in as a new
    /// pair and make that current. Returns the setup if it did.
    fn take_in_legacy(&mut self) -> Option<Setup> {
        let text = fs::read(&self.legacy).ok()?;
        let hash = hash(&text);
        let current = self.current.clone()?;
        if current.legacy.as_deref() == Some(hash.as_str()) {
            return None;
        }
        let remembered = CurrentFile {
            legacy: Some(hash),
            ..current.clone()
        };
        match self.read_legacy(&text) {
            Err(error) => {
                // Said once: remembered as seen, so the next start is quiet.
                self.notes.push(format!(
                    "{} changed since it was taken in, but it can't be taken in again: {}",
                    self.legacy.display(),
                    error.describe(&self.legacy)
                ));
                if let Err(err) = write_current(&self.root, &remembered) {
                    self.notes
                        .push(format!("couldn't update {}: {err:#}", self.root.display()));
                }
                self.current = Some(remembered);
                None
            }
            Ok(setup) => {
                let name = if setup.name.trim().is_empty() {
                    "setup.toml".to_string()
                } else {
                    setup.name.clone()
                };
                let base = library::slug(&name, "setup");
                let ids = (
                    self.free_id(LibraryKind::Collection, &base),
                    self.free_id(LibraryKind::Profile, &base),
                );
                match self.take_in(&setup, &text, ids, &name) {
                    Err(err) => {
                        self.notes.push(format!(
                            "{} changed since it was taken in, but couldn't be taken in again \
                             ({err:#}); the library is unchanged",
                            self.legacy.display()
                        ));
                        None
                    }
                    Ok(taken) => {
                        self.notes.push(format!(
                            "{} changed since it was taken in (an older Isocast saved it, or it \
                             was edited), so it is now scene collection {:?} ({}) and profile \
                             {name:?} ({}), in use; the ones in use before are kept (collection \
                             {}, profile {})",
                            self.legacy.display(),
                            setup.name,
                            taken.collection,
                            taken.profile,
                            current.collection,
                            current.profile
                        ));
                        self.names = (setup.name.clone(), name);
                        self.load_error = None;
                        self.current = Some(taken);
                        Some(setup)
                    }
                }
            }
        }
    }

    /// setup.toml's text as a valid setup, or why it isn't one.
    fn read_legacy(&mut self, text: &[u8]) -> Result<Setup, SetupLoadError> {
        let failed = |message: String, issues| SetupLoadError { message, issues };
        let text = String::from_utf8_lossy(text);
        let parsed = if is_json(&self.legacy) {
            Setup::from_json(&text)
        } else {
            Setup::from_toml(&text)
        };
        let setup =
            parsed.map_err(|err| failed(format!("it is not a valid setup: {err}"), Vec::new()))?;
        let issues = setup.validate();
        if !issues.is_empty() {
            return Err(failed(
                format!("it has {} problem(s)", issues.len()),
                issues,
            ));
        }
        Ok(setup)
    }

    /// Write `setup` as collection `ids.0` and profile `ids.1` (named
    /// `profile_name`), with a copy of the setup file's `text` in
    /// `backups/`, and make them current.
    fn take_in(
        &self,
        setup: &Setup,
        text: &[u8],
        ids: (String, String),
        profile_name: &str,
    ) -> Result<CurrentFile> {
        let backups = self.root.join("backups");
        fs::create_dir_all(&backups)
            .with_context(|| format!("could not create {}", backups.display()))?;
        let name = self
            .legacy
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let backup = unused(&backups.join(format!("{} {name}", stamp())));
        write_atomically(&backup, text)?;
        let (collection, profile) = setup.clone().into_parts(profile_name);
        write_document(
            &self.path(LibraryKind::Collection, &ids.0),
            &collection,
            COLLECTION_HEADER,
        )?;
        write_profile(&self.path(LibraryKind::Profile, &ids.1), &profile)?;
        let current = CurrentFile {
            version: 1,
            collection: ids.0,
            profile: ids.1,
            legacy: Some(hash(text)),
        };
        write_current(&self.root, &current)?;
        Ok(current)
    }

    /// `base`, or `base-2`, `base-3`… whichever has no file yet.
    pub fn free_id(&self, kind: LibraryKind, base: &str) -> String {
        let base = library::slug(base, kind.command());
        let taken = |id: &str| self.path(kind, id).exists();
        if !taken(&base) {
            return base;
        }
        (2..)
            .map(|n| format!("{base}-{n}"))
            .find(|id| !taken(id))
            .unwrap()
    }

    /// Get ready to save a change to the live setup: where it goes (making
    /// the "default" pair if there is none yet), after moving aside a file
    /// that couldn't be loaded at start. Returns notes, and the target if
    /// saving can go ahead.
    pub fn prepare(&mut self) -> (Vec<String>, Option<Target>) {
        let mut notes = Vec::new();
        if let Some(failure) = self.load_error.clone() {
            if failure.move_aside && failure.path.exists() {
                match move_aside(&failure.path) {
                    Ok(kept) => notes.push(format!(
                        "{} couldn't be loaded at start, so it was kept as {}",
                        failure.path.display(),
                        kept.display()
                    )),
                    Err(err) => {
                        let message = format!(
                            "the change is live but wasn't saved: {} couldn't be loaded at start \
                             and couldn't be moved aside ({err:#}), so it was left as it was",
                            failure.path.display()
                        );
                        self.save_error = Some(message.clone());
                        notes.push(message);
                        return (notes, None);
                    }
                }
                // A broken current.toml: start a new pair rather than guess
                // which files it meant.
                if failure.path == self.root.join("current.toml") {
                    self.current = None;
                }
            }
            self.load_error = None;
        }
        let current = match &self.current {
            Some(current) => current.clone(),
            None => {
                let current = CurrentFile {
                    version: 1,
                    collection: self.free_id(LibraryKind::Collection, "default"),
                    profile: self.free_id(LibraryKind::Profile, "default"),
                    // setup.toml as it is now was the live setup (or
                    // couldn't load): not newer work to take in later.
                    legacy: fs::read(&self.legacy).ok().map(|text| hash(&text)),
                };
                self.names.1 = "Default".into();
                self.current = Some(current.clone());
                current
            }
        };
        let target = Target {
            collection: self.path(LibraryKind::Collection, &current.collection),
            profile: self.path(LibraryKind::Profile, &current.profile),
            profile_name: self.names.1.clone(),
            current: (self.root.clone(), current),
        };
        (notes, Some(target))
    }

    /// Take the outcome of a [`write`]; returns notes about a failure.
    pub fn saved(&mut self, result: Result<()>) -> Vec<String> {
        let mut notes = Vec::new();
        match result {
            Ok(()) => self.save_error = None,
            Err(err) => {
                let message = format!(
                    "the change is live but couldn't be saved to {} ({err:#}); restarting \
                     isocastd would lose it",
                    self.root.display()
                );
                self.save_error = Some(message.clone());
                notes.push(message);
            }
        }
        notes
    }

    /// Make collection `ids.0` and profile `ids.1` the pair in use (their
    /// files exist), named `names`. Returns `current.toml` to write
    /// ([`write_current`]).
    pub fn set_current(&mut self, ids: (&str, &str), names: (&str, &str)) -> CurrentFile {
        let legacy = self
            .current
            .as_ref()
            .and_then(|current| current.legacy.clone())
            .or_else(|| fs::read(&self.legacy).ok().map(|text| hash(&text)));
        let current = CurrentFile {
            version: 1,
            collection: ids.0.to_string(),
            profile: ids.1.to_string(),
            legacy,
        };
        self.current = Some(current.clone());
        self.names = (names.0.to_string(), names.1.to_string());
        self.load_error = None;
        current
    }

    /// The profile in use was renamed.
    pub fn renamed_profile(&mut self, name: &str) {
        self.names.1 = name.to_string();
    }
}

/// Every collection or profile in the library at `root`, sorted by name;
/// `current` is the id of the one in use.
pub fn list(root: &Path, kind: LibraryKind, current: Option<&str>) -> Vec<LibraryEntry> {
    let mut entries: Vec<LibraryEntry> = fs::read_dir(root.join(kind.folder()))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "toml"))
        .filter_map(|path| {
            let id = path.file_stem()?.to_str()?.to_string();
            library::valid_id(&id).then(|| entry(root, kind, &id, current == Some(id.as_str())))
        })
        .collect();
    entries.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.id.cmp(&b.id))
    });
    entries
}

/// One collection or profile, read from its file.
pub fn entry(root: &Path, kind: LibraryKind, id: &str, current: bool) -> LibraryEntry {
    let path = document_path(root, kind, id);
    let read = match kind {
        LibraryKind::Collection => {
            read_document::<Collection>(&path).map(|c| (c.name.clone(), c.summary()))
        }
        LibraryKind::Profile => read_profile(&path).map(|p| (p.name.clone(), p.summary())),
    };
    let (name, summary, problem) = match read {
        Ok((name, summary)) => (name, Some(summary), None),
        Err((message, _)) => (String::new(), None, Some(message)),
    };
    LibraryEntry {
        kind,
        id: id.to_string(),
        name,
        current,
        path,
        summary,
        problem,
    }
}

/// The library's folder for setup file `legacy`: beside it, named after it
/// (`…/Isocast/setup.toml` → `…/Isocast/setup/`), so engines with setup
/// files of their own (`$ISOCAST_SETUP`, `$ISOCAST_SOCKET`) have libraries
/// of their own.
pub fn library_folder(legacy: &Path) -> PathBuf {
    match legacy.extension() {
        Some(_) => legacy.with_extension(""),
        None => sibling(legacy, ".library"),
    }
}

pub fn document_path(root: &Path, kind: LibraryKind, id: &str) -> PathBuf {
    root.join(kind.folder()).join(format!("{id}.toml"))
}

/// Read a collection or profile: the document, or why not and whether the
/// file is unreadable as it is (rather than missing or unreadable now).
pub fn read_document<T: serde::de::DeserializeOwned + Versioned>(
    path: &Path,
) -> Result<T, (String, bool)> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err((format!("{} is missing", path.display()), false));
        }
        Err(err) => return Err((format!("{} can't be read: {err}", path.display()), false)),
    };
    let document: T = library::parse(&text, is_json(path))
        .map_err(|err| (format!("{} is not valid: {err}", path.display()), true))?;
    if document.version() != isocast_protocol::SETUP_VERSION {
        return Err((
            format!(
                "{}: unsupported version {}; expected {}",
                path.display(),
                document.version(),
                isocast_protocol::SETUP_VERSION
            ),
            true,
        ));
    }
    Ok(document)
}

/// A document with a format version.
pub trait Versioned {
    fn version(&self) -> u32;
}

impl Versioned for Collection {
    fn version(&self) -> u32 {
        self.version
    }
}

impl Versioned for Profile {
    fn version(&self) -> u32 {
        self.version
    }
}

/// Save a change to the live setup into the pair `target` names: each part
/// only if its text changed. The profile keeps the name its file has (a
/// rename may have come after the change).
pub fn write(target: &Target, setup: &Setup, parts: Parts) -> Result<()> {
    let existing = read_profile(&target.profile).ok();
    let profile_name = existing
        .as_ref()
        .map(|profile| profile.name.clone())
        .unwrap_or_else(|| target.profile_name.clone());
    let (collection, mut profile) = setup.clone().into_parts(&profile_name);
    // The scene collection decides whether there is a vertical canvas; the
    // profile's vertical frame rate, recording and stream are kept while
    // the live one has none (a collection without one, or one turned off),
    // so switching back finds them again.
    if let Some(existing) = existing {
        if setup.vertical.is_none() {
            profile.vertical = existing.vertical;
        }
        // The same for `[[canvases]]` (#84): a saved canvas's outputs stay
        // in the profile while the live collection doesn't have it.
        for saved in existing.canvases {
            if saved.id.is_some() && !profile.canvases.iter().any(|p| p.id == saved.id) {
                let live = setup.canvases.iter().any(|c| c.id == saved.id);
                if !live {
                    profile.canvases.push(saved);
                }
            }
        }
    }
    if parts.collection {
        write_document(&target.collection, &collection, COLLECTION_HEADER)?;
    }
    if parts.profile {
        write_profile(&target.profile, &profile)?;
    }
    let (root, current) = &target.current;
    if !root.join("current.toml").exists() {
        write_current(root, current)?;
    }
    Ok(())
}

/// Which parts of a pair to write (one may have a newer save already).
#[derive(Debug, Clone, Copy)]
pub struct Parts {
    pub collection: bool,
    pub profile: bool,
}

/// Kept beside the profile, outside its v1 document. Old binaries ignore this
/// extension rather than parking the entire profile for an unknown field.
pub fn hotkeys_path(profile: &Path) -> PathBuf {
    profile.with_extension("hotkeys.json")
}

pub fn read_profile(path: &Path) -> Result<Profile, (String, bool)> {
    let mut profile = read_document::<Profile>(path)?;
    // Inline metadata takes precedence until an explicit write migrates it.
    // Listing, exporting, switching or loading must not rewrite this file or
    // rotate its backup just to inspect it.
    if profile.hotkeys.is_absent() {
        let sidecar = hotkeys_path(path);
        match fs::read(&sidecar) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(value) => profile.hotkeys.value = Some(value),
                Err(error) => {
                    profile.hotkeys.load_warning = Some(format!(
                        "{}: invalid JSON ({error}); bindings disabled; file preserved",
                        sidecar.display()
                    ))
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                profile.hotkeys.load_warning = Some(format!(
                    "{}: {error}; bindings disabled; file preserved",
                    sidecar.display()
                ))
            }
        }
    }
    Ok(profile)
}

pub fn write_profile(path: &Path, profile: &Profile) -> Result<bool> {
    if !path.try_exists()? {
        archive_orphan_hotkeys(path)?;
    }
    let mut disk = profile.clone();
    let config = std::mem::take(&mut disk.hotkeys);
    // Unreadable sidecars have no parsed value and survive unrelated saves.
    // Failed inline migrations DO retain a value: retry it and require a
    // durable sidecar before stripping the field, even on a profile rename.
    if let Some(value) = config.value {
        let sidecar = hotkeys_path(path);
        if !value.as_object().is_some_and(|v| v.is_empty()) || sidecar.exists() {
            let text = format!("{}\n", serde_json::to_string_pretty(&value)?);
            if !fs::read_to_string(&sidecar).is_ok_and(|old| old == text) {
                write_atomically(&sidecar, text.as_bytes())?;
            }
        }
    }
    write_document(path, &disk, PROFILE_HEADER)
}

/// Old daemons move only the TOML when deleting a profile. A fresh profile
/// (blank, imported or duplicated) must never adopt its predecessor's sidecar.
/// Preserve orphaned data outside the live namespace instead of deleting it.
fn archive_orphan_hotkeys(profile: &Path) -> Result<()> {
    let sidecar = hotkeys_path(profile);
    for source in [&sidecar, &sibling(&sidecar, ".bak")] {
        if !source.try_exists()? {
            continue;
        }
        let root = profile
            .parent()
            .and_then(Path::parent)
            .context("profile has no library folder")?;
        let deleted = root.join("deleted/profiles");
        fs::create_dir_all(&deleted)?;
        let name = source
            .file_name()
            .context("sidecar has no file name")?
            .to_string_lossy();
        let kept = unused(&deleted.join(format!("orphan {} {name}", stamp())));
        fs::rename(source, &kept)
            .with_context(|| format!("could not archive orphaned hotkeys {}", source.display()))?;
    }
    Ok(())
}

/// Write a collection or profile to `path` atomically, keeping the previous
/// file as `<path>.bak`, unless the file already says the same. Returns
/// whether it wrote.
pub fn write_document<T>(path: &Path, document: &T, header: &str) -> Result<bool>
where
    T: Serialize + serde::de::DeserializeOwned + PartialEq,
{
    let text = if is_json(path) {
        let mut text = serde_json::to_string_pretty(document)?;
        text.push('\n');
        text
    } else {
        format!("{header}{}", library::to_toml(document)?)
    };
    if fs::read_to_string(path).is_ok_and(|old| old == text) {
        return Ok(false);
    }
    // Never save something that would load as something else.
    if library::parse::<T>(&text, is_json(path)).ok().as_ref() != Some(document) {
        bail!("it doesn't read back as the same document (a bug in Isocast; please report it)");
    }
    write_atomically(path, text.as_bytes())?;
    Ok(true)
}

/// The header for a kind's files.
pub fn header(kind: LibraryKind) -> &'static str {
    match kind {
        LibraryKind::Collection => COLLECTION_HEADER,
        LibraryKind::Profile => PROFILE_HEADER,
    }
}

/// Write `current.toml` into the library at `root`.
pub fn write_current(root: &Path, current: &CurrentFile) -> Result<()> {
    let text = format!("{CURRENT_HEADER}{}", library::to_toml(current)?);
    write_atomically(&root.join("current.toml"), text.as_bytes())
}

/// Write `text` to `path` atomically, keeping the previous file as
/// `<path>.bak`.
pub(crate) fn write_atomically(path: &Path, text: &[u8]) -> Result<()> {
    let folder = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(folder) = folder {
        fs::create_dir_all(folder)
            .with_context(|| format!("could not create {}", folder.display()))?;
    }
    let temporary = sibling(path, ".partial");
    let mut file = File::create(&temporary)
        .with_context(|| format!("could not create {}", temporary.display()))?;
    file.write_all(text)?;
    file.sync_all()?;
    drop(file);
    if path.exists() {
        // Copied, so `path` is never missing, even for a moment.
        let backup = sibling(path, ".bak");
        let partial = sibling(&backup, ".partial");
        fs::copy(path, &partial)
            .with_context(|| format!("could not back up {}", path.display()))?;
        isocast_protocol::fs::replace(&partial, &backup)?;
    }
    isocast_protocol::fs::replace(&temporary, path)
        .with_context(|| format!("could not replace {}", path.display()))?;
    // Make the rename itself durable.
    if let Some(folder) = folder
        && let Ok(folder) = File::open(folder)
    {
        let _ = folder.sync_all();
    }
    Ok(())
}

/// Move a collection or profile's file into the library's `deleted`
/// folder (with its .bak, and a collection's kept data keys), never
/// replacing anything. Returns where it went.
pub fn delete(root: &Path, kind: LibraryKind, id: &str) -> Result<PathBuf> {
    let path = document_path(root, kind, id);
    let deleted = root.join("deleted").join(kind.folder());
    fs::create_dir_all(&deleted)
        .with_context(|| format!("could not create {}", deleted.display()))?;
    let kept = unused(&deleted.join(format!("{id} {}.toml", stamp())));
    fs::rename(&path, &kept).with_context(|| format!("could not move {}", path.display()))?;
    let backup = sibling(&path, ".bak");
    if backup.exists() {
        let _ = fs::rename(&backup, sibling(&kept, ".bak"));
    }
    let data = crate::data::data_path(&path);
    if data.exists() {
        let _ = fs::rename(&data, crate::data::data_path(&kept));
        let _ = fs::rename(
            sibling(&data, ".bak"),
            sibling(&crate::data::data_path(&kept), ".bak"),
        );
    }
    if kind == LibraryKind::Collection {
        let view = view_path(&path);
        if view.exists() {
            fs::rename(&view, view_path(&kept))?;
            if sibling(&view, ".bak").exists() {
                fs::rename(sibling(&view, ".bak"), sibling(&view_path(&kept), ".bak"))?;
            }
        }
    }
    if kind == LibraryKind::Profile {
        let hotkeys = hotkeys_path(&path);
        if hotkeys.exists() {
            fs::rename(&hotkeys, hotkeys_path(&kept))?;
            if sibling(&hotkeys, ".bak").exists() {
                fs::rename(
                    sibling(&hotkeys, ".bak"),
                    sibling(&hotkeys_path(&kept), ".bak"),
                )?;
            }
        }
    }
    Ok(kept)
}

/// Move a file that couldn't be loaded to `<name>.broken.<ext>` (or
/// `.broken-2`, …), never replacing anything.
fn move_aside(path: &Path) -> Result<PathBuf> {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let extension = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let kept = unused(&path.with_file_name(format!("{stem}.broken{extension}")));
    fs::rename(path, &kept)
        .with_context(|| format!("could not rename it to {}", kept.display()))?;
    Ok(kept)
}

/// `path`, or `path` with `-2`, `-3`… before its extension, whichever
/// doesn't exist.
fn unused(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let extension = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (2..)
        .map(|n| path.with_file_name(format!("{stem}-{n}{extension}")))
        .find(|path| !path.exists())
        .unwrap()
}

fn is_json(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "json")
}

/// `path` with `suffix` added to its file name.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// A stable hash of a file's bytes (FNV-1a, 64 bits), to notice changes.
fn hash(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The local date and time for file names: `2026-10-13 09-41-05`.
#[cfg(unix)]
fn stamp() -> String {
    // SAFETY: time and localtime_r only write the values they are given.
    let tm = unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        tm
    };
    format!(
        "{:04}-{:02}-{:02} {:02}-{:02}-{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

#[cfg(windows)]
fn stamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H-%M-%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(name: &str) -> Setup {
        let mut setup = Setup::quick(&["test-pattern".into(), "tone".into()]).unwrap();
        setup.name = name.into();
        setup
    }

    fn folder(test: &str) -> PathBuf {
        let folder =
            std::env::temp_dir().join(format!("isocast-store-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).unwrap();
        folder
    }

    fn save(store: &mut Store, setup: &Setup) -> Vec<String> {
        let (mut notes, target) = store.prepare();
        if let Some(target) = target {
            let all = Parts {
                collection: true,
                profile: true,
            };
            let result = write(&target, setup, all);
            notes.extend(store.saved(result));
        }
        notes
    }

    #[test]
    fn a_new_library_saves_a_default_pair_and_loads_it_back() {
        let folder = folder("new");
        let legacy = folder.join("setup.toml");
        let mut store = Store::new(legacy.clone());
        assert_eq!(store.load(), None);
        assert!(save(&mut store, &setup("one")).is_empty());
        assert!(save(&mut store, &setup("two")).is_empty());
        let root = folder.join("setup");
        assert!(root.join("collections/default.toml").exists());
        assert!(root.join("profiles/default.toml").exists());
        assert!(!legacy.exists());
        let mut again = Store::new(legacy);
        assert_eq!(again.load(), Some(setup("two")));
        assert_eq!(again.current().unwrap().collection, "default");
        let backup = fs::read_to_string(root.join("collections/default.toml.bak")).unwrap();
        assert!(backup.contains("name = \"one\""));
        // The profile didn't change, so it was written once.
        assert!(!root.join("profiles/default.toml.bak").exists());
        let _ = fs::remove_dir_all(&folder);
    }

    #[test]
    fn setup_toml_is_taken_in_once_and_again_when_it_changes() {
        let folder = folder("migrate");
        let legacy = folder.join("setup.toml");
        let old = setup("Recording").to_toml().unwrap();
        fs::write(&legacy, &old).unwrap();
        let mut store = Store::new(legacy.clone());
        assert_eq!(store.load(), Some(setup("Recording")));
        assert!(store.notes()[0].contains("took in"), "{:?}", store.notes());
        assert_eq!(store.current_names(None).1.unwrap().name, "Default");
        // Left as it was, with a copy in backups/.
        assert_eq!(fs::read_to_string(&legacy).unwrap(), old);
        let backups: Vec<_> = fs::read_dir(folder.join("setup/backups"))
            .unwrap()
            .collect();
        assert_eq!(backups.len(), 1);
        // A change in the library; setup.toml is not taken in again.
        assert!(save(&mut store, &setup("Recording 2")).is_empty());
        let mut again = Store::new(legacy.clone());
        assert_eq!(again.load(), Some(setup("Recording 2")));
        assert!(again.notes().is_empty());
        // An older Isocast saves setup.toml: taken in as a new pair, made
        // current; the default pair is kept.
        fs::write(&legacy, setup("From old").to_toml().unwrap()).unwrap();
        let mut third = Store::new(legacy.clone());
        assert_eq!(third.load(), Some(setup("From old")));
        assert_eq!(third.current().unwrap().collection, "from-old");
        assert!(third.notes()[0].contains("changed"), "{:?}", third.notes());
        assert_eq!(list(third.root(), LibraryKind::Collection, None).len(), 2);
        assert_eq!(list(third.root(), LibraryKind::Profile, None).len(), 2);
        let mut fourth = Store::new(legacy);
        assert_eq!(fourth.load(), Some(setup("From old")));
        assert!(fourth.notes().is_empty());
        let _ = fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_file_that_fails_to_load_is_kept_and_moved_aside_on_save() {
        let folder = folder("broken");
        let legacy = folder.join("setup.toml");
        fs::write(&legacy, "version = 1\nname = ").unwrap();
        let mut store = Store::new(legacy.clone());
        assert_eq!(store.load(), None);
        assert!(store.info().load_error.is_some());
        assert!(store.load_warning().unwrap().contains("setup.toml"));
        // Still there, untouched, until a change is saved.
        assert_eq!(fs::read_to_string(&legacy).unwrap(), "version = 1\nname = ");
        let notes = save(&mut store, &setup("fixed"));
        assert!(notes[0].contains("setup.broken.toml"), "{notes:?}");
        assert_eq!(store.info().load_error, None);
        assert_eq!(Store::new(legacy.clone()).load(), Some(setup("fixed")));
        // A broken collection file is moved aside the same way.
        let collection = folder.join("setup/collections/default.toml");
        fs::write(&collection, "version = ").unwrap();
        let mut store = Store::new(legacy.clone());
        assert_eq!(store.load(), None);
        assert!(store.load_warning().unwrap().contains("default.toml"));
        let notes = save(&mut store, &setup("again"));
        assert!(notes[0].contains("default.broken.toml"), "{notes:?}");
        assert_eq!(Store::new(legacy).load(), Some(setup("again")));
        let _ = fs::remove_dir_all(&folder);
    }

    #[test]
    fn an_invalid_pair_reports_its_problems() {
        let folder = folder("invalid");
        let legacy = folder.join("setup.json");
        let mut bad = setup("bad");
        bad.canvas.fps = 0;
        fs::write(&legacy, serde_json::to_string(&bad).unwrap()).unwrap();
        let mut store = Store::new(legacy);
        assert_eq!(store.load(), None);
        let error = store.info().load_error.unwrap();
        assert_eq!(error.issues[0].path, "canvas.fps");
        let _ = fs::remove_dir_all(&folder);
    }
}
