//! Handlers fichiers : lecture/listing/suppression bornés au cwd de la
//! session, download par tranches (dossiers zippés sous blobs/dl-*.zip),
//! lecture d'extraits de blobs journalisés.

use super::*;

use crate::journal;

impl Daemon {
    pub(super) async fn h_read_file(self: &Arc<Self>, session: String, path: String, binary: bool) -> Result<Value> {
                let s = self.get_session(&session).await?;
                let p = PathBuf::from(&path);
                let full = if p.is_absolute() {
                    p
                } else {
                    PathBuf::from(&s.meta_snapshot().cwd).join(p)
                };
                if binary {
                    // Aperçu image : ~16 Mio de JSON base64 max ; `data` omis
                    // si tronqué (le client n'affiche que « trop grosse »).
                    const CAP: u64 = 12 * 1024 * 1024;
                    let (data, size) = read_slice(&full, 0, CAP)
                        .with_context(|| format!("{}", full.display()))?;
                    let mut resp = json!({
                        "path": full.display().to_string(),
                        "size": size,
                        "truncated": size > CAP,
                    });
                    if size <= CAP {
                        resp["data"] = json!(b64(&data));
                    }
                    Ok(resp)
                } else {
                    const CAP: u64 = 512 * 1024;
                    let (data, size) = read_slice(&full, 0, CAP)
                        .with_context(|| format!("{}", full.display()))?;
                    Ok(json!({
                        "path": full.display().to_string(),
                        "content": String::from_utf8_lossy(&data),
                        "truncated": size > CAP,
                    }))
                }
    }
    pub(super) async fn h_list_dir(self: &Arc<Self>, session: String, path: String) -> Result<Value> {
                let s = self.get_session(&session).await?;
                let dir = resolve_in_cwd(&s, &path)?;
                if !dir.is_dir() {
                    anyhow::bail!("pas un dossier: {path}");
                }
                let mut ents: Vec<(bool, String, u64)> = Vec::new();
                let mut truncated = false;
                for e in std::fs::read_dir(&dir)? {
                    if ents.len() >= 5000 {
                        truncated = true;
                        break;
                    }
                    let e = e?;
                    let ft = e.file_type()?;
                    ents.push((
                        ft.is_dir(),
                        e.file_name().to_string_lossy().into_owned(),
                        e.metadata().map(|m| m.len()).unwrap_or(0),
                    ));
                }
                ents.sort_by(|a, b| {
                    b.0.cmp(&a.0)
                        .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
                });
                let entries: Vec<Value> = ents
                    .iter()
                    .map(|(d, n, sz)| json!({"name": n, "dir": d, "size": sz}))
                    .collect();
                Ok(json!({
                    "dir": dir.display().to_string(),
                    "entries": entries,
                    "truncated": truncated,
                }))
    }
    pub(super) async fn h_delete_path(self: &Arc<Self>, session: String, path: String) -> Result<Value> {
                let s = self.get_session(&session).await?;
                let p = resolve_in_cwd(&s, &path)?;
                if p == resolve_in_cwd(&s, "")? {
                    anyhow::bail!("racine du workspace");
                }
                if p.is_dir() {
                    std::fs::remove_dir_all(&p)?;
                } else {
                    std::fs::remove_file(&p)?;
                }
                Ok(json!({"deleted": p.display().to_string()}))
    }
    pub(super) async fn h_download_path(self: &Arc<Self>, session: String, path: String, offset: u64, limit: u64, archive: Option<String>) -> Result<Value> {
                let s = self.get_session(&session).await?;
                download_path(&s, &path, offset, limit, archive.as_deref()).await
    }
    pub(super) async fn h_get_blob(self: &Arc<Self>, session: String, blob: String, offset: u64, limit: u64) -> Result<Value> {
                let s = self.get_session(&session).await?;
                let (data, total) = journal::read_blob(&s.dir, &blob, offset, limit)?;
                Ok(json!({
                    "data": String::from_utf8_lossy(&data),
                    "total": total,
                    "offset": offset,
                }))
    }
}

/// Résout `path` (relatif au cwd de la session) en chemin canonique et
/// garantit qu'il reste dans le workspace — anti `..`, chemins absolus et
/// liens symboliques sortants.
fn resolve_in_cwd(s: &Session, path: &str) -> Result<PathBuf> {
    let cwd = std::fs::canonicalize(&s.meta_snapshot().cwd)
        .with_context(|| format!("cwd: {}", s.meta_snapshot().cwd))?;
    let p = std::fs::canonicalize(cwd.join(path))
        .with_context(|| format!("{}", cwd.join(path).display()))?;
    if !p.starts_with(&cwd) {
        anyhow::bail!("hors du workspace: {path}");
    }
    Ok(p)
}

/// Taille max d'une tranche download (le base64 enfle ~×1,34 sur le fil).
const DL_CHUNK: u64 = 4 * 1024 * 1024;

/// `limit` = 0 → métadonnées seules ; sinon tranche base64 à `offset`.
/// Dossier → zippé une fois dans une archive unique sous blobs/dl-*.zip
/// (offset 0 construit, `archive` renvoyée puis passée par l'appelant —
/// deux téléchargements concurrents du même dossier ne se marchent pas
/// dessus, et le LRU purge les archives abandonnées).
async fn download_path(
    s: &Session,
    path: &str,
    offset: u64,
    limit: u64,
    archive: Option<&str>,
) -> Result<Value> {
    let p = resolve_in_cwd(s, path)?;
    let meta = std::fs::metadata(&p).with_context(|| format!("{}", p.display()))?;
    let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
    if meta.is_file() {
        if limit == 0 {
            return Ok(json!({"name": name, "dir": false, "size": meta.len()}));
        }
        let (data, total) = read_slice(&p, offset, limit.min(DL_CHUNK))?;
        return Ok(json!({
            "name": name, "dir": false, "total": total,
            "offset": offset, "data": b64(&data),
        }));
    }
    if !meta.is_dir() {
        anyhow::bail!("type non téléchargeable: {}", p.display());
    }
    if limit == 0 {
        // Sonde métadonnées : ne pas zipper ici — la première tranche
        // (offset 0, limit>0) construira l'archive une seule fois.
        return Ok(json!({"name": format!("{name}.zip"), "dir": true, "size": null}));
    }
    let zip_name = if offset == 0 {
        let zip_name = new_zip_name(&p);
        let (src, dst) = (p.clone(), s.dir.join("blobs").join(&zip_name));
        tokio::task::spawn_blocking(move || {
            let r = zip_dir(&src, &dst);
            if r.is_err() {
                let _ = std::fs::remove_file(dst.with_extension("part"));
            }
            r
        })
        .await
        .context("zip task")??;
        zip_name
    } else {
        let a = archive.context("archive manquante — repartir à offset 0")?;
        if !a.starts_with("dl-")
            || !a.ends_with(".zip")
            || !a[3..a.len() - 4].chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            anyhow::bail!("archive invalide: {a}");
        }
        a.to_string()
    };
    let zip_path = s.dir.join("blobs").join(&zip_name);
    if !zip_path.is_file() {
        anyhow::bail!("archive expirée (purge) — relancez le téléchargement");
    }
    // Rafraîchit la mtime : le LRU trie par ancienneté et ne doit pas
    // purger une archive encore en cours de transfert.
    let _ = std::fs::File::open(&zip_path)
        .and_then(|f| f.set_modified(std::time::SystemTime::now()));
    let (data, total) = read_slice(&zip_path, offset, limit.min(DL_CHUNK))?;
    Ok(json!({
        "name": format!("{name}.zip"), "dir": true, "total": total,
        "offset": offset, "data": b64(&data), "archive": zip_name,
    }))
}

/// Lit [offset, offset+limit) d'un fichier. Retourne (données, taille totale).
fn read_slice(p: &std::path::Path, offset: u64, limit: u64) -> Result<(Vec<u8>, u64)> {
    use std::io::{Read, Seek, SeekFrom};
    let total = std::fs::metadata(p)?.len();
    let mut f = std::fs::File::open(p)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; limit as usize];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    Ok((buf, total))
}

fn b64(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// Nom d'archive unique par téléchargement : dl-<fnv64(path)>-<secs>-<seq>.zip
/// — unique même en téléchargements concurrents du même dossier.
fn new_zip_name(p: &std::path::Path) -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut h: u64 = 0xcbf29ce484222325;
    for b in p.as_os_str().as_encoded_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("dl-{h:016x}-{secs:x}-{seq}.zip")
}

/// Zippe récursivement `src` dans `dst` (entrée racine = nom du dossier,
/// permissions unix préservées). Écrit via .part puis renommage atomique.
/// Liens symboliques : suivis seulement s'ils restent dans `src` (dédoublonnés
/// par chemin canonique → pas de boucle), fichiers volatils ignorés.
fn zip_dir(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    let parent = src.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    std::fs::create_dir_all(dst.parent().unwrap())?;
    let tmp = dst.with_extension("part");
    let mut zw = zip::ZipWriter::new(std::fs::File::create(&tmp)?);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let mut seen = std::collections::HashSet::from([src.to_path_buf()]);
    let mut stack = vec![src.to_path_buf()];
    while let Some(dir) = stack.pop() {
        #[cfg(unix)]
        let dir_opts = {
            use std::os::unix::fs::PermissionsExt;
            match std::fs::metadata(&dir) {
                Ok(m) => opts.unix_permissions(m.permissions().mode() & 0o777),
                Err(_) => opts,
            }
        };
        #[cfg(not(unix))]
        let dir_opts = opts;
        let rel_dir = dir.strip_prefix(&parent).unwrap_or(&dir).to_string_lossy();
        zw.add_directory(format!("{}/", rel_dir.replace('\\', "/")), dir_opts)?;
        let rd = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(e) => {
                warn!("zip: skip {}: {e}", dir.display());
                continue;
            }
        };
        for e in rd.flatten() {
            let path = e.path();
            // Cible effective : canonise les liens, borne au dossier racine
            // (file_type en échec → skip, jamais de suivi implicite).
            let ft = match e.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            let real = if ft.is_symlink() {
                match std::fs::canonicalize(&path) {
                    Ok(c) if c.starts_with(src) => c,
                    _ => continue,
                }
            } else {
                path.clone()
            };
            let meta = match std::fs::metadata(&real) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let rel = path
                .strip_prefix(&parent)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            #[cfg(unix)]
            let opts = {
                use std::os::unix::fs::PermissionsExt;
                opts.unix_permissions(meta.permissions().mode() & 0o777)
            };
            if meta.is_dir() {
                if seen.insert(real) {
                    stack.push(path);
                }
            } else if meta.is_file() {
                if let Ok(mut f) = std::fs::File::open(&real) {
                    match zw.start_file(&rel, opts) {
                        Ok(()) => {
                            if let Err(e) = std::io::copy(&mut f, &mut zw) {
                                warn!("zip: {rel}: {e}");
                                let _ = zw.abort_file();
                            }
                        }
                        Err(e) => warn!("zip: {rel}: {e}"),
                    }
                }
            }
        }
    }
    zw.finish()?;
    std::fs::rename(&tmp, dst)?;
    Ok(())
}

