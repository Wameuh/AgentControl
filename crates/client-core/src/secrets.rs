//! Chiffrement au repos des secrets du client (clé ed25519 de l'app,
//! clés appairées, connections.json).
//!
//! Le core ne connaît pas le backend : l'app injecte un `SecretBox`
//! (`init_secret_box`) au démarrage — sur Android c'est le plugin
//! `ac-keystore` (clé AES/GCM non exportable dans AndroidKeyStore).
//! Sans box (desktop) tout reste en clair, comportement inchangé.
//!
//! Format sur disque : `ACENC1` || ciphertext. Un fichier sans le magic
//! est lu tel quel (rétrocompat) puis ré-écrit chiffré si la box est
//! active et que le chemin vit sous `base_dir()` — les chemins hors
//! base (clé ssh utilisateur pointée par config) ne sont jamais
//! ré-écrits.

use std::io;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context, Result};

use crate::{base_dir, log, Level};

/// Magic de tête des fichiers scellés — permet la lecture
/// rétrocompatible des fichiers en clair hérités.
const MAGIC: &[u8; 6] = b"ACENC1";

/// Backend de chiffrement injecté par l'app — `seal`/`open` font
/// l'aller-retour vers le keystore de plateforme.
pub trait SecretBox: Send + Sync + 'static {
    fn seal(&self, data: &[u8]) -> Result<Vec<u8>>;
    fn open(&self, data: &[u8]) -> Result<Vec<u8>>;
}

static SECRET_BOX: OnceLock<Box<dyn SecretBox>> = OnceLock::new();

/// Installe le backend de chiffrement (au démarrage, avant tout accès
/// fichier). Sans appel : lecture/écriture en clair — comportement
/// desktop inchangé.
pub fn init_secret_box(b: impl SecretBox) {
    let _ = SECRET_BOX.set(Box::new(b));
}

fn boxed() -> Option<&'static dyn SecretBox> {
    SECRET_BOX.get().map(|b| &**b)
}

/// Écrit un fichier secret : chiffré si la box est active, clair sinon.
/// Écriture atomique `<name>.part` + `rename` — un kill en cours
/// d'écriture ne laisse jamais un secret tronqué (le .part orphelin
/// sera écrasé à la prochaine écriture). Mode 0600 dès la création
/// (unix) — jamais de fenêtre en permissions d'umask. Échec de `seal`
/// = échec de l'écriture (fail closed : pas de repli silencieux en
/// clair).
pub(crate) fn write_secret_file(path: &Path, data: &[u8]) -> io::Result<()> {
    let bytes = match boxed() {
        Some(b) => {
            let ct = b.seal(data).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::Other,
                    format!("seal {}: {e:#}", path.display()),
                )
            })?;
            let mut v = Vec::with_capacity(MAGIC.len() + ct.len());
            v.extend_from_slice(MAGIC);
            v.extend_from_slice(&ct);
            v
        }
        None => data.to_vec(),
    };
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".part");
    let tmp = path.with_file_name(tmp_name);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    io::Write::write_all(&mut f, &bytes)?;
    drop(f);
    // Windows refuse le rename sur une cible existante — la suppression
    // préalable ouvre une micro-fenêtre, acceptable hors box (desktop).
    #[cfg(windows)]
    let _ = std::fs::remove_file(path);
    std::fs::rename(&tmp, path)
}

/// Lit un fichier secret : déchiffre si le magic est présent (box
/// requise — sans box le fichier est illisible), clair sinon avec
/// re-scellement opportuniste des fichiers gérés par l'app sous
/// `base_dir()`.
pub(crate) fn read_secret_file(path: &Path) -> Result<Vec<u8>> {
    let raw = std::fs::read(path).with_context(|| format!("lecture {}", path.display()))?;
    if raw.starts_with(MAGIC) {
        let b = boxed().context("fichier chiffré sans backend keystore")?;
        return b
            .open(&raw[MAGIC.len()..])
            .with_context(|| format!("déchiffrement {}", path.display()));
    }
    // Héritage en clair : migration silencieuse des fichiers de l'app.
    if boxed().is_some() && path.starts_with(base_dir()) {
        if let Err(e) = write_secret_file(path, &raw) {
            log(
                Level::Warn,
                format!("re-scellement {}: {e}", path.display()),
            );
        }
    }
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct XorBox;
    impl SecretBox for XorBox {
        fn seal(&self, data: &[u8]) -> Result<Vec<u8>> {
            Ok(data.iter().map(|b| b ^ 0xAA).collect())
        }
        fn open(&self, data: &[u8]) -> Result<Vec<u8>> {
            self.seal(data)
        }
    }

    #[test]
    fn seal_open_reseal_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ac-secrets-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        crate::init_paths(dir.clone());
        init_secret_box(XorBox);

        // Écriture scellée : magic présent, clair absent, relecture ok.
        let p = dir.join("k");
        write_secret_file(&p, b"secret").unwrap();
        let raw = std::fs::read(&p).unwrap();
        assert!(raw.starts_with(MAGIC));
        assert!(!raw.windows(6).any(|w| w == b"secret"));
        assert_eq!(read_secret_file(&p).unwrap(), b"secret");

        // Héritage en clair sous base_dir : lu puis ré-écrit scellé.
        let legacy = dir.join("legacy");
        std::fs::write(&legacy, b"plain").unwrap();
        assert_eq!(read_secret_file(&legacy).unwrap(), b"plain");
        assert!(std::fs::read(&legacy).unwrap().starts_with(MAGIC));

        // Hors base_dir : jamais ré-écrit.
        let ext = std::env::temp_dir().join(format!("ac-ext-{}", std::process::id()));
        std::fs::write(&ext, b"user").unwrap();
        assert_eq!(read_secret_file(&ext).unwrap(), b"user");
        assert_eq!(std::fs::read(&ext).unwrap(), b"user");
    }
}
