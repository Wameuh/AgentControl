//! Embarque `client/dist` dans le binaire démon : la web UI (AC#14)
//! reste servie quelle que soit l'installation — `ensure_daemon` et
//! `self_update` ne posent que le binaire, aucun arbre d'assets à
//! déployer ni à versionner séparément (la UI livrée = la version du
//! démon qui la sert).
//!
//! Blob OUT_DIR/web-assets.bin :
//!   u32 count, puis par entrée { u16 path_len, path utf8, u32 len, data }
//! dist absent (build sans frontend) → blob vide → le runtime renonce
//! à l'embed et cherche un dossier disque (--web-dir, root/web…).
//! Sourcemaps exclues : inutilisées en prod, lourdes à embarquer.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let crate_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let dist = crate_dir.join("../../client/dist");
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    if dist.is_dir() {
        // Le tracking recouvre tout le sous-arbre : npm run build puis
        // cargo build ré-embarque les assets frais (workflow dev).
        println!("cargo:rerun-if-changed={}", dist.display());
        let root = dist.canonicalize().unwrap();
        walk(&root, &root, &mut entries);
        entries.sort();
    } else {
        println!("cargo:warning=client/dist absent — web UI non embarquée");
    }
    let mut blob = Vec::new();
    blob.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (p, data) in &entries {
        blob.extend_from_slice(&(p.len() as u16).to_le_bytes());
        blob.extend_from_slice(p.as_bytes());
        blob.extend_from_slice(&(data.len() as u32).to_le_bytes());
        blob.extend_from_slice(data);
    }
    let out = Path::new(&env::var("OUT_DIR").unwrap()).join("web-assets.bin");
    fs::write(&out, &blob).unwrap();
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(root, &p, out);
        } else if p.extension().map(|x| x != "map").unwrap_or(false) {
            let rel = p
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if let Ok(data) = fs::read(&p) {
                out.push((rel, data));
            }
        }
    }
}
