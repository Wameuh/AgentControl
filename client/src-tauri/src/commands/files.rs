//! Fichiers locaux : écriture des tranches de téléchargement (base64)
//! reçues du démon vers le fichier choisi par l'utilisateur.

use anyhow::Result;
use std::path::PathBuf;

/// Écrit une tranche de téléchargement (base64) dans `path` — flux du
/// fichier workspace démon → fichier local choisi par l'utilisateur.
/// Retourne le nombre d'octets écrits.
#[tauri::command]
pub(crate) fn write_download(path: String, data: String, append: bool) -> Result<u64, String> {
    use base64::Engine;
    use std::io::Write;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| e.to_string())?;
    let p = PathBuf::from(&path);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut o = std::fs::OpenOptions::new();
    o.create(true).write(true);
    if append {
        o.append(true);
    } else {
        o.truncate(true);
    }
    let mut f = o.open(&p).map_err(|e| e.to_string())?;
    f.write_all(&bytes).map_err(|e| e.to_string())?;
    Ok(bytes.len() as u64)
}

