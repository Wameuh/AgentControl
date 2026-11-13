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

// ---- Collage presse-papier : fichiers copiés depuis un gestionnaire --
// Linux/WebKitGTK n'expose pas de File object au paste d'un fichier
// copié — le clipboard porte des URI `file://` (text/uri-list,
// x-special/gnome-copied-files, parfois text/plain « copy\nfile://… »).
// Ces commandes ne servent QUE ce flux : bornées au scheme `file:` et
// aux fichiers réguliers existants — pas de lecture de chemins
// arbitraires fournis par d'autres callers.

/// Taille max d'une tranche lue par clipboard_file_read — le client
/// découpe déjà à 4 Mio ; la borne protège d'un appel déraisonnable.
const CLIP_READ_CAP: u64 = 8 * 1024 * 1024;

/// URI `file://` → chemin local. Refuse tout autre scheme.
fn clipboard_path(uri: &str) -> Result<PathBuf, String> {
    let u = url::Url::parse(uri).map_err(|e| format!("URI invalide: {e}"))?;
    if u.scheme() != "file" {
        return Err("URI non-file:// refusée".into());
    }
    u.to_file_path()
        .map_err(|_| "URI file:// invalide".to_string())
}

/// Métadonnées (nom, taille) du fichier visé par une URI clipboard —
/// alimente la pipeline d'upload existante (cap 256 Mio côté TS).
#[derive(serde::Serialize)]
pub(crate) struct ClipFileStat {
    name: String,
    size: u64,
}

#[tauri::command]
pub(crate) fn clipboard_file_stat(uri: String) -> Result<ClipFileStat, String> {
    let p = clipboard_path(&uri)?;
    let md = std::fs::metadata(&p).map_err(|e| e.to_string())?;
    if !md.is_file() {
        return Err("pas un fichier régulier".into());
    }
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "fichier".into());
    Ok(ClipFileStat {
        name,
        size: md.len(),
    })
}

/// Tranche [offset, offset+limit) du fichier clipboard en base64 —
/// même contrat que les tranches write_file côté upload.
#[tauri::command]
pub(crate) fn clipboard_file_read(uri: String, offset: u64, limit: u64) -> Result<String, String> {
    use base64::Engine;
    use std::io::{Read, Seek, SeekFrom};
    let p = clipboard_path(&uri)?;
    // is_file comme stat : une URI vers une FIFO bloquerait open() en
    // attente d'un writer — le check avant open évite le blocage.
    if !std::fs::metadata(&p).map(|m| m.is_file()).unwrap_or(false) {
        return Err("pas un fichier régulier".into());
    }
    let mut f = std::fs::File::open(&p).map_err(|e| e.to_string())?;
    f.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    f.take(limit.min(CLIP_READ_CAP))
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    Ok(base64::engine::general_purpose::STANDARD.encode(&buf))
}
