//! Route `/plugin/<name>/<path>` — assets statiques d'une vue plugin
//! (option B : iframe sandboxée + bridge postMessage borné, cf.
//! docs/plugin-views-exploration.md §5 et le backlog #248).
//!
//! Auth : token de bridge **éphémère** émis par `PluginViewToken{name}`
//! (TTL ~5 min, scopé au composant) en `?token=` — jamais le
//! `web.token` global : un asset de plugin n'a pas à connaître le
//! token d'administration du démon, et un token de A n'ouvre pas la
//! vue de B. L'exposition dans l'URL est bornée par le TTL et le
//! listener loopback.
//!
//! Garde-fous : la racine de service est le dossier de vue déclaré
//! (`spec.view_dir` ou convention `components/<name>/web/`),
//! canonicalisé — traversal `..`/symlink d'URL impossible
//! (`safe_join`), pas de listing de dossier, taille d'asset bornée,
//! CSP stricte + nosniff sur chaque réponse.

use super::http;
use super::{mime_by_ext, safe_join};
use crate::server::Daemon;
use anyhow::Result;
use std::sync::Arc;
use tokio::net::TcpStream;

/// CSP de la vue plugin : `default-src 'self'` = le dossier servi ;
/// `'unsafe-inline'` toléré pour les scripts/styles inline du plugin
/// (durcissement par hash/nonce = v2) ; `connect-src 'none'` — le
/// plugin ne dialogue qu'en postMessage, jamais en fetch ; l'origine
/// est opaque (iframe `sandbox` sans `allow-same-origin`).
const PLUGIN_CSP: &str = "default-src 'self'; \
    script-src 'self' 'unsafe-inline'; \
    style-src 'self' 'unsafe-inline'; \
    img-src 'self' data:; \
    font-src 'self' data:; \
    connect-src 'none'; \
    worker-src 'none'; \
    base-uri 'none'; \
    form-action 'none'";

/// Taille max d'un asset de vue — les données passent par le bridge
/// (`component_query`), pas par des fichiers servis.
const ASSET_MAX: u64 = 8 * 1024 * 1024;

/// `GET|HEAD /plugin/<name>/<path>` — servi après le contrôle du token
/// de vue ; la route est matchée avant le fallback SPA (mod.rs).
pub async fn serve_plugin(
    daemon: &Arc<Daemon>,
    stream: &mut TcpStream,
    head: &http::Head,
    q: &[(String, String)],
) -> Result<()> {
    let path = head.path_query().0;
    let rest = path.strip_prefix("/plugin/").unwrap_or_default();
    let (name, rel) = rest.split_once('/').unwrap_or((rest, ""));
    // Nom de composant : même alphabet que `check_usable` — un segment
    // invalide ne peut désigner aucune spec.
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return http::respond_err(stream, 404, "introuvable").await;
    }
    let token = http::qparam(q, "token").unwrap_or_default();
    if !daemon.components.view_token_valid(name, token) {
        return http::respond_err(stream, 401, "token de vue invalide").await;
    }
    let Some(dir) = daemon.components.plugin_view_dir_of(name).await else {
        return http::respond_err(stream, 404, "pas de vue pour ce composant").await;
    };
    // "" ou "/" → index.html ; pas de listing de dossier.
    let mut rel = rel.to_string();
    if rel.is_empty() {
        rel = "index.html".into();
    } else if rel.ends_with('/') {
        rel.push_str("index.html");
    }
    let Some(file) = safe_join(&dir, &rel) else {
        return http::respond_err(stream, 404, "introuvable").await;
    };
    // La racine est canonicalisée mais pas la cible : un symlink posé
    // dans le dossier de vue ne doit pas servir hors de la racine.
    let file = match file.canonicalize() {
        Ok(f) if f.starts_with(&dir) => f,
        _ => return http::respond_err(stream, 404, "introuvable").await,
    };
    let meta = match tokio::fs::metadata(&file).await {
        Ok(m) if m.is_file() => m,
        _ => return http::respond_err(stream, 404, "introuvable").await,
    };
    if meta.len() > ASSET_MAX {
        return http::respond_err(stream, 413, "asset trop gros").await;
    }
    let body = tokio::fs::read(&file).await?;
    let fname = file.file_name().unwrap_or_default().to_string_lossy();
    let mime = mime_by_ext(&fname);
    let ctype = if mime == "text/html" {
        // Le charset compte pour les contenus accentués du plugin.
        "text/html; charset=utf-8".to_string()
    } else {
        mime.to_string()
    };
    let headers = vec![
        ("Content-Type".into(), ctype),
        ("X-Content-Type-Options".into(), "nosniff".into()),
        ("Content-Security-Policy".into(), PLUGIN_CSP.into()),
        // Le token vit dans l'URL : aucun cache ne doit le retenir.
        ("Cache-Control".into(), "no-store".into()),
    ];
    if head.method == "HEAD" {
        http::respond_head(stream, 200, &headers, body.len()).await
    } else {
        http::respond(stream, 200, &headers, &body).await
    }
}
