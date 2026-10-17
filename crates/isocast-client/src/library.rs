//! Scene collection and profile files, on the client's side: reading one
//! someone picked to import, as `library_import` (or `library_import_obs`)
//! params, and writing one `library_get` returned, to export it. The CLI
//! (`isocast collection|profile import|export`) and both apps' Profile and
//! Scene Collection menus use these, so a file means the same everywhere.
//! The daemon never opens the files people pick; it takes documents.

use std::path::Path;

use anyhow::{Context, Result, bail};
use isocast_protocol::Setup;
use isocast_protocol::api::Method;
use isocast_protocol::library::{
    self, Collection, LibraryDocument, LibraryEntry, LibraryImportObsParams, LibraryImportParams,
    LibraryKind, Profile,
};

/// A file to import, as `library_import` params: a collection or profile
/// file (of the kind asked for, as `export` writes them, .toml or .json)
/// or a whole setup (its collection and its profile). Relative paths in it
/// are made absolute against the file's folder (isocastd runs elsewhere).
pub fn read_import(kind: LibraryKind, path: &Path) -> Result<LibraryImportParams> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("could not read {}", path.display()))?;
    let json = path.extension().is_some_and(|e| e == "json");
    let folder = std::path::absolute(path)?
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let mut params = LibraryImportParams {
        setup: None,
        collection: None,
        profile: None,
        name: None,
        switch: false,
    };
    let as_collection = library::parse::<Collection>(&text, json);
    let as_profile = library::parse::<Profile>(&text, json);
    match (kind, as_collection, as_profile) {
        (LibraryKind::Collection, Ok(collection), _) => {
            let mut setup = Setup::from_parts(collection, Profile::starter(""));
            setup.resolve_paths(&folder);
            params.collection = Some(setup.into_parts("").0);
            Ok(params)
        }
        (LibraryKind::Profile, _, Ok(mut profile)) => {
            profile.recording.folder = absolute_folder(&profile.recording.folder, &folder);
            params.profile = Some(profile);
            Ok(params)
        }
        (LibraryKind::Collection, Err(_), Ok(_)) => bail!(
            "{} is a profile, not a scene collection; import it as a profile (`isocast profile \
             import`, or Profile → Import… in the app)",
            path.display()
        ),
        (LibraryKind::Profile, Ok(_), Err(_)) => bail!(
            "{} is a scene collection, not a profile; import it as a scene collection (`isocast \
             collection import`, or Scene Collection → Import… in the app)",
            path.display()
        ),
        (LibraryKind::Collection, Err(error), _) | (LibraryKind::Profile, _, Err(error)) => {
            let setup = if json {
                Setup::from_json(&text)
            } else {
                Setup::from_toml(&text)
            };
            match setup {
                Ok(mut setup) => {
                    setup.resolve_paths(&folder);
                    setup.recording.folder = absolute_folder(&setup.recording.folder, &folder);
                    params.setup = Some(setup);
                    Ok(params)
                }
                Err(_) => bail!(
                    "{} is not a {} file (nor a whole setup): {error}",
                    path.display(),
                    kind.label()
                ),
            }
        }
    }
}

/// What the apps' Import… does with a file: an Isocast file as
/// [`read_import`] reads it, or one from OBS Studio, as OBS's own Import
/// takes them: a scene collection's .json (for a scene collection), a
/// profile's basic.ini or its folder (for a profile). The method to call;
/// nothing is switched to.
pub fn import_method(kind: LibraryKind, path: &Path) -> Result<Method> {
    let absolute = || -> Result<String> { Ok(std::path::absolute(path)?.display().to_string()) };
    let obs_profile = path.is_dir() || path.extension().is_some_and(|e| e == "ini");
    if kind == LibraryKind::Profile && obs_profile {
        return Ok(Method::LibraryImportObs(Box::new(LibraryImportObsParams {
            profile: Some(absolute()?),
            ..LibraryImportObsParams::default()
        })));
    }
    match read_import(kind, path) {
        Ok(params) => Ok(Method::LibraryImport(Box::new(params))),
        Err(_) if kind == LibraryKind::Collection && is_obs_collection(path) => {
            Ok(Method::LibraryImportObs(Box::new(LibraryImportObsParams {
                collection: Some(absolute()?),
                ..LibraryImportObsParams::default()
            })))
        }
        Err(error) => Err(error),
    }
}

/// Whether `path` looks like an OBS Studio scene collection: JSON with
/// OBS's `current_scene` or `scene_order`.
fn is_obs_collection(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "json")
        && std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .is_some_and(|value| {
                value.get("current_scene").is_some() || value.get("scene_order").is_some()
            })
}

/// A recording folder made absolute from `base` if it is relative (isocastd
/// runs elsewhere, and refuses one).
fn absolute_folder(folder: &str, base: &Path) -> String {
    if folder.is_empty() || Path::new(folder).is_absolute() || folder.starts_with('~') {
        folder.to_string()
    } else {
        base.join(folder).display().to_string()
    }
}

/// An exported scene collection or profile as a file's text: JSON (`json`)
/// or TOML with a comment saying what it is; `import` reads either back.
pub fn export_text(document: &LibraryDocument, json: bool) -> Result<String> {
    let entry = &document.entry;
    Ok(match (&document.collection, &document.profile, json) {
        (Some(collection), _, true) => serde_json::to_string_pretty(collection)? + "\n",
        (_, Some(profile), true) => serde_json::to_string_pretty(profile)? + "\n",
        (Some(collection), _, false) => header(entry) + &library::to_toml(collection)?,
        (_, Some(profile), false) => header(entry) + &library::to_toml(profile)?,
        (None, None, _) => bail!("isocastd sent no document"),
    })
}

/// Write `document` to `path`: JSON if it ends in .json, else TOML.
pub fn write_export(document: &LibraryDocument, path: &Path) -> Result<()> {
    let json = path.extension().is_some_and(|e| e == "json");
    let text = export_text(document, json)?;
    std::fs::write(path, text).with_context(|| format!("could not write {}", path.display()))
}

fn header(entry: &LibraryEntry) -> String {
    let menu = match entry.kind {
        LibraryKind::Collection => "Scene Collection",
        LibraryKind::Profile => "Profile",
    };
    format!(
        "# An exported Isocast {}; add it with `isocast {} import FILE` or {menu} → Import… in \
         the app.\n\n",
        entry.kind.label(),
        entry.kind.command(),
    )
}
