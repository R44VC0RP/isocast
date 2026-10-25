//! The folders other computers can use, and the guard rails for network
//! callers (docs/remote-engines.md, decisions 25 and 26).
//!
//! Folder roots and the browser rule are guard rails, best effort, not a
//! sandbox (decision 26): paired clients are trusted show operators. Every
//! path a network caller (a paired client or an obs-websocket client)
//! chooses, read or written, and every path the engine derives for its
//! request (a default report folder), must
//! resolve inside a root, symlinks included. Checked: method params
//! (`check_params`, which also hands the resolved path onward), files a
//! setup change, import or library switch brings in (`check_setup`), and
//! derived destinations where they're decided (`check_path`). Browser
//! sources from a network caller reach http(s) on the internet and the
//! LAN (the WHATWG-parsed host, loaded in that normalized form), never
//! loopback, link-local or files outside the roots. Local callers are
//! never checked here.
//!
//! Accepted residuals: a symlink swapped after the check, browser
//! redirects, DNS rebinding, and paths nested inside project or import
//! files.
//!
//! The roots: the folders chosen on this machine (`folder_roots_set`;
//! Movies or Videos, Pictures, Music, Desktop and Downloads by default),
//! the recordings folders set on this machine (a local caller's live
//! setup), and the font folders (read only), kept in
//! `<setup>.roots.json`. Recording history (folders recorded into lately)
//! never becomes a root, and neither does a folder a network caller set.

use std::net::{IpAddr, ToSocketAddrs};
use std::path::Component;

use isocast_protocol::roots::{
    FILES_LIST_MAX, FileEntry, FilesList, FilesListParams, FilesRoots, FolderRoot, FolderRoots,
    FolderRootsSetParams,
};

use super::*;

/// Recordings folders set on this machine, remembered at most.
const MAX_LOCAL_RECORDING_FOLDERS: usize = 20;

#[derive(Default)]
pub(super) struct Runtime {
    /// What `<setup>.roots.json` holds, once read.
    saved: Mutex<Option<Saved>>,
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    /// The chosen folders; the defaults if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    folders: Option<Vec<String>>,
    /// Recordings folders a local caller's live setup has had, newest
    /// last.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    recording_folders: Vec<String>,
}

fn settings_path() -> PathBuf {
    isocast_protocol::api::setup_path().with_extension("roots.json")
}

/// Read or write.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    Read,
    Write,
}

/// A root, resolved.
struct Root {
    path: PathBuf,
    /// How many of `path`'s components exist (canonical, exact case).
    existing: usize,
    shown: PathBuf,
    read_only: bool,
    why: &'static str,
}

fn default_folders() -> Vec<String> {
    let movies = if cfg!(target_os = "macos") {
        "~/Movies"
    } else {
        "~/Videos"
    };
    [movies, "~/Pictures", "~/Music", "~/Desktop", "~/Downloads"]
        .into_iter()
        .map(String::from)
        .collect()
}

fn font_folders() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if cfg!(target_os = "macos") {
        out.push(PathBuf::from("/System/Library/Fonts"));
        out.push(PathBuf::from("/Library/Fonts"));
        out.push(expand_home("~/Library/Fonts"));
    } else if cfg!(windows) {
        if let Some(windir) = std::env::var_os("WINDIR") {
            out.push(PathBuf::from(windir).join("Fonts"));
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            out.push(PathBuf::from(local).join("Microsoft\\Windows\\Fonts"));
        }
    } else {
        out.push(PathBuf::from("/usr/share/fonts"));
        out.push(expand_home("~/.local/share/fonts"));
        out.push(expand_home("~/.fonts"));
    }
    out
}

/// A UNC path (`\\server\share`, `\\?\UNC\…`): resolving it would connect
/// to that server, so network callers can't give one.
fn is_unc(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::path::Prefix;
        matches!(
            path.components().next(),
            Some(Component::Prefix(prefix))
                if matches!(prefix.kind(), Prefix::UNC(..) | Prefix::VerbatimUNC(..) | Prefix::DeviceNS(..))
        )
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

/// `path` as people write it: on Windows without the `\\?\` a resolved
/// path starts with (`\\?\C:\Users` is `C:\Users`).
fn plain(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\")
            && rest.as_bytes().get(1) == Some(&b':')
        {
            return PathBuf::from(rest);
        }
    }
    path
}

/// Whether names in `path` (a folder that exists) compare without case:
/// asked of macOS (`pathconf`). Windows can make single folders case
/// sensitive, and asking needs a handle per folder, so there names compare
/// exactly (refusing a differently cased name that doesn't exist yet is
/// the conservative side); elsewhere exactly too.
fn caseless(path: &Path) -> bool {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut at = path;
        loop {
            if at.exists() {
                let Ok(c) = std::ffi::CString::new(at.as_os_str().as_bytes()) else {
                    return true;
                };
                // SAFETY: a valid NUL-terminated path; pathconf only reads it.
                let sensitive = unsafe { libc::pathconf(c.as_ptr(), libc::_PC_CASE_SENSITIVE) };
                return sensitive != 1;
            }
            match at.parent() {
                Some(parent) => at = parent,
                None => return true,
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        false
    }
}

/// `path` with its symlinks resolved: the longest part that exists,
/// canonicalized, then the rest. Refused: relative paths, `.` and `..`,
/// UNC paths, and a dangling symlink (what it points at could be
/// anywhere). Also how many components of the result exist (canonical, in
/// the case the file system keeps).
fn resolve_parts(path: &Path) -> Result<(PathBuf, usize), String> {
    if !path.is_absolute() {
        return Err("give an absolute path (or one starting with ~/)".into());
    }
    if is_unc(path) {
        return Err("give a path on this computer, not a network share".into());
    }
    if path
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err("give a path without `.` or `..` in it".into());
    }
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    loop {
        match std::fs::canonicalize(&existing) {
            Ok(mut resolved) => {
                let existing = resolved.components().count();
                for part in rest.iter().rev() {
                    resolved.push(part);
                }
                return Ok((resolved, existing));
            }
            Err(_) => {
                if std::fs::symlink_metadata(&existing).is_ok() {
                    return Err(format!(
                        "{} is a link to something that isn't there",
                        existing.display()
                    ));
                }
                let Some(name) = existing.file_name() else {
                    return Err("no part of it exists".into());
                };
                rest.push(name.to_os_string());
                if !existing.pop() {
                    return Err("no part of it exists".into());
                }
            }
        }
    }
}

/// Whether `path` is `root` or inside it. Components that exist on both
/// sides are canonical and compare exactly; a component that doesn't exist
/// yet (on either side) compares ignoring ASCII case only where its folder
/// is `caseless`, and exactly otherwise.
fn within(path: (&Path, usize), root: (&Path, usize), caseless: bool) -> bool {
    let mut parts = path.0.components();
    root.0
        .components()
        .enumerate()
        .all(|(i, r)| match parts.next() {
            Some(p) if i < path.1 && i < root.1 => p == r,
            Some(p) if caseless => match (p.as_os_str().to_str(), r.as_os_str().to_str()) {
                (Some(p), Some(r)) => p.eq_ignore_ascii_case(r),
                _ => p == r,
            },
            Some(p) => p == r,
            None => false,
        })
}

/// Whether `ip` is this machine's own (loopback, unspecified) or
/// link-local, after unmapping IPv4-mapped IPv6.
fn local_only_address(ip: IpAddr) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    };
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback() || v4.is_unspecified() || v4.octets()[0] == 0 || v4.is_link_local()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Every browser source's URL in its WHATWG-normalized form, the form a
/// network caller's URL was checked in (decision 26): the page loaded is
/// the host checked.
pub(crate) fn normalize_browser_urls(setup: &mut Setup) {
    for source in &mut setup.sources {
        if let isocast_protocol::SourceKind::Browser(browser) = &mut source.kind
            && let Some(url) = &mut browser.url
            && let Ok(parsed) = url::Url::parse(url)
        {
            *url = parsed.to_string();
        }
    }
}

fn refused(message: String) -> RpcError {
    RpcError {
        code: RpcError::NOT_PERMITTED,
        message,
    }
}

impl Runtime {
    fn saved(&self) -> Saved {
        let mut saved = self.saved.lock();
        if saved.is_none() {
            *saved = Some(match std::fs::read(settings_path()) {
                Ok(bytes) => serde_json::from_slice::<Saved>(&bytes).unwrap_or_else(|err| {
                    // Fail closed: no chosen folders until they're chosen
                    // again here.
                    isocast_diagnostics::diagnostic!("warn";
                        "isocastd: {} didn't load ({err}); other computers can use no folders \
                         until they're chosen again",
                        settings_path().display()
                    );
                    Saved {
                        folders: Some(Vec::new()),
                        recording_folders: Vec::new(),
                    }
                }),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Saved::default(),
                Err(err) => {
                    isocast_diagnostics::diagnostic!("warn";
                        "isocastd: could not read {}: {err}", settings_path().display()
                    );
                    Saved {
                        folders: Some(Vec::new()),
                        recording_folders: Vec::new(),
                    }
                }
            });
        }
        saved.clone().unwrap_or_default()
    }

    fn save(&self, saved: Saved) -> Result<(), RpcError> {
        let path = settings_path();
        let partial = path.with_extension("json.partial");
        let bytes = serde_json::to_vec_pretty(&saved).map_err(|err| failed(err.into()))?;
        std::fs::write(&partial, bytes)
            .and_then(|()| isocast_protocol::fs::replace(&partial, &path))
            .map_err(|err| failed(anyhow::anyhow!("could not save {}: {err}", path.display())))?;
        *self.saved.lock() = Some(saved);
        Ok(())
    }
}

impl Daemon {
    /// The roots, resolved.
    fn roots(&self) -> Vec<Root> {
        let saved = self.roots.saved();
        let custom = saved.folders.is_some();
        let mut roots = Vec::new();
        let mut add = |shown: PathBuf, read_only: bool, why: &'static str| {
            // A network share isn't resolved (that would connect to it).
            let path = if is_unc(&shown) {
                Ok((shown.clone(), shown.components().count()))
            } else {
                resolve_parts(&shown)
            };
            if let Ok((path, existing)) = path {
                roots.push(Root {
                    path,
                    existing,
                    shown,
                    read_only,
                    why,
                });
            }
        };
        for folder in saved.folders.unwrap_or_else(default_folders) {
            add(
                expand_home(&folder),
                false,
                if custom { "chosen" } else { "default" },
            );
        }
        for folder in saved.recording_folders {
            add(expand_home(&folder), false, "recordings");
        }
        for folder in font_folders() {
            add(folder, true, "fonts");
        }
        roots
    }

    /// A local caller's live setup records into `folder`: it is a
    /// recordings folder set on this machine, a root from now on.
    pub(crate) fn note_local_recording_folder(&self, folder: &str) {
        if !callers::current().is_local() {
            return;
        }
        let mut saved = self.roots.saved();
        if saved.recording_folders.iter().any(|f| f == folder) {
            return;
        }
        saved.recording_folders.push(folder.to_string());
        let excess = saved
            .recording_folders
            .len()
            .saturating_sub(MAX_LOCAL_RECORDING_FOLDERS);
        saved.recording_folders.drain(..excess);
        if let Err(err) = self.roots.save(saved) {
            isocast_diagnostics::diagnostic!("warn"; "isocastd: {}", err.message);
        }
    }

    /// See [`Method::FolderRoots`].
    pub(crate) fn folder_roots(&self) -> FolderRoots {
        FolderRoots {
            roots: self
                .roots()
                .into_iter()
                .map(|root| FolderRoot {
                    path: root.shown,
                    read_only: root.read_only,
                    why: root.why.into(),
                })
                .collect(),
            custom: self.roots.saved().folders.is_some(),
        }
    }

    /// See [`Method::FolderRootsSet`] (local only).
    pub(crate) fn folder_roots_set(
        &self,
        params: FolderRootsSetParams,
    ) -> Result<FolderRoots, RpcError> {
        if let Some(folders) = &params.folders {
            for (i, folder) in folders.iter().enumerate() {
                let path = expand_home(folder);
                if !path.is_absolute() {
                    return Err(RpcError {
                        code: RpcError::INVALID_PARAMS,
                        message: format!(
                            "folders[{i}]: {folder:?}: give an absolute path (or one starting \
                             with ~/)"
                        ),
                    });
                }
            }
        }
        let mut saved = self.roots.saved();
        saved.folders = params.folders;
        self.roots.save(saved)?;
        Ok(self.folder_roots())
    }

    /// See [`Method::FilesRoots`].
    pub(crate) fn files_roots(&self) -> FilesRoots {
        FilesRoots {
            roots: self
                .roots()
                .into_iter()
                .filter(|root| root.path.is_dir())
                .map(|root| FolderRoot {
                    path: root.shown,
                    read_only: root.read_only,
                    why: root.why.into(),
                })
                .collect(),
            home: isocast_protocol::api::home_folder(),
        }
    }

    /// See [`Method::FilesList`]. A network caller's `path` was resolved
    /// inside a root by `check_params`; each entry it gets must resolve
    /// inside one too (a link out of the roots is left out). Listing only:
    /// nothing is opened but the folder.
    pub(crate) fn files_list(&self, params: FilesListParams) -> Result<FilesList, RpcError> {
        let local = callers::current().is_local();
        let shown = params.path.display().to_string();
        let invalid = |problem: String| RpcError {
            code: RpcError::INVALID_PARAMS,
            message: format!("params.path: {shown:?}: {problem}"),
        };
        let path = if local {
            let path = expand_home(&params.path.to_string_lossy());
            if !path.is_absolute() {
                return Err(invalid(
                    "give an absolute path (or one starting with ~/)".into(),
                ));
            }
            std::fs::canonicalize(&path).map_err(|err| invalid(err.to_string()))?
        } else {
            params.path.clone()
        };
        let roots = if local { Vec::new() } else { self.roots() };
        let allowed =
            |path: &Path| local || self.resolve_in(&roots, "", path, Access::Read).is_ok();
        let read = std::fs::read_dir(&path).map_err(|err| invalid(err.to_string()))?;
        let extensions: Option<Vec<String>> = params.extensions.map(|list| {
            list.iter()
                .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
                .collect()
        });
        let mut entries = Vec::new();
        let mut truncated = false;
        for entry in read.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                // FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_SYSTEM, on the
                // entry itself (a junction like "Application Data" is
                // hidden; what it points at isn't).
                if entry
                    .metadata()
                    .is_ok_and(|own| own.file_attributes() & 0x6 != 0)
                {
                    continue;
                }
            }
            // Follows a link: what it points at decides folder or file.
            let Ok(meta) = std::fs::metadata(entry.path()) else {
                continue;
            };
            let folder = meta.is_dir();
            if params.folders_only && !folder {
                continue;
            }
            if !folder && let Some(extensions) = &extensions {
                let extension = Path::new(&name)
                    .extension()
                    .map(|e| e.to_string_lossy().to_ascii_lowercase())
                    .unwrap_or_default();
                if !extensions.contains(&extension) {
                    continue;
                }
            }
            let entry_path = path.join(&name);
            if !allowed(&entry_path) {
                continue;
            }
            if entries.len() == FILES_LIST_MAX {
                truncated = true;
                break;
            }
            entries.push(FileEntry {
                name,
                path: plain(entry_path),
                folder,
                size: (!folder).then_some(meta.len()),
                modified: meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs()),
            });
        }
        entries.sort_by(|a, b| {
            b.folder
                .cmp(&a.folder)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        let parent = path
            .parent()
            .filter(|parent| allowed(parent))
            .map(|parent| plain(parent.to_path_buf()));
        Ok(FilesList {
            path: plain(path),
            parent,
            entries,
            truncated,
        })
    }

    /// `path` (given at `field`) resolved, if it is inside a root that
    /// allows `access`; refused otherwise.
    fn resolve_in(
        &self,
        roots: &[Root],
        field: &str,
        path: &Path,
        access: Access,
    ) -> Result<PathBuf, RpcError> {
        let machine = callers::machine_name();
        let shown = path.display().to_string();
        let (resolved, existing) = resolve_parts(path)
            .map_err(|problem| refused(format!("{field}: {shown:?}: {problem}")))?;
        // The folder a not-yet-existing part would be made in.
        let parent: PathBuf = resolved.components().take(existing).collect();
        let caseless = caseless(&parent);
        if roots.iter().any(|root| {
            within((&resolved, existing), (&root.path, root.existing), caseless)
                && !(access == Access::Write && root.read_only)
        }) {
            return Ok(resolved);
        }
        let list: Vec<String> = roots
            .iter()
            .filter(|root| !(access == Access::Write && root.read_only))
            .map(|root| format!("{:?}", root.shown.display().to_string()))
            .collect();
        Err(refused(format!(
            "{field}: {shown:?} isn't in a folder other computers can use ({}); choose those \
             on {machine} (`isocast roots set`)",
            list.join(", ")
        )))
    }

    fn check_path_in(
        &self,
        roots: &[Root],
        field: &str,
        path: &str,
        access: Access,
    ) -> Result<(), RpcError> {
        self.resolve_in(roots, field, &expand_home(path), access)
            .map(drop)
    }

    /// Refuse `path` for a network caller unless it is inside a root that
    /// allows `access`; local callers pass.
    pub(crate) fn check_path(
        &self,
        field: &str,
        path: &Path,
        access: Access,
    ) -> Result<(), RpcError> {
        self.checked_path(field, &path.to_string_lossy(), access)
            .map(drop)
    }

    /// The path to use for `path`: for a network caller, resolved
    /// (symlinks followed), and refused unless inside a root that allows
    /// `access`; for a local caller, as given. Use the result right away.
    pub(crate) fn checked_path(
        &self,
        field: &str,
        path: &str,
        access: Access,
    ) -> Result<PathBuf, RpcError> {
        if callers::current().is_local() {
            return Ok(PathBuf::from(path));
        }
        self.resolve_in(&self.roots(), field, &expand_home(path), access)
    }

    /// A browser source's URL from a network caller, parsed as a browser
    /// parses it (WHATWG): http(s) to anything but this machine's own or
    /// link-local addresses (the host and what it resolves to now), or a
    /// file URL inside a root.
    fn check_browser_url(&self, roots: &[Root], field: &str, raw: &str) -> Result<(), RpcError> {
        let machine = callers::machine_name();
        let parsed = url::Url::parse(raw)
            .map_err(|err| refused(format!("{field}: {raw:?} isn't a URL ({err})")))?;
        match parsed.scheme() {
            "file" => {
                if parsed
                    .host_str()
                    .is_some_and(|host| !host.is_empty() && host != "localhost")
                {
                    return Err(refused(format!(
                        "{field}: {raw:?} names a file on another computer; from another \
                         computer a browser source opens files on {machine}"
                    )));
                }
                let path = parsed.to_file_path().map_err(|()| {
                    refused(format!("{field}: {raw:?} isn't a file path on {machine}"))
                })?;
                self.resolve_in(roots, field, &path, Access::Read).map(drop)
            }
            "http" | "https" => {
                let addresses: Vec<IpAddr> = match parsed.host() {
                    Some(url::Host::Ipv4(ip)) => vec![IpAddr::V4(ip)],
                    Some(url::Host::Ipv6(ip)) => vec![IpAddr::V6(ip)],
                    Some(url::Host::Domain(host)) => {
                        let port = parsed.port_or_known_default().unwrap_or(80);
                        match (host, port).to_socket_addrs() {
                            Ok(found) => found.map(|a| a.ip()).collect(),
                            Err(err) => {
                                return Err(refused(format!(
                                    "{field}: {raw:?}: {host} couldn't be looked up from \
                                     {machine} ({err}), so it can't be checked; try again when \
                                     it resolves"
                                )));
                            }
                        }
                    }
                    None => return Err(refused(format!("{field}: {raw:?} names no host"))),
                };
                if addresses.is_empty() || addresses.iter().any(|ip| local_only_address(*ip)) {
                    return Err(refused(format!(
                        "{field}: {raw:?} reaches {machine}'s own services (loopback or \
                         link-local); from another computer a browser source opens pages on the \
                         internet or the local network"
                    )));
                }
                Ok(())
            }
            _ => Err(refused(format!(
                "{field}: {raw:?}: from another computer a browser source opens http(s) pages, \
                 or files in the folders other computers can use"
            ))),
        }
    }

    /// A setup a network caller brings in (`new`, replacing `old`, by a
    /// change, an import or a library switch): no developer settings
    /// changed (a hard boundary), and every file, browser URL, recordings
    /// folder and hotkey path it adds inside the roots (a guard rail).
    /// Paths `old` already has pass (the user put them there). Local
    /// callers pass.
    pub(crate) fn check_setup(&self, new: &Setup, old: Option<&Setup>) -> Result<(), RpcError> {
        if callers::current().is_local() {
            return Ok(());
        }
        let default_developer = isocast_protocol::developer::DeveloperSettings::default();
        if new.developer != *old.map_or(&default_developer, |old| &old.developer) {
            return Err(refused(format!(
                "developer: developer mode is changed on {} itself (Settings › Developer, or \
                 `isocast debug`)",
                callers::machine_name()
            )));
        }
        let roots = self.roots();
        let had: Vec<String> = old
            .map(|old| old.file_paths().into_iter().map(|(_, p)| p).collect())
            .unwrap_or_default();
        for (field, path) in new.file_paths() {
            if !had.contains(&path) {
                self.check_path_in(&roots, &field, &path, Access::Read)?;
            }
        }
        let had_urls: Vec<String> = old
            .map(|old| old.browser_urls().into_iter().map(|(_, u)| u).collect())
            .unwrap_or_default();
        for (field, url) in new.browser_urls() {
            if !had_urls.contains(&url) {
                self.check_browser_url(&roots, &field, &url)?;
            }
        }
        if old.is_none_or(|old| old.recording.folder != new.recording.folder) {
            self.check_path_in(
                &roots,
                "recording.folder",
                &new.recording.folder,
                Access::Write,
            )?;
        }
        let had_hotkeys = old
            .map(|old| hotkey_paths(&old.hotkeys))
            .unwrap_or_default();
        for (field, action, path) in hotkey_paths(&new.hotkeys) {
            if !had_hotkeys
                .iter()
                .any(|(_, a, p)| *a == action && *p == path)
            {
                self.check_path_in(&roots, &field, &path, Access::Write)?;
            }
        }
        Ok(())
    }

    /// The paths a network caller's request names, checked before it runs
    /// and replaced by their resolved form (so the consumer opens what was
    /// checked); defaults the engine would pick are filled in and checked
    /// too. Setups given with a request are checked like a change of the
    /// live setup. Local callers pass, untouched.
    pub(crate) fn check_params(&self, method: &mut Method) -> Result<(), RpcError> {
        if callers::current().is_local() {
            return Ok(());
        }
        let roots = self.roots();
        let resolve_buf = |field: &str, path: &mut PathBuf, access: Access| {
            *path =
                self.resolve_in(&roots, field, &expand_home(&path.to_string_lossy()), access)?;
            Ok::<(), RpcError>(())
        };
        let resolve_opt = |field: &str, path: &mut Option<PathBuf>, access: Access| match path {
            Some(path) => resolve_buf(field, path, access),
            None => Ok(()),
        };
        let resolve_text = |field: &str, path: &mut String| {
            *path = self
                .resolve_in(&roots, field, &expand_home(path), Access::Read)?
                .to_string_lossy()
                .into_owned();
            Ok::<(), RpcError>(())
        };
        // OBS's own settings folder holds collections and profiles named
        // without a path: that's the engine's choice, not the caller's.
        let path_like = |name: &str, ext: &str| {
            name.contains('/')
                || name.contains('\\')
                || name.starts_with('~')
                || name.to_ascii_lowercase().ends_with(ext)
        };
        let transition_file =
            |field: &str, transition: &Option<isocast_protocol::Transition>| match transition
                .as_ref()
                .and_then(|t| t.file())
            {
                Some(file) => self.check_path_in(&roots, field, file, Access::Read),
                None => Ok(()),
            };
        let live = || self.state.read().setup.clone();
        match method {
            Method::Start(params) => {
                resolve_opt("params.folder", &mut params.folder, Access::Write)
            }
            Method::TakeExportNle(params) => {
                resolve_buf("params.take", &mut params.take, Access::Read)?;
                resolve_opt("params.folder", &mut params.folder, Access::Write)?;
                resolve_opt("params.path", &mut params.path, Access::Write)
            }
            Method::BugReport(params) if !params.preview && params.preview_id.is_none() => {
                // Where the report would go by default is checked too.
                let out = params
                    .out
                    .get_or_insert_with(isocast_diagnostics::bundle::default_out);
                resolve_buf("params.out", out, Access::Write)
            }
            Method::BugReport(params) => resolve_opt("params.out", &mut params.out, Access::Write),
            Method::FilesList(params) => resolve_buf("params.path", &mut params.path, Access::Read),
            Method::Doctor(params) => {
                resolve_opt("params.folder", &mut params.folder, Access::Write)?;
                match &params.setup {
                    Some(setup) => self.check_setup(setup, live().as_ref()),
                    None => Ok(()),
                }
            }
            Method::Tune(params) => {
                resolve_opt("params.folder", &mut params.folder, Access::Write)?;
                match &params.setup {
                    Some(setup) => self.check_setup(setup, live().as_ref()),
                    None => Ok(()),
                }
            }
            Method::Optimize(params) => {
                resolve_opt("params.folder", &mut params.folder, Access::Write)
            }
            Method::ImportObs(params) => {
                if path_like(&params.collection, ".json") {
                    resolve_text("params.collection", &mut params.collection)?;
                }
                match &mut params.profile {
                    Some(profile) if path_like(profile, ".ini") => {
                        resolve_text("params.profile", profile)
                    }
                    _ => Ok(()),
                }
            }
            Method::LibraryImportObs(params) => {
                if let Some(collection) = &mut params.collection
                    && path_like(collection, ".json")
                {
                    resolve_text("params.collection", collection)?;
                }
                match &mut params.profile {
                    Some(profile) if path_like(profile, ".ini") => {
                        resolve_text("params.profile", profile)
                    }
                    _ => Ok(()),
                }
            }
            Method::ObsDiscover(Some(params)) => match &mut params.folder {
                Some(folder) => resolve_text("params.folder", folder),
                None => Ok(()),
            },
            // A luma wipe's picture or a stinger's video given with the
            // request.
            Method::Take { transition, .. } | Method::SwitchScene { transition, .. } => {
                transition_file("params.transition", transition)
            }
            _ => Ok(()),
        }
    }
}

/// The paths hotkey bindings write to ([`PATH_ACTIONS`]): field, action
/// and path.
fn hotkey_paths(config: &isocast_protocol::hotkeys::Config) -> Vec<(String, String, String)> {
    let Some(bindings) = config.value.as_ref().and_then(serde_json::Value::as_object) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (id, binding) in bindings {
        let Some(action) = binding.get("action").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(&(_, arg, _)) = PATH_ACTIONS.iter().find(|(a, ..)| *a == action) else {
            continue;
        };
        if let Some(path) = binding
            .get("args")
            .and_then(|args| args.get(arg))
            .and_then(serde_json::Value::as_str)
        {
            out.push((
                format!(
                    "hotkeys.{}.args.{arg}",
                    isocast_protocol::hotkeys::bounded(id, 64)
                ),
                action.to_string(),
                path.to_string(),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_addresses_are_recognised() {
        for ip in [
            "127.0.0.1",
            "127.9.9.9",
            "0.0.0.0",
            "169.254.1.1",
            "::1",
            "::",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(local_only_address(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "10.0.10.26",
            "192.168.1.5",
            "8.8.8.8",
            "2001:db8::1",
            "fd00::1",
        ] {
            assert!(!local_only_address(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn urls_are_read_as_browsers_read_them() {
        // Octal and backslash tricks name loopback to a browser.
        let host = |raw: &str| url::Url::parse(raw).unwrap().host().map(|h| h.to_owned());
        assert_eq!(
            host("http://0177.0.0.1/"),
            Some(url::Host::Ipv4([127, 0, 0, 1].into()))
        );
        assert_eq!(
            host("http://127.0.0.1\\@8.8.8.8/"),
            Some(url::Host::Ipv4([127, 0, 0, 1].into()))
        );
    }

    #[test]
    fn paths_resolve_through_symlinks() {
        assert!(resolve_parts(Path::new("relative/x")).is_err());
        assert!(resolve_parts(Path::new("/tmp/../etc")).is_err());
        let tmp = std::env::temp_dir().join(format!("isocast-roots-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let link = tmp.join("link");
        let _ = std::fs::remove_file(&link);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc", &link).unwrap();
            let resolved = resolve_parts(&link.join("new/file")).unwrap().0;
            assert!(resolved.starts_with(std::fs::canonicalize("/etc").unwrap()));
            let dangling = tmp.join("dangling");
            let _ = std::fs::remove_file(&dangling);
            std::os::unix::fs::symlink("/nonexistent-isocast", &dangling).unwrap();
            assert!(resolve_parts(&dangling.join("x")).is_err());
        }
        // Existing (canonical) parts compare exactly, missing ones by the
        // volume's rule, ASCII only.
        assert!(!within(
            (Path::new("/A/b/c"), 3),
            (Path::new("/a/B"), 3),
            true
        ));
        assert!(within(
            (Path::new("/A/b/c"), 1),
            (Path::new("/A/B"), 2),
            true
        ));
        assert!(!within(
            (Path::new("/A/b/c"), 1),
            (Path::new("/A/B"), 2),
            false
        ));
        assert!(!within(
            (Path::new("/A/É/c"), 1),
            (Path::new("/A/é"), 1),
            true
        ));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
