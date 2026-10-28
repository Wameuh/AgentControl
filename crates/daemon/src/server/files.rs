//! Handlers fichiers : lecture/listing/suppression bornés au cwd de la
//! session, download par tranches (dossiers zippés sous blobs/dl-*.zip),
//! lecture d'extraits de blobs journalisés.

use super::*;

use crate::journal;
use std::path::Path;

impl Daemon {
    pub(super) async fn h_read_file(self: &Arc<Self>, session: String, path: String, binary: bool, offset: u64, limit: Option<u64>) -> Result<Value> {
                let s = self.get_session(&session).await?;
                let p = PathBuf::from(&path);
                let full = if p.is_absolute() {
                    p
                } else {
                    PathBuf::from(&s.meta_snapshot().cwd).join(p)
                };
                if let Some(limit) = limit {
                    // Lecture bornée (streaming vidéo + chargement progressif
                    // de l'éditeur côté client) — tranche brute base64,
                    // plafonnée comme DL_CHUNK (~5,4 Mio JSON). `mtime_ms`
                    // alimente la garde anti-écrasement de write_file.
                    let (data, size) = read_slice(&full, offset, limit.min(DL_CHUNK))
                        .with_context(|| format!("{}", full.display()))?;
                    return Ok(json!({
                        "path": full.display().to_string(),
                        "offset": offset,
                        "size": size,
                        "data": b64(&data),
                        "mtime_ms": mtime_ms(&full),
                    }));
                }
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
    pub(super) async fn h_find_files(self: &Arc<Self>, session: String, path: String, query: String, limit: Option<u32>) -> Result<Value> {
                let s = self.get_session(&session).await?;
                let root = resolve_in_cwd(&s, &path)?;
                if !root.is_dir() {
                    anyhow::bail!("pas un dossier: {path}");
                }
                let q = query.to_lowercase();
                if q.is_empty() {
                    return Ok(json!({"matches": [], "truncated": false, "scanned": 0}));
                }
                let cwd = resolve_in_cwd(&s, "")?;
                let limit = limit.unwrap_or(200).clamp(1, 1000) as usize;
                // Walk bloquant (read_dir récursif) → spawn_blocking, borné.
                tokio::task::spawn_blocking(move || find_files(&cwd, &root, &q, limit))
                    .await
                    .context("find task")?
    }
    /// Éditeur du client : remplace [offset, offset+delete_len) par `data`
    /// (base64) — atomique (.part + rename), borné workspace, garde mtime
    /// optionnelle ("conflit: <mtime>" si le disque a changé).
    pub(super) async fn h_write_file(self: &Arc<Self>, session: String, path: String, offset: u64, delete_len: u64, data: String, if_mtime_ms: Option<u64>) -> Result<Value> {
                let s = self.get_session(&session).await?;
                let p = resolve_in_cwd_create(&s, &path)?;
                if p.is_dir() {
                    anyhow::bail!("cible est un dossier: {path}");
                }
                // data est une ligne JSON — ~12 Mio décodés max (le client
                // envoie le buffer édité complet en une tranche).
                const WRITE_CAP: usize = 16 * 1024 * 1024;
                if data.len() > WRITE_CAP {
                    anyhow::bail!("tranche trop grosse (> {} o)", WRITE_CAP);
                }
                use base64::Engine;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&data)
                    .context("data base64")?;
                let len = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                if let Some(want) = if_mtime_ms {
                    match mtime_ms(&p) {
                        Some(m) if m == want => {}
                        Some(m) => anyhow::bail!("conflit: {m}"),
                        None => anyhow::bail!("conflit: fichier supprimé"),
                    }
                }
                if offset > len {
                    anyhow::bail!("offset au-delà de la fin ({offset} > {len})");
                }
                let delete_len = delete_len.min(len - offset);
                let data_len = bytes.len() as u64;
                let p2 = p.clone();
                // La copie de queue peut brasser un gros reliquat — hors de
                // la boucle d'events tokio.
                tokio::task::spawn_blocking(move || splice_write(&p2, offset, delete_len, &bytes))
                    .await
                    .context("write task")??;
                Ok(json!({
                    "path": p.display().to_string(),
                    "size": len - delete_len + data_len,
                    "mtime_ms": mtime_ms(&p),
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
    /// Création exclusive d'un fichier vide ou d'un dossier (explorateur
    /// « nouveau ») — même borne workspace que write_file. La cible ne
    /// doit exister sous aucune forme : create_new/O_EXCL garantit
    /// l'exclusivité même en cas de course (write_file, lui, écrirait
    /// sans broncher dans un fichier apparu entre-temps).
    pub(super) async fn h_create_path(self: &Arc<Self>, session: String, path: String, dir: bool) -> Result<Value> {
                let s = self.get_session(&session).await?;
                let p = resolve_in_cwd_create(&s, &path)?;
                if p == resolve_in_cwd(&s, "")? {
                    anyhow::bail!("racine du workspace");
                }
                // resolve_in_cwd_create renvoie la cible canonique quand la
                // feuille existe (symlink suivi) → déjà là sous une forme ou
                // une autre = collision explicite plutôt qu'erreur système
                // (lien pendant : canonicalize échoue plus tôt — refus pareil).
                if std::fs::symlink_metadata(&p).is_ok() {
                    anyhow::bail!("existe déjà: {path}");
                }
                if dir {
                    std::fs::create_dir(&p)
                        .with_context(|| format!("{}", p.display()))?;
                } else {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&p)
                        .with_context(|| format!("{}", p.display()))?;
                }
                Ok(json!({"path": p.display().to_string(), "dir": dir}))
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

/// Plafond d'entrées parcourues par find_files — un workspace peut
/// contenir des centaines de milliers de fichiers (node_modules, .git) ;
/// au-delà on rend `truncated` plutôt que de scanner indéfiniment.
const FIND_MAX_SCANNED: usize = 100_000;

/// Parcours BFS de `root` : entrées dont le nom contient `q` (déjà
/// lowercasé). `path` renvoyé relatif au cwd (séparateurs `/`). Les liens
/// symboliques matchent par leur nom mais ne sont jamais descendus (pas
/// de boucle, pas de sortie du workspace — resolve_in_cwd a déjà borné
/// la racine). Ordre BFS = résultats peu profonds d'abord.
fn find_files(cwd: &Path, root: &Path, q: &str, limit: usize) -> Result<Value> {
    let mut matches: Vec<Value> = Vec::new();
    let mut queue = std::collections::VecDeque::from([root.to_path_buf()]);
    let mut scanned = 0usize;
    let mut truncated = false;
    'walk: while let Some(dir) = queue.pop_front() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(e) => {
                warn!("find: skip {}: {e}", dir.display());
                continue;
            }
        };
        for e in rd.flatten() {
            scanned += 1;
            if scanned > FIND_MAX_SCANNED || matches.len() >= limit {
                truncated = true;
                break 'walk;
            }
            let ft = match e.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            let name = e.file_name().to_string_lossy().into_owned();
            let p = e.path();
            if name.to_lowercase().contains(q) {
                let rel = p
                    .strip_prefix(cwd)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .replace('\\', "/");
                let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                matches.push(json!({"path": rel, "dir": ft.is_dir(), "size": size}));
            }
            if ft.is_dir() {
                queue.push_back(p);
            }
        }
    }
    Ok(json!({
        "matches": matches,
        "truncated": truncated,
        "scanned": scanned,
    }))
}

/// Variante pour l'écriture : la feuille peut être absente (création) —
/// le parent est canonicalisé et doit rester dans le workspace. Un lien
/// symbolique existant (même pendant) est résolu : la cible doit rester
/// dans le workspace sinon refus.
fn resolve_in_cwd_create(s: &Session, path: &str) -> Result<PathBuf> {
    let cwd = std::fs::canonicalize(&s.meta_snapshot().cwd)
        .with_context(|| format!("cwd: {}", s.meta_snapshot().cwd))?;
    let joined = cwd.join(path);
    // symlink_metadata (pas exists()) : voit aussi les liens pendants.
    if std::fs::symlink_metadata(&joined).is_ok() {
        let p = std::fs::canonicalize(&joined)
            .with_context(|| format!("{}", joined.display()))?;
        if !p.starts_with(&cwd) {
            anyhow::bail!("hors du workspace: {path}");
        }
        return Ok(p);
    }
    let parent = joined.parent().unwrap_or(cwd.as_path());
    let base = std::fs::canonicalize(parent)
        .with_context(|| format!("{}", parent.display()))?;
    if !base.starts_with(&cwd) {
        anyhow::bail!("hors du workspace: {path}");
    }
    let name = joined
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("nom de fichier invalide: {path}"))?;
    Ok(base.join(name))
}

/// mtime en ms depuis epoch (None si absent/illisible).
fn mtime_ms(p: &std::path::Path) -> Option<u64> {
    let m = std::fs::metadata(p).ok()?.modified().ok()?;
    Some(m.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as u64)
}

/// Remplace [offset, offset+delete_len) par `data` via un fichier .part
/// puis renommage atomique (le mode unix de l'original est préservé).
/// Le temporaire porte un nom unique `.<nom>.part-<pid>-<seq>` et est
/// ouvert en `create_new` (O_EXCL) : un `foo.part` symlink préexistant
/// ne peut pas faire écrire hors du workspace, et un vrai `foo.part`
/// utilisateur n'est jamais tronqué.
fn splice_write(p: &std::path::Path, offset: u64, delete_len: u64, data: &[u8]) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = p
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("nom de fichier invalide"))?
        .to_string_lossy();
    let tmp = p.with_file_name(format!(
        ".{name}.part-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));
    let perms = std::fs::metadata(p).ok().map(|m| m.permissions());
    let res = (|| -> Result<()> {
        // O_EXCL : toute entrée existante — symlink compris — refuse
        // l'ouverture (jamais de suivi de lien, jamais d'écrasement).
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        if let Ok(mut f) = std::fs::File::open(p) {
            // (&mut f) est lui-même Read — `take` consomme en bornant à
            // `offset` octets (by_ref serait ambigu : File est Read+Write).
            std::io::copy(&mut (&mut f).take(offset), &mut out)?;
            f.seek(SeekFrom::Start(offset + delete_len))?;
            out.write_all(data)?;
            std::io::copy(&mut f, &mut out)?;
        } else {
            out.write_all(data)?;
        }
        // Durabilité avant le rename : sans fsync le .part peut perdre
        // des données en cas de crash pile après le renommage.
        out.sync_all()?;
        Ok(())
    })();
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    #[cfg(unix)]
    if let Some(perms) = perms {
        let _ = std::fs::set_permissions(&tmp, perms);
    }
    std::fs::rename(&tmp, p)?;
    Ok(())
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

