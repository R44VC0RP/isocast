//! Picking a recording's project folder (7.7b); the folder format is in
//! `isocast_protocol::project`.

use std::path::{Path, PathBuf};

use chrono::Local;
use isocast_protocol::project;

/// The project folder a recording asked to write into `folder` uses:
/// `folder` itself if it is a project's (the new take joins it), else a new project folder made in it, named for now. Returns
/// the folder, its project file, and whether it was made.
pub fn begin(folder: &Path) -> std::io::Result<(PathBuf, project::ProjectFile, bool)> {
    if let Some(file) = project::read(folder) {
        return Ok((folder.to_path_buf(), file, false));
    }
    let now = Local::now();
    let (made, file) = project::create(
        folder,
        &now.format("%Y-%m-%d %H-%M-%S").to_string(),
        &now.format("%Y%m%d-%H%M%S").to_string(),
        &now.to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
    )?;
    Ok((made, file, true))
}
