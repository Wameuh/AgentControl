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
        println!("cargo:warning=client/dist missing — web UI not embedded");
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

    // Skills agents embarqués (item #264) : `skills/` = source de
    // vérité versionnée avec le binaire ; `.devin/skills/` est fusionné
    // en compat (un skill y ajouté — ex. en vol sur une autre PR — est
    // provisionné aussi). Sur conflit de chemin, `skills/` prime :
    // il est inséré en dernier dans la map. Le runtime les synchronise
    // dans les répertoires scannés par chaque provider (skills.rs) au
    // spawn et au boot.
    // Piège : `.devin/skills/` est AUSSI une sortie provisionnée — une
    // session devin dont le cwd est ce checkout y fait écrire les
    // skills embarqués + un manifeste `.agentcontrol-skills.json`.
    // Relire ce répertoire comme source créerait une boucle : un skill
    // retiré de `skills/` survivrait via sa copie provisionnée.
    // Les skills listés par le manifeste (managés = pas des sources)
    // sont donc exclus de l'embed.
    let mut skills: std::collections::BTreeMap<String, Vec<u8>> = std::collections::BTreeMap::new();
    for rel in [".devin/skills", "skills"] {
        let dir = crate_dir.join("../..").join(rel);
        if dir.is_dir() {
            println!("cargo:rerun-if-changed={}", dir.display());
            let root = dir.canonicalize().unwrap();
            // Noms de skills gérés par le provisionnement (clés de
            // premier niveau de l'objet "skills" du manifeste).
            let managed: std::collections::BTreeSet<String> = if rel == ".devin/skills" {
                let mf = root.join(".agentcontrol-skills.json");
                println!("cargo:rerun-if-changed={}", mf.display());
                fs::read_to_string(&mf)
                    .ok()
                    .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                    .and_then(|v| {
                        v.get("skills")?.as_object().map(|o| {
                            o.keys()
                                .filter_map(|k| k.split('/').next().map(str::to_owned))
                                .collect()
                        })
                    })
                    .unwrap_or_default()
            } else {
                Default::default()
            };
            let mut entries = Vec::new();
            walk_all(&root, &root, &mut entries);
            for (p, data) in entries {
                let skill = p.split('/').next().unwrap_or("");
                if !managed.contains(skill) {
                    skills.insert(p, data);
                }
            }
        }
    }
    let mut blob = Vec::new();
    blob.extend_from_slice(&(skills.len() as u32).to_le_bytes());
    for (p, data) in &skills {
        blob.extend_from_slice(&(p.len() as u16).to_le_bytes());
        blob.extend_from_slice(p.as_bytes());
        blob.extend_from_slice(&(data.len() as u32).to_le_bytes());
        blob.extend_from_slice(data);
    }
    let out = Path::new(&env::var("OUT_DIR").unwrap()).join("agent-skills.bin");
    fs::write(&out, &blob).unwrap();
}

/// Tous les fichiers (contrairement à `walk` qui filtre les
/// sourcemaps) — un skill peut embarquer des scripts sans extension.
fn walk_all(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk_all(root, &p, out);
        } else if p
            .file_name()
            .map(|n| n == ".agentcontrol-skills.json")
            .unwrap_or(false)
        {
            // Manifeste de sync écrit par le runtime dans un workspace
            // provisionné — jamais une source à embarquer.
        } else if let Ok(data) = fs::read(&p) {
            let rel = p
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, data));
        }
    }
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
