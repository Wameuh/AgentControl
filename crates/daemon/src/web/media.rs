//! `/media` et `/download` : accès HTTP natif aux fichiers workspace.
//!
//! `/media` remplace le scheme `acvideo://` du client Tauri : le
//! navigateur sait demander des `Range` (lecture vidéo/audio) et
//! streamer les grosses pièces sans passer par l'encode base64 du RPC.
//! `/download` rend les fichiers/dossiers en téléchargement navigateur
//! (`Content-Disposition: attachment`) — dossiers zippés via la même
//! archive `blobs/dl-*.zip` que `download_path`.
//!
//! Les chemins restent bornés au workspace de la session par
//! `resolve_in_cwd` : jamais de fichier hors du cwd session.

use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::http::{self, Head};
use crate::server::files::{download_archive, resolve_in_cwd};
use crate::server::Daemon;
use std::sync::Arc;

/// Tranche de streaming vers la socket — gros médias sans bufferiser.
const STREAM_CHUNK: usize = 256 * 1024;

/// `GET|HEAD /media?session=<id>&path=<p>` — sert un FICHIER du
/// workspace avec support `Range: bytes=a-b` (206) pour la vidéo.
/// Un dossier = 400 (l'explorateur n'appelle /media que sur des
/// fichiers) ; le Content-Type vient de l'extension.
pub async fn serve_media(
    daemon: &Arc<Daemon>,
    stream: &mut TcpStream,
    head: &Head,
    query: &[(String, String)],
) -> Result<()> {
    let Some(session) = http::qparam(query, "session") else {
        return http::respond_err(stream, 400, "session manquante").await;
    };
    let Some(path) = http::qparam(query, "path") else {
        return http::respond_err(stream, 400, "path manquant").await;
    };
    let p = match resolve_ws_path(daemon, session, path).await {
        Ok(p) => p,
        Err(e) => return http::respond_err(stream, 403, &format!("{e:#}")).await,
    };
    let meta = match std::fs::metadata(&p) {
        Ok(m) if m.is_file() => m,
        Ok(_) => return http::respond_err(stream, 400, "pas un fichier").await,
        Err(_) => return http::respond_err(stream, 404, "introuvable").await,
    };
    let total = meta.len();
    // Range: bytes=start-end | start- | -count — une seule plage, comme
    // les navigateurs en émettent pour <video>/<audio>.
    let range = match head.header("range").map(|r| parse_range(r, total)) {
        Some(Ok(r)) => r,
        Some(Err(())) => {
            return http::respond_err(stream, 416, "plage invalide").await;
        }
        None => None,
    };
    send_file(
        stream,
        head.method == "HEAD",
        &p,
        range,
        total,
        &[
            (
                "Content-Type".into(),
                mime_for(&p).into(),
            ),
            ("Accept-Ranges".into(), "bytes".into()),
            // Fichiers workspace rendus sous notre origine authentifiée :
            // un .svg/.html navigué directement exécuterait ses scripts
            // ici → sandbox coupe tout script/plugin, les balises
            // <img>/<video> ne sont pas affectées.
            (
                "Content-Security-Policy".into(),
                "sandbox".into(),
            ),
        ],
    )
    .await
}

/// `GET /download?session=<id>&path=<p>` — téléchargement navigateur :
/// `attachment` + nom de fichier ; un dossier part en `dl-*.zip`
/// (archive construite une fois sous blobs/, LRU-purgée ensuite).
pub async fn serve_download(
    daemon: &Arc<Daemon>,
    stream: &mut TcpStream,
    head: &Head,
    query: &[(String, String)],
) -> Result<()> {
    let Some(session) = http::qparam(query, "session") else {
        return http::respond_err(stream, 400, "session manquante").await;
    };
    let Some(path) = http::qparam(query, "path") else {
        return http::respond_err(stream, 400, "path manquant").await;
    };
    let s = match daemon.get_session(session).await {
        Ok(s) => s,
        Err(e) => return http::respond_err(stream, 404, &format!("{e:#}")).await,
    };
    let p = match resolve_in_cwd(&s, path) {
        Ok(p) => p,
        Err(e) => return http::respond_err(stream, 403, &format!("{e:#}")).await,
    };
    let meta = match std::fs::metadata(&p) {
        Ok(m) => m,
        Err(_) => return http::respond_err(stream, 404, "introuvable").await,
    };
    let name = p
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let (file, fname, size) = if meta.is_dir() {
        let (zp, _zn) = match download_archive(&s, &p).await {
            Ok(v) => v,
            Err(e) => {
                return http::respond_err(stream, 500, &format!("zip: {e:#}")).await
            }
        };
        let size = std::fs::metadata(&zp).map(|m| m.len()).unwrap_or(0);
        // Le nom servi reprend celui du dossier, pas le hash dl-*.
        (zp, format!("{name}.zip"), size)
    } else {
        (p, name, meta.len())
    };
    send_file(
        stream,
        head.method == "HEAD",
        &file,
        None,
        size,
        &[
            ("Content-Type".into(), "application/octet-stream".into()),
            (
                "Content-Disposition".into(),
                format!("attachment; filename=\"{}\"", sanitize_filename(&fname)),
            ),
        ],
    )
    .await
}

/// Session + chemin résolus bornés workspace (factorisé /media).
async fn resolve_ws_path(daemon: &Arc<Daemon>, session: &str, path: &str) -> Result<PathBuf> {
    let s = daemon.get_session(session).await?;
    resolve_in_cwd(&s, path)
}

/// `Range` parsé → (début, fin inclus). `None` = pas de range.
/// `Err(())` = syntaxe/étendue invalide → 416.
fn parse_range(spec: &str, total: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(spec) = spec.strip_prefix("bytes=") else {
        return Ok(None); // unité étrangère → ignorée (jamais d'erreur)
    };
    let spec = spec.trim();
    if spec.contains(',') {
        return Err(()); // multi-plages : pas implémenté
    }
    let (a, b) = spec.split_once('-').ok_or(())?;
    let (start, end) = if a.is_empty() {
        // -N : les N derniers octets.
        let n: u64 = b.trim().parse().map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        (total.saturating_sub(n), total.saturating_sub(1))
    } else {
        let s: u64 = a.trim().parse().map_err(|_| ())?;
        let e = if b.trim().is_empty() {
            total.saturating_sub(1)
        } else {
            b.trim().parse::<u64>().map_err(|_| ())?
        };
        (s, e.min(total.saturating_sub(1)))
    };
    if total == 0 || start > end || start >= total {
        return Err(());
    }
    Ok(Some((start, end)))
}

/// Écrit la réponse fichier : 200 plein ou 206 borné, puis stream
/// depuis l'offset (HEAD = têtes seules). `extra` = headers métier.
async fn send_file(
    stream: &mut TcpStream,
    head_only: bool,
    path: &std::path::Path,
    range: Option<(u64, u64)>,
    total: u64,
    extra: &[(String, String)],
) -> Result<()> {
    let (code, start, len) = match range {
        Some((s, e)) => (206, s, e - s + 1),
        None => (200, 0, total),
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {len}\r\nConnection: close\r\n",
        code,
        http::status_text(code)
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some((s, e)) = range {
        head.push_str(&format!("Content-Range: bytes {s}-{e}/{total}\r\n"));
    }
    head.push_str("X-Content-Type-Options: nosniff\r\n\r\n");
    stream.write_all(head.as_bytes()).await?;
    if head_only || len == 0 {
        return Ok(());
    }
    let mut f = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("{}", path.display()))?;
    if start > 0 {
        f.seek(std::io::SeekFrom::Start(start)).await?;
    }
    let mut left = len;
    let mut buf = vec![0u8; STREAM_CHUNK.min(len as usize)];
    while left > 0 {
        let want = buf.len().min(left as usize);
        let n = f.read(&mut buf[..want]).await?;
        if n == 0 {
            break; // fichier tronqué entre la tête et le corps
        }
        stream.write_all(&buf[..n]).await?;
        left -= n as u64;
    }
    Ok(())
}

/// Le nom dans Content-Disposition doit être ASCII-safe : tout caractère
/// hors `[A-Za-z0-9._-]` → '_' (un navigateur n'a besoin que d'un nom
/// plausible ; l'UTF-8 `filename*=…` n'apporte rien au cas d'usage).
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// MIME minimal — le front n'a besoin que des types média/texte qu'il
/// sert depuis un workspace (le navigateur n'exécute rien de /media :
/// nosniff est posé et les types exécutables restent neutres).
fn mime_for(p: &std::path::Path) -> &'static str {
    match p
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase()
        .as_str()
    {
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}

