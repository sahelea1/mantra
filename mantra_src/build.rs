//! Embeds the web UI (`src/web/assets/**`) into the binary: generates `$OUT_DIR/web_assets.rs`
//! with one `include_bytes!` per file, so adding an asset needs no Rust change.

use std::path::{Path, PathBuf};

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        // editor droppings and dotfiles are never served
        if name.starts_with('.') || name.ends_with('~') || name.ends_with(".swp") || name.ends_with(".tmp") {
            continue;
        }
        if p.is_dir() {
            walk(&p, out);
        } else if p.is_file() {
            out.push(p);
        }
    }
}

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into())).join("src/web/assets");
    println!("cargo:rerun-if-changed=src/web/assets");
    let mut files = vec![];
    walk(&root, &mut files);
    let mut src = String::from("/// (path relative to src/web/assets, bytes)\npub static ASSETS: &[(&str, &[u8])] = &[\n");
    for f in &files {
        println!("cargo:rerun-if-changed={}", f.display());
        let rel = f.strip_prefix(&root).map(|r| r.to_string_lossy().replace('\\', "/")).unwrap_or_default();
        src.push_str(&format!("    ({rel:?}, include_bytes!({:?})),\n", f.display().to_string()));
    }
    src.push_str("];\n");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_else(|_| ".".into())).join("web_assets.rs");
    let _ = std::fs::write(out, src);
}
