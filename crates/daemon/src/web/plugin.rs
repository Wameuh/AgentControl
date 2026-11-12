//! Route `/plugin/<name>/<path>` — assets statiques d'une vue plugin
//! (option B : iframe sandboxée + bridge postMessage borné — backlog
//! #248).
//!
//! Auth : token de bridge **éphémère** émis par `PluginViewToken{name}`
//! (TTL ~5 min, scopé au composant) en `?token=` sur les **documents
//! HTML** — jamais le `web.token` global : un asset de plugin
//! n'a pas à connaître le token d'administration du démon, et un token
//! de A n'ouvre pas la vue de B. L'exposition dans l'URL est bornée
//! par le TTL et le listener loopback.
//!
//! Les sous-ressources (js/css/img) sont servies **sans token** : une
//! iframe sandboxée en origine opaque ne peut pas porter le `?token=`
//! de la page parente sur ses requêtes relatives, et un cookie de
//! session ne traverserait pas non plus une origine opaque en mode
//! desktop (cross-site). Même niveau d'exposition que la SPA — des
//! fichiers statiques choisis par l'auteur du composant ; les données
//! passent par le bridge `component_query` (RPC authentifiée), pas par
//! les assets. Le token reste exigé sur le document et conditionne le
//! bootstrap `ready` du bridge.
//!
//! Garde-fous : la racine de service est le dossier de vue déclaré
//! (`spec.view_dir` ou convention `components/<name>/web/`),
//! canonicalisé — traversal `..`/symlink d'URL impossible
//! (`safe_join` + re-canonicalisation), pas de listing de dossier,
//! taille d'asset bornée, CSP stricte + nosniff sur chaque réponse.

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
///
/// `'self'` seul ne suffit pas : dans une iframe sandboxée sans
/// `allow-same-origin`, le document a une origine opaque et `'self'`
/// peut ne matcher aucune URL réseau selon le moteur — l'origine
/// concrète de la requête (scheme + Host) est donc ajoutée
/// explicitement à chaque directive de source.
fn plugin_csp(origin: &str) -> String {
    format!(
        "default-src 'self' {o}; \
         script-src 'self' 'unsafe-inline' {o}; \
         style-src 'self' 'unsafe-inline' {o}; \
         img-src 'self' data: {o}; \
         font-src 'self' data: {o}; \
         connect-src 'none'; \
         worker-src 'none'; \
         base-uri 'none'; \
         form-action 'none'",
        o = origin
    )
}

/// Taille max d'un asset de vue — les données passent par le bridge
/// (`component_query`), pas par des fichiers servis.
const ASSET_MAX: u64 = 8 * 1024 * 1024;

/// `GET|HEAD /plugin/<name>/<path>` — les documents HTML exigent le
/// token de vue ; les sous-ressources sont libres (voir l'entête).
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
    // "" ou "/" → index.html ; pas de listing de dossier.
    let mut rel = rel.to_string();
    if rel.is_empty() {
        rel = "index.html".into();
    } else if rel.ends_with('/') {
        rel.push_str("index.html");
    }
    let Some(dir) = daemon.components.plugin_view_dir_of(name).await else {
        return http::respond_err(stream, 404, "pas de vue pour ce composant").await;
    };
    let Some(file) = safe_join(&dir, &rel) else {
        return http::respond_err(stream, 404, "introuvable").await;
    };
    // La racine est canonicalisée mais pas la cible : un symlink posé
    // dans le dossier de vue ne doit pas servir hors de la racine.
    let file = match file.canonicalize() {
        Ok(f) if f.starts_with(&dir) => f,
        _ => return http::respond_err(stream, 404, "introuvable").await,
    };
    // Tout document HTML exige le token de vue (le chemin direct
    // `/plugin/<name>/index.html` compris — pas seulement l'URL
    // canonique) : c'est lui qui démarre le bootstrap `ready` du
    // bridge. Les autres assets statiques, eux, sont au niveau
    // d'exposition de la SPA (publique loopback).
    let is_doc = file
        .file_name()
        .is_some_and(|n| n.to_string_lossy().to_lowercase().ends_with(".html"));
    if is_doc {
        let token = http::qparam(q, "token").unwrap_or_default();
        if !daemon.components.view_token_valid(name, token) {
            return http::respond_err(stream, 401, "token de vue invalide").await;
        }
    }
    let meta = match tokio::fs::metadata(&file).await {
        Ok(m) if m.is_file() => m,
        _ => return http::respond_err(stream, 404, "introuvable").await,
    };
    if meta.len() > ASSET_MAX {
        return http::respond_err(stream, 413, "asset trop gros").await;
    }
    // Origine concrète de la requête pour la CSP (cf. plugin_csp) —
    // Host absent (HTTP/1.0) → 'self' seul. IPv6 : ne bracket que le
    // littéral nu (`::1`) — `host:port` et `[::1]:port` passent tels
    // quels (un bracket sur host:port produirait une source invalide).
    let host = head.header("host").unwrap_or_default();
    let origin = if host.is_empty() {
        String::new()
    } else if host.parse::<std::net::Ipv6Addr>().is_ok() {
        format!(" http://[{host}]")
    } else {
        format!(" http://{host}")
    };
    let fname = file.file_name().unwrap_or_default().to_string_lossy();
    let mime = mime_by_ext(&fname);
    let ctype = if mime == "text/html" {
        // Le charset compte pour les contenus accentués du plugin.
        "text/html; charset=utf-8".to_string()
    } else {
        mime.to_string()
    };
    // CSP : le document garde la politique plugin + `sandbox
    // allow-scripts` (défense en profondeur — une navigation directe
    // avec un token valide ne doit pas produire de document non
    // sandboxé lisant l'origine du démon) ; les sous-ressources, servies
    // sans token sous l'origine authentifiée, prennent `sandbox` nu —
    // un .svg/.html navigué directement y serait sinon un document
    // scriptable dans l'origine du démon (vol du web.token via
    // sessionStorage — même motif que media.rs).
    let csp = if is_doc {
        format!("{}; sandbox allow-scripts", plugin_csp(&origin))
    } else {
        "sandbox".to_string()
    };
    let headers = vec![
        ("Content-Type".into(), ctype),
        ("X-Content-Type-Options".into(), "nosniff".into()),
        ("Content-Security-Policy".into(), csp),
        // Le token vit dans l'URL du document : aucun cache ne doit
        // le retenir.
        ("Cache-Control".into(), "no-store".into()),
    ];
    if head.method == "HEAD" {
        http::respond_head(stream, 200, &headers, meta.len() as usize).await
    } else {
        let body = tokio::fs::read(&file).await?;
        http::respond(stream, 200, &headers, &body).await
    }
}
