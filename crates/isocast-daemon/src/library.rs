//! The library methods (`library_*`): scene collections and profiles, the
//! two parts the live setup is kept in (`isocast_protocol::library`, on disk
//! in `store`). The pair in use is the live setup: changing it goes through
//! `apply` like any change, and switching applies the new pair to the live
//! engine at once.

use std::path::{Path, PathBuf};

use isocast_protocol::api::{ApplyParams, Event, RpcError};
use isocast_protocol::change::Change;
use isocast_protocol::library::{
    self, Collection, Library, LibraryAction, LibraryChange, LibraryCreateParams, LibraryDocument,
    LibraryEntry, LibraryImportObsParams, LibraryImportParams, LibraryKind, LibraryRenameParams,
    LibrarySwitchParams, LibraryTarget, Profile,
};
use isocast_protocol::obs;
use isocast_protocol::{Issue, SETUP_VERSION, Setup, Transition};

use crate::store::{self, CurrentFile};
use crate::{Daemon, State, content_size, devices, now, relative_folder};

/// A scene collection or profile in hand.
#[derive(Debug, Clone)]
enum Document {
    Collection(Collection),
    Profile(Profile),
}

impl Document {
    fn kind(&self) -> LibraryKind {
        match self {
            Self::Collection(_) => LibraryKind::Collection,
            Self::Profile(_) => LibraryKind::Profile,
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Collection(collection) => &collection.name,
            Self::Profile(profile) => &profile.name,
        }
    }

    fn set_name(&mut self, name: &str) {
        match self {
            Self::Collection(collection) => collection.name = name.to_string(),
            Self::Profile(profile) => profile.name = name.to_string(),
        }
    }

    /// Write it to `path` (atomically, keeping a .bak).
    fn write(&self, path: &Path) -> anyhow::Result<()> {
        let header = store::header(self.kind());
        match self {
            Self::Collection(collection) => store::write_document(path, collection, header),
            Self::Profile(profile) => store::write_profile(path, profile),
        }
        .map(|_| ())
    }

    /// Problems it has of its own, whatever it is paired with: checked
    /// with a neutral other half (a starter profile, or an empty collection
    /// on the largest canvas), keeping the problems in its own fields.
    fn problems(&self) -> Vec<Issue> {
        let (setup, version) = match self {
            Self::Collection(collection) => (
                Setup::from_parts(collection.clone(), Profile::starter("")),
                collection.version,
            ),
            Self::Profile(profile) => (
                Setup::from_parts(Collection::blank("", 8192, 8192), profile.clone()),
                profile.version,
            ),
        };
        let mut problems: Vec<Issue> = setup
            .validate()
            .into_iter()
            .filter(|issue| library::part_of(&issue.path) == Some(self.kind()))
            .collect();
        if version != SETUP_VERSION {
            problems.insert(
                0,
                Issue {
                    path: "version".into(),
                    message: format!("unsupported version {version}; expected {SETUP_VERSION}"),
                },
            );
        }
        if let Self::Profile(profile) = self
            && let Some(message) = relative_folder(&profile.recording.folder)
        {
            problems.push(Issue {
                path: "recording.folder".into(),
                message,
            });
        }
        problems
    }
}

fn invalid(message: impl Into<String>) -> RpcError {
    RpcError {
        code: RpcError::INVALID_PARAMS,
        message: message.into(),
    }
}

fn wrong_state(message: impl Into<String>) -> RpcError {
    RpcError {
        code: RpcError::WRONG_STATE,
        message: message.into(),
    }
}

fn failed(message: impl Into<String>) -> RpcError {
    RpcError {
        code: RpcError::FAILED,
        message: message.into(),
    }
}

/// A name for a collection or profile: not empty, one line.
fn check_name(field: &str, name: &str) -> Result<String, RpcError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(invalid(format!("{field}: a name must not be empty")));
    }
    if name.chars().count() > 120 || name.contains(['\n', '\r']) {
        return Err(invalid(format!(
            "{field}: a name must be one line of at most 120 characters"
        )));
    }
    Ok(name.to_string())
}

/// "default (Recording), gaming (Gaming)", or that there are none.
fn listing(root: &Path, kind: LibraryKind) -> String {
    let entries = store::list(root, kind, None);
    if entries.is_empty() {
        return format!(
            "there are no {}s yet (`isocast {} new NAME`, or MCP's \
             create_collection_or_profile, makes one)",
            kind.label(),
            kind.command()
        );
    }
    let ids: Vec<String> = entries
        .iter()
        .map(|entry| format!("{} ({})", entry.id, entry.name))
        .collect();
    format!("there are: {}", ids.join(", "))
}

/// Check `id` names an existing collection or profile; the error names
/// `field` and lists the ones there are.
fn check_exists(root: &Path, kind: LibraryKind, field: &str, id: &str) -> Result<(), RpcError> {
    if library::valid_id(id) && store::document_path(root, kind, id).exists() {
        return Ok(());
    }
    Err(invalid(format!(
        "{field}: no {} {id:?}; {}",
        kind.label(),
        listing(root, kind)
    )))
}

/// Why the pair in use can't be swapped now, if it can't.
fn switch_refusal(state: &State, what: &str) -> Option<String> {
    if state.camera.active() {
        return Some(format!(
            "can't switch {what} while the virtual camera runs; isocast vcam stop, then switch"
        ));
    }
    if state.session.is_some() {
        return Some(format!(
            "can't switch {what} while recording: the recording's files belong to the live \
             setup; stop it first (`isocast stop`, or MCP's stop_recording), then switch"
        ));
    }
    if state.replay.active() {
        return Some(format!(
            "can't switch {what} while the replay buffer runs: it keeps the current canvas; \
             stop it first (`isocast replay stop`, or MCP's stop_replay), then switch"
        ));
    }
    if state.streaming() || state.streams_starting > 0 {
        return Some(format!(
            "can't switch {what} while streaming: the stream would change under its viewers; \
             stop it first (`isocast stream stop`, or MCP's stop_stream), then switch"
        ));
    }
    None
}

/// One line per problem, saying which part each field is in.
fn describe(problems: &[Issue]) -> String {
    problems
        .iter()
        .map(|issue| {
            let part = library::part_of(&issue.path)
                .map_or(String::new(), |kind| format!(" (the {}'s)", kind.label()));
            format!("\n  {}{part}: {}", issue.path, issue.message)
        })
        .collect()
}

impl Daemon {
    /// The library's folder and the pair in use, read under the lock so
    /// the files can be read without it.
    fn library_view(&self) -> (PathBuf, Option<CurrentFile>) {
        let state = self.state.read();
        (
            state.file.root().to_path_buf(),
            state.file.current().cloned(),
        )
    }

    pub(crate) fn library(&self) -> Library {
        let (root, current) = self.library_view();
        let collection = current.as_ref().map(|c| c.collection.clone());
        let profile = current.as_ref().map(|c| c.profile.clone());
        Library {
            collections: store::list(&root, LibraryKind::Collection, collection.as_deref()),
            profiles: store::list(&root, LibraryKind::Profile, profile.as_deref()),
            collection,
            profile,
            folder: root,
        }
    }

    /// The entry for `id` after a change.
    fn entry(&self, kind: LibraryKind, id: &str) -> LibraryEntry {
        let (root, current) = self.library_view();
        let current = current.is_some_and(|c| match kind {
            LibraryKind::Collection => c.collection == id,
            LibraryKind::Profile => c.profile == id,
        });
        store::entry(&root, kind, id, current)
    }

    /// A change's result: `entries` and the pair in use after.
    fn change(&self, entries: Vec<LibraryEntry>) -> LibraryChange {
        let (_, current) = self.library_view();
        let mut change = LibraryChange {
            collection: None,
            profile: None,
            current_collection: current.as_ref().map(|c| c.collection.clone()),
            current_profile: current.as_ref().map(|c| c.profile.clone()),
            revision: None,
            notes: Vec::new(),
            unsupported: Vec::new(),
            problems: Vec::new(),
        };
        for entry in entries {
            match entry.kind {
                LibraryKind::Collection => change.collection = Some(entry),
                LibraryKind::Profile => change.profile = Some(entry),
            }
        }
        change
    }

    fn library_changed(&self, action: LibraryAction, kind: Option<LibraryKind>, id: Option<&str>) {
        let (_, current) = self.library_view();
        self.publish(Event::LibraryChanged {
            action,
            kind,
            id: id.map(str::to_string),
            collection: current.as_ref().map(|c| c.collection.clone()),
            profile: current.as_ref().map(|c| c.profile.clone()),
        });
    }

    /// Collection or profile `id`: the one in use from the live setup
    /// (which is newer than its file while a change waits to be saved),
    /// others from their files.
    fn document(&self, kind: LibraryKind, field: &str, id: &str) -> Result<Document, RpcError> {
        let (root, _) = self.library_view();
        check_exists(&root, kind, field, id)?;
        {
            let state = self.state.read();
            let in_use = state.file.current().is_some_and(|c| match kind {
                LibraryKind::Collection => c.collection == id,
                LibraryKind::Profile => c.profile == id,
            });
            if in_use && let Some(setup) = &state.setup {
                let profile_name = state
                    .file
                    .current_names(None)
                    .1
                    .map(|p| p.name)
                    .unwrap_or_default();
                let (collection, profile) = setup.clone().into_parts(&profile_name);
                return Ok(match kind {
                    LibraryKind::Collection => Document::Collection(collection),
                    LibraryKind::Profile => Document::Profile(profile),
                });
            }
        }
        let path = store::document_path(&root, kind, id);
        let read = match kind {
            LibraryKind::Collection => store::read_document(&path).map(Document::Collection),
            LibraryKind::Profile => store::read_profile(&path).map(Document::Profile),
        };
        read.map_err(|(message, _)| failed(format!("{field}: {message}")))
    }

    pub(crate) fn library_get(&self, target: LibraryTarget) -> Result<LibraryDocument, RpcError> {
        let document = self.document(target.kind, "id", &target.id)?;
        let entry = self.entry(target.kind, &target.id);
        Ok(match document {
            Document::Collection(collection) => LibraryDocument {
                entry,
                collection: Some(collection),
                profile: None,
            },
            Document::Profile(profile) => LibraryDocument {
                entry,
                collection: None,
                profile: Some(profile),
            },
        })
    }

    pub(crate) fn library_create(
        &self,
        params: LibraryCreateParams,
    ) -> Result<LibraryChange, RpcError> {
        let kind = params.kind;
        let name = check_name("name", &params.name)?;
        let (root, _) = self.library_view();
        let mut document = match &params.copy_of {
            Some(source) => self.document(kind, "copy_of", source)?,
            None => match kind {
                LibraryKind::Collection => {
                    // On the canvas in use, so the new scenes fit it.
                    let canvas = self
                        .state
                        .lock()
                        .setup
                        .as_ref()
                        .map(|s| (s.canvas.width, s.canvas.height));
                    let (width, height) = canvas.unwrap_or((1920, 1080));
                    Document::Collection(Collection::blank(&name, width, height))
                }
                LibraryKind::Profile => Document::Profile(Profile::starter(&name)),
            },
        };
        document.set_name(&name);
        let id = self.add(&root, &document, params.id.as_deref())?;
        // A copy of a collection has its kept data keys too.
        if let (LibraryKind::Collection, Some(source)) = (kind, &params.copy_of) {
            self.data.flush();
            let from = crate::data::data_path(&store::document_path(&root, kind, source));
            let to = crate::data::data_path(&store::document_path(&root, kind, &id));
            if from.exists()
                && let Err(err) = std::fs::copy(&from, &to)
            {
                isocast_diagnostics::diagnostic!("warn";
                    "isocastd: couldn't copy {} to {}: {err}",
                    from.display(),
                    to.display()
                );
            }
        }
        self.library_changed(LibraryAction::Created, Some(kind), Some(&id));
        let mut change = self.change(vec![self.entry(kind, &id)]);
        change.problems = self.problems_in_use(&document);
        if params.copy_of.is_none() && kind == LibraryKind::Collection {
            change.notes.push(format!(
                "made scene collection {id:?} with one empty scene; switch to it (`isocast \
                 collection switch {id}`) and add sources"
            ));
        }
        if params.switch {
            let switch = match kind {
                LibraryKind::Collection => LibrarySwitchParams {
                    collection: Some(id.clone()),
                    profile: None,
                },
                LibraryKind::Profile => LibrarySwitchParams {
                    collection: None,
                    profile: Some(id.clone()),
                },
            };
            let switched = self.library_switch(switch).map_err(|err| RpcError {
                message: format!(
                    "made {} {id:?}, but didn't switch to it: {}",
                    kind.label(),
                    err.message
                ),
                ..err
            })?;
            change.revision = switched.revision;
            change.current_collection = switched.current_collection;
            change.current_profile = switched.current_profile;
            change.notes.extend(switched.notes);
            change.problems.clear();
        }
        Ok(change)
    }

    /// Write a new collection or profile with id `id` (or one made from its
    /// name). Returns the id.
    fn add(&self, root: &Path, document: &Document, id: Option<&str>) -> Result<String, RpcError> {
        let kind = document.kind();
        let _files = self.saved.lock();
        // Imports/copies can keep the same display name while receiving a
        // fresh id. Give only the incoming document a distinguishable label.
        let mut document = document.clone();
        let entries = store::list(root, kind, None);
        let taken_name = |name: &str| {
            entries
                .iter()
                .any(|entry| entry.name.eq_ignore_ascii_case(name))
        };
        if taken_name(document.name()) {
            let base = document.name().to_string();
            let name = (2..)
                .map(|n| {
                    let suffix = format!(" ({n})");
                    let base: String = base.chars().take(120 - suffix.chars().count()).collect();
                    format!("{base}{suffix}")
                })
                .find(|name| !taken_name(name))
                .expect("an unused numbered name");
            document.set_name(&name);
        }
        let id = match id {
            Some(id) if !library::valid_id(id) => {
                return Err(invalid(format!(
                    "id: {id:?} must be lowercase letters, digits and dashes (at most 64)"
                )));
            }
            Some(id) if store::document_path(root, kind, id).exists() => {
                return Err(invalid(format!(
                    "id: {} {id:?} already exists; give another id, or none to make one from \
                     the name",
                    kind.label()
                )));
            }
            Some(id) => id.to_string(),
            None => {
                let base = library::slug(document.name(), kind.command());
                let taken = |id: &str| store::document_path(root, kind, id).exists();
                if taken(&base) {
                    (2..)
                        .map(|n| format!("{base}-{n}"))
                        .find(|id| !taken(id))
                        .unwrap()
                } else {
                    base
                }
            }
        };
        document
            .write(&store::document_path(root, kind, &id))
            .map_err(|err| failed(format!("couldn't save {} {id:?}: {err:#}", kind.label())))?;
        Ok(id)
    }

    /// Problems `document` would have with the other half in use (a switch
    /// to it is refused until they are fixed).
    fn problems_in_use(&self, document: &Document) -> Vec<Issue> {
        let (_, current) = self.library_view();
        let Some(current) = current else {
            return Vec::new();
        };
        let other = match document {
            Document::Collection(_) => {
                self.document(LibraryKind::Profile, "profile", &current.profile)
            }
            Document::Profile(_) => {
                self.document(LibraryKind::Collection, "collection", &current.collection)
            }
        };
        let setup = match (document, &other) {
            (Document::Collection(c), Ok(Document::Profile(p)))
            | (Document::Profile(p), Ok(Document::Collection(c))) => {
                Setup::from_parts(c.clone(), p.clone())
            }
            _ => return Vec::new(),
        };
        setup.validate()
    }

    pub(crate) fn library_rename(
        &self,
        params: LibraryRenameParams,
    ) -> Result<LibraryChange, RpcError> {
        let LibraryRenameParams { kind, id, name } = params;
        let name = check_name("name", &name)?;
        let (root, current) = self.library_view();
        check_exists(&root, kind, "id", &id)?;
        let in_use = current.as_ref().is_some_and(|c| match kind {
            LibraryKind::Collection => c.collection == id,
            LibraryKind::Profile => c.profile == id,
        });
        let live = self.state.lock().setup.is_some();
        let mut revision = None;
        let mut notes = Vec::new();
        if in_use && live && kind == LibraryKind::Collection {
            // The collection's name is the live setup's: a change like any
            // other, saved with it.
            let at = self.state.lock().revision;
            let applied = self.apply(ApplyParams {
                revision: at,
                change: Change::Patch(serde_json::json!({ "name": name })),
                live: false,
            })?;
            revision = Some(applied.revision);
            notes = applied.notes;
        } else {
            // A change waiting to be saved goes first; the file then gets
            // the new name (later saves keep it).
            self.flush_live_save();
            let _files = self.saved.lock();
            let path = store::document_path(&root, kind, &id);
            let mut document = match kind {
                LibraryKind::Collection => store::read_document(&path).map(Document::Collection),
                LibraryKind::Profile => store::read_profile(&path).map(Document::Profile),
            }
            .map_err(|(message, _)| failed(format!("id: {message}")))?;
            document.set_name(&name);
            document.write(&path).map_err(|err| {
                failed(format!("couldn't rename {} {id:?}: {err:#}", kind.label()))
            })?;
            if in_use && kind == LibraryKind::Profile {
                self.state.lock().file.renamed_profile(&name);
            }
        }
        self.library_changed(LibraryAction::Renamed, Some(kind), Some(&id));
        let mut change = self.change(vec![self.entry(kind, &id)]);
        change.revision = revision;
        change.notes = notes;
        Ok(change)
    }

    pub(crate) fn library_delete(&self, target: LibraryTarget) -> Result<LibraryChange, RpcError> {
        let LibraryTarget { kind, id } = target;
        let (root, _) = self.library_view();
        check_exists(&root, kind, "id", &id)?;
        // A change to it waiting to be saved goes first (it was in use a
        // moment ago).
        self.flush_live_save();
        let entry = self.entry(kind, &id);
        let kept = {
            let mut files = self.saved.lock();
            let in_use = self
                .state
                .lock()
                .file
                .current()
                .is_some_and(|c| match kind {
                    LibraryKind::Collection => c.collection == id,
                    LibraryKind::Profile => c.profile == id,
                });
            if in_use {
                let others: Vec<String> = store::list(&root, kind, None)
                    .into_iter()
                    .filter(|other| other.id != id)
                    .map(|other| format!("{} ({})", other.id, other.name))
                    .collect();
                return Err(wrong_state(format!(
                    "{} {id:?} is in use; switch to another first (`isocast {} switch ID`, or \
                     MCP's switch_collection_or_profile){}",
                    kind.label(),
                    kind.command(),
                    if others.is_empty() {
                        format!(", after making one (`isocast {} new NAME`)", kind.command())
                    } else {
                        format!("; there are: {}", others.join(", "))
                    }
                )));
            }
            let kept = store::delete(&root, kind, &id).map_err(|err| {
                failed(format!("couldn't delete {} {id:?}: {err:#}", kind.label()))
            })?;
            files.remove(&entry.path);
            kept
        };
        self.library_changed(LibraryAction::Deleted, Some(kind), Some(&id));
        let mut change = self.change(vec![LibraryEntry {
            current: false,
            ..entry
        }]);
        change.notes.push(format!(
            "deleted {} {id:?}; its file is kept as {}",
            kind.label(),
            kept.display()
        ));
        Ok(change)
    }

    pub(crate) fn library_switch(
        &self,
        params: LibrarySwitchParams,
    ) -> Result<LibraryChange, RpcError> {
        let what = match (&params.collection, &params.profile) {
            (None, None) => {
                return Err(invalid(
                    "give `collection`, `profile` or both: the ids to switch to (`library` lists \
                     them)",
                ));
            }
            (Some(_), Some(_)) => "the scene collection and profile",
            (Some(_), None) => "the scene collection",
            (None, Some(_)) => "the profile",
        };
        if let Some(message) = switch_refusal(&self.state.lock(), what) {
            return Err(wrong_state(message));
        }
        // The pair in use has its newest change on disk before another
        // takes over.
        self.flush_live_save();
        let (root, current) = self.library_view();
        let pick = |given: &Option<String>, kind: LibraryKind| -> Result<String, RpcError> {
            let in_use = current.as_ref().map(|c| match kind {
                LibraryKind::Collection => c.collection.clone(),
                LibraryKind::Profile => c.profile.clone(),
            });
            given.clone().or(in_use).ok_or_else(|| {
                invalid(format!(
                    "{}: there is no {} in use yet, so give one too; {}",
                    kind.command(),
                    kind.label(),
                    listing(&root, kind)
                ))
            })
        };
        let collection_id = pick(&params.collection, LibraryKind::Collection)?;
        let profile_id = pick(&params.profile, LibraryKind::Profile)?;
        let Document::Collection(collection) =
            self.document(LibraryKind::Collection, "collection", &collection_id)?
        else {
            unreachable!()
        };
        let Document::Profile(profile) =
            self.document(LibraryKind::Profile, "profile", &profile_id)?
        else {
            unreachable!()
        };
        let names = (collection.name.clone(), profile.name.clone());
        let setup = Setup::from_parts(collection, profile);
        let mut problems = setup.validate();
        if let Some(message) = relative_folder(&setup.recording.folder) {
            problems.push(Issue {
                path: "recording.folder".into(),
                message,
            });
        }
        if !problems.is_empty() {
            return Err(failed(format!(
                "scene collection {collection_id:?} with profile {profile_id:?} would have \
                 problems, so nothing changed:{}",
                describe(&problems)
            )));
        }
        let same = current
            .as_ref()
            .is_some_and(|c| c.collection == collection_id && c.profile == profile_id);
        let mut state = self.state.lock();
        let collection_changed = state
            .file
            .current()
            .is_none_or(|c| c.collection != collection_id);
        if same && state.setup.is_some() {
            drop(state);
            let mut change = self.change(vec![
                self.entry(LibraryKind::Collection, &collection_id),
                self.entry(LibraryKind::Profile, &profile_id),
            ]);
            change.notes.push("already in use; nothing changed".into());
            return Ok(change);
        }
        if let Some(message) = switch_refusal(&state, what) {
            return Err(wrong_state(message));
        }
        let leaving = collection_changed
            .then(|| {
                state.file.current().map(|c| {
                    (
                        store::document_path(&root, LibraryKind::Collection, &c.collection),
                        store::CollectionView {
                            scene: state.scene.clone(),
                            preview: state.preview.clone(),
                        },
                    )
                })
            })
            .flatten();
        let mut notes = Vec::new();
        let view = if collection_changed {
            let file = store::document_path(&root, LibraryKind::Collection, &collection_id);
            match store::read_view(&file) {
                Ok(view) => view,
                Err(err) => {
                    notes.push(format!(
                        "couldn't restore the collection's last scene: {err:#}"
                    ));
                    store::CollectionView::default()
                }
            }
        } else {
            store::CollectionView {
                scene: state.scene.clone(),
                preview: state.preview.clone(),
            }
        }
        .restore(&setup);
        // The live engine runs the new pair from its next frame: sources
        // open and close as for any change.
        if let Some(engine) = &state.engine {
            engine
                .update(&setup, state.revision + 1)
                .map_err(wrong_state)?;
        }
        state.setup = Some(setup);
        state.hotkey_holds.clear();
        state.revision += 1;
        let revision = state.revision;
        let written = state
            .file
            .set_current((&collection_id, &profile_id), (&names.0, &names.1));
        // Each collection remembers its own on-air and Studio preview ids.
        let mut on_air = None;
        if collection_changed || state.scene != view.scene {
            state.scene = view.scene;
            if let (Some(control), Some(scene)) = (state.control(), &state.scene) {
                let _ = control.switch_scene(now(), scene.clone(), Transition::default());
            }
            on_air = state.scene.clone();
        }
        let preview_changed = collection_changed || state.preview != view.preview;
        state.preview = view.preview;
        if let Some(engine) = &state.engine {
            engine.set_preview(state.preview.clone());
        }
        let preview = state.preview.clone();
        drop(state);
        {
            let _files = self.saved.lock();
            if let Some((file, view)) = leaving
                && let Err(err) = store::write_view(&file, &view)
            {
                notes.push(format!(
                    "switched, but couldn't remember the previous collection's scene: {err:#}"
                ));
            }
            if let Err(err) = store::write_current(&root, &written) {
                notes.push(format!(
                    "switched, but couldn't save which ones are in use ({err:#}); isocastd \
                     would start with the ones before"
                ));
            }
        }
        // Kept data keys belong to the collection: the one before's are
        // saved with it, and this one's come back.
        if collection_changed && let Some(file) = self.collection_file() {
            notes.extend(self.data.load_collection(&file));
        }
        self.publish(Event::SetupChanged { revision });
        if preview_changed {
            self.publish(match preview {
                Some(scene) => Event::PreviewChanged { scene },
                None => Event::PreviewCleared,
            });
        }
        if let Some(scene) = on_air {
            self.publish(Event::SceneChanged {
                scene,
                take: None,
                time_seconds: None,
                transition: None,
            });
        }
        let (kind, id) = match (&params.collection, &params.profile) {
            (Some(id), None) => (Some(LibraryKind::Collection), Some(id.as_str())),
            (None, Some(id)) => (Some(LibraryKind::Profile), Some(id.as_str())),
            _ => (None, None),
        };
        self.library_changed(LibraryAction::Switched, kind, id);
        let mut change = self.change(vec![
            self.entry(LibraryKind::Collection, &collection_id),
            self.entry(LibraryKind::Profile, &profile_id),
        ]);
        change.revision = Some(revision);
        change.notes = notes;
        Ok(change)
    }

    pub(crate) fn library_import(
        &self,
        params: LibraryImportParams,
    ) -> Result<LibraryChange, RpcError> {
        let rename = |mut document: Document| -> Result<Document, RpcError> {
            if let Some(name) = &params.name {
                document.set_name(&check_name("name", name)?);
            }
            Ok(document)
        };
        let documents = match (
            params.setup.clone(),
            params.collection.clone(),
            params.profile.clone(),
        ) {
            (Some(setup), None, None) => {
                let problems = setup.validate();
                if !problems.is_empty() {
                    return Err(invalid(format!(
                        "setup: it has problems, so nothing was imported:{}",
                        describe(&problems)
                    )));
                }
                let name = params.name.clone().unwrap_or_else(|| setup.name.clone());
                let (collection, profile) = setup.into_parts(&name);
                vec![
                    rename(Document::Collection(collection))?,
                    rename(Document::Profile(profile))?,
                ]
            }
            (None, collection, profile) if collection.is_some() || profile.is_some() => {
                let mut documents = Vec::new();
                if let Some(collection) = collection {
                    documents.push(rename(Document::Collection(collection))?);
                }
                if let Some(profile) = profile {
                    documents.push(rename(Document::Profile(profile))?);
                }
                documents
            }
            _ => {
                return Err(invalid(
                    "give `setup` (a whole setup, imported as a collection and a profile), or \
                     `collection` and/or `profile`",
                ));
            }
        };
        self.add_documents(documents, params.switch, Vec::new(), Vec::new())
    }

    /// Add imported collections and profiles (refusing any with problems of
    /// their own, before writing anything), and switch to them if asked.
    fn add_documents(
        &self,
        documents: Vec<Document>,
        switch: bool,
        unsupported: Vec<obs::Unsupported>,
        mut notes: Vec<String>,
    ) -> Result<LibraryChange, RpcError> {
        for document in &documents {
            let problems = document.problems();
            if !problems.is_empty() {
                return Err(invalid(format!(
                    "the {} {:?} has problems, so nothing was imported:{}",
                    document.kind().label(),
                    document.name(),
                    describe(&problems)
                )));
            }
        }
        let (root, _) = self.library_view();
        let mut added = Vec::new();
        for document in &documents {
            let id = self.add(&root, document, None)?;
            self.library_changed(LibraryAction::Imported, Some(document.kind()), Some(&id));
            added.push((document.kind(), id));
        }
        let entries = added
            .iter()
            .map(|(kind, id)| self.entry(*kind, id))
            .collect();
        let mut change = self.change(entries);
        change.unsupported = unsupported;
        // Problems with the other half in use, or with each other.
        change.problems = match documents.as_slice() {
            [Document::Collection(c), Document::Profile(p)] => {
                Setup::from_parts(c.clone(), p.clone()).validate()
            }
            [document] => self.problems_in_use(document),
            _ => Vec::new(),
        };
        if switch {
            let id = |kind: LibraryKind| {
                added
                    .iter()
                    .find(|(k, _)| *k == kind)
                    .map(|(_, id)| id.clone())
            };
            let switched = self
                .library_switch(LibrarySwitchParams {
                    collection: id(LibraryKind::Collection),
                    profile: id(LibraryKind::Profile),
                })
                .map_err(|err| RpcError {
                    message: format!("imported, but didn't switch: {}", err.message),
                    ..err
                })?;
            change.revision = switched.revision;
            change.current_collection = switched.current_collection;
            change.current_profile = switched.current_profile;
            notes.extend(switched.notes);
            change.problems.clear();
        }
        change.notes = notes;
        Ok(change)
    }

    pub(crate) fn library_import_obs(
        &self,
        params: LibraryImportObsParams,
    ) -> Result<LibraryChange, RpcError> {
        if params.collection.is_none() && params.profile.is_none() {
            return Err(invalid(
                "give `collection` (an OBS scene collection's name or .json file), `profile` (an \
                 OBS profile's name, folder or basic.ini) or both",
            ));
        }
        let canvas = match (params.width, params.height) {
            (Some(width), Some(height)) if width > 0 && height > 0 => Some((width, height)),
            (None, None) => None,
            _ => {
                return Err(invalid(
                    "width: give the canvas width and height together, above 0",
                ));
            }
        };
        let home = std::env::var("HOME").ok();
        let mut documents = Vec::new();
        let mut unsupported = Vec::new();
        let mut notes = Vec::new();
        let mut iso = None;
        // A stream key OBS keeps in plain text, moved only when asked.
        let mut stream_key = None;
        if params.stream_key && params.profile.is_none() {
            return Err(invalid(
                "stream_key: give the OBS `profile` whose service.json holds the key",
            ));
        }
        if let Some(collection) = &params.collection {
            let devices = devices();
            let options = obs::Options {
                canvas,
                fps: None,
                profile: None,
                devices: devices.as_ref().ok(),
                content_size: &content_size,
                home: home.as_deref(),
            };
            #[cfg(target_os = "macos")]
            crate::SLOW_CONTENT.store(0, std::sync::atomic::Ordering::SeqCst);
            let (imported, _profile_findings) =
                obs::import_collection_file(collection, params.profile.as_deref(), options)
                    .map_err(|message| invalid(format!("collection: {message}")))?;
            #[cfg(target_os = "macos")]
            match crate::SLOW_CONTENT.load(std::sync::atomic::Ordering::SeqCst) {
                0 => {}
                n => notes.push(format!(
                    "{n} file(s) took too long to read, so their sizes were guessed; macOS may \
                     be asking for permission to read their folder (like Downloads): allow it \
                     and import again"
                )),
            }
            if let Err(err) = devices {
                notes.push(format!(
                    "devices couldn't be listed ({err:#}), so device names and sizes weren't \
                     checked"
                ));
            }
            let (mut part, profile_part) = imported.setup.into_parts("");
            if let Some(name) = &params.name {
                part.name = check_name("name", name)?;
            }
            let iso_sources = part.recording.iso.sources.len();
            if iso_sources > 0 && params.profile.is_none() {
                notes.push(format!(
                    "Source Record recorded {iso_sources} source(s): they are the collection's \
                     ISO sources; whether and how ISO files are written is the profile's \
                     (recording.iso), so import the OBS profile too or check the one in use"
                ));
            }
            iso = Some(profile_part.recording.iso);
            documents.push(Document::Collection(part));
            unsupported.extend(imported.unsupported);
            notes.extend(imported.notes);
        }
        if let Some(profile) = &params.profile {
            let imported = obs::import_profile(profile, home.as_deref())
                .map_err(|message| invalid(format!("profile: {message}")))?;
            let mut part = imported.profile;
            if params.stream_key {
                match (&part.stream, obs::plain_stream_key(&imported.path)) {
                    (Some(stream), Some(key)) => stream_key = Some((stream.clone(), key)),
                    (None, _) => notes.push(
                        "stream_key: the OBS profile has no stream Isocast can use, so its key \
                         stayed behind"
                            .into(),
                    ),
                    (Some(_), None) => notes.push(
                        "stream_key: OBS keeps no stream key in this profile's service.json \
                         (or keeps it in the Keychain), so set it with `isocast stream key set`"
                            .into(),
                    ),
                }
            }
            if let Some(name) = &params.name {
                part.name = check_name("name", name)?;
            }
            // Source Record's files come with the collection.
            if let Some(iso) = iso.filter(|iso| iso.enabled) {
                part.recording.iso = iso;
            }
            documents.push(Document::Profile(part));
            unsupported.extend(imported.unsupported);
            notes.extend(imported.notes);
        }
        let mut change = self.add_documents(documents, params.switch, unsupported, notes)?;
        if let Some((stream, key)) = stream_key {
            let server = (stream.service == isocast_protocol::stream::StreamService::Custom)
                .then(|| stream.server.clone())
                .flatten();
            let params = isocast_protocol::api::StreamKeyParams {
                key,
                target: isocast_protocol::api::StreamKeyTarget {
                    destination: None,
                    service: Some(stream.service),
                    server,
                },
            };
            match self.stream_key_set(params) {
                Ok(stored) => {
                    // The profile's own note says the key stayed behind.
                    change
                        .notes
                        .retain(|n| !n.contains("its stream key stays in OBS"));
                    change.notes.push(format!(
                        "moved the stream key from OBS's service.json to the Keychain (account \
                     {:?}); OBS's copy is untouched",
                        stored.account
                    ))
                }
                Err(err) => change.notes.push(format!(
                    "imported, but the stream key couldn't be kept in the Keychain: {}",
                    err.message
                )),
            }
        }
        Ok(change)
    }
}
