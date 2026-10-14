//! Short-lived redacted snapshots. A file read never touches an arbitrary path.
use isocast_protocol::diagnostics::{ReportFile, ReportFileParams};
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
struct Preview {
    id: String,
    at: Instant,
    files: BTreeMap<String, String>,
}
static PREVIEWS: OnceLock<Mutex<Vec<Preview>>> = OnceLock::new();
pub fn save(files: BTreeMap<String, String>) -> Result<String, String> {
    if files.values().map(String::len).sum::<usize>() > 64 << 20 {
        return Err("preview exceeds 64 MiB; request fewer launches".into());
    }
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
    let id = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let mut cache = PREVIEWS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    cache.retain(|p| p.at.elapsed() < Duration::from_secs(300));
    while cache.len() >= 2 {
        cache.remove(0);
    }
    cache.push(Preview {
        id: id.clone(),
        at: Instant::now(),
        files,
    });
    Ok(id)
}
/// A preview's files, exactly as shown, to write and send.
pub fn contents(id: &str) -> Result<BTreeMap<String, String>, String> {
    let mut cache = PREVIEWS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let p = cache
        .iter_mut()
        .find(|p| p.id == id && p.at.elapsed() < Duration::from_secs(300))
        .ok_or("params.preview_id: expired or unknown; create a new bug_report preview")?;
    p.at = Instant::now();
    Ok(p.files.clone())
}

pub fn read(params: &ReportFileParams) -> Result<ReportFile, String> {
    let limit = params.limit.unwrap_or(4096);
    if !(1..=16384).contains(&limit) {
        return Err("params.limit: valid range 1..16384 bytes".into());
    }
    let mut cache = PREVIEWS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let p = cache
        .iter_mut()
        .find(|p| p.id == params.preview_id && p.at.elapsed() < Duration::from_secs(300))
        .ok_or("params.preview_id: expired or unknown; create a new bug_report preview")?;
    p.at = Instant::now();
    let text = p
        .files
        .get(&params.file)
        .ok_or("params.file: choose a name from the preview file index")?;
    if params.offset > text.len() || !text.is_char_boundary(params.offset) {
        return Err("params.offset: must be a UTF-8 byte boundary within this file".into());
    }
    let mut end = params.offset.saturating_add(limit).min(text.len());
    while end > params.offset && !text.is_char_boundary(end) {
        end -= 1;
    }
    if end == params.offset && end < text.len() {
        return Err("params.limit: too small for the next UTF-8 character".into());
    }
    Ok(ReportFile {
        file: params.file.clone(),
        content: text[params.offset..end].to_owned(),
        next_offset: (end < text.len()).then_some(end),
        total_bytes: text.len(),
    })
}
