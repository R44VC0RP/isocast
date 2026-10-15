//! Embeds the web interface (`ui/`) in the binary: a table of every file's
//! path and bytes, served by the app's `isocast://` scheme handler.

use std::path::{Path, PathBuf};

fn main() {
    let ui = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("ui");
    println!("cargo:rerun-if-changed={}", ui.display());
    let mut files = Vec::new();
    collect(&ui, &mut files);
    files.sort();
    let mut table = String::from("pub static ASSETS: &[(&str, &[u8])] = &[\n");
    for file in &files {
        let path = file
            .strip_prefix(&ui)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        table.push_str(&format!(
            "    ({path:?}, include_bytes!({:?})),\n",
            file.display().to_string()
        ));
    }
    table.push_str("];\n");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("assets.rs");
    std::fs::write(out, table).unwrap();
}

/// Every file under `folder`, skipping hidden ones (like `.DS_Store`).
fn collect(folder: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if path.is_dir() {
            collect(&path, files);
        } else {
            files.push(path);
        }
    }
}
