//! Serveur HTTP loopback du démon : sert la SPA `client/dist` et expose
//! le protocole NDJSON existant au navigateur (AC#14).
//!
//! Endpoints :
//! - `GET /*`              → fichiers statiques de la SPA (+ fallback
//!                          index.html — routing côté client)
//! - `GET /ping`           → 204 sans auth (sonde de tunnel SSH)
//! - `GET /auth?token=`    → 204 si le token est bon, 401 sinon — le
//!                          front valide la saisie utilisateur sans
//!                          ouvrir de WS
//! - `GET /ws?token=`      → WebSocket : pont vers `Daemon::serve_conn`
//!                          — le navigateur parle le protocole NDJSON
//!                          exact des sockets TCP/Unix (dispatch +
//!                          subscribe → events push + terminaux)
//! - `POST /rpc`           → une requête `Request` JSON → `Response`
//!                          JSON (compat one-shot, debug, clients non-WS)
//! - `GET /media`          → fichier workspace streamé, `Range` supporté
//!                          (remplace le scheme acvideo:// du Tauri)
//! - `GET /download`       → fichier/dossier (zip) en `attachment`
//!
//! Sécurité :
//! - bind loopback uniquement — l'accès distant passe par `ssh -L`,
//!   jamais par exposition réseau (vérifié au parse de config)
//! - token aléatoire généré au démarrage, stocké `<root>/web.token`
//!   mode 600 ; requis sur tout sauf /ping (query `?token=` ou
//!   header `Authorization: Bearer`)
//! - le WebSocket vérifie en plus que Origin (si envoyé) partage l'hôte
//!   du header Host — anti-CSWSH (un site tiers ne peut pas ouvrir de
//!   WS cross-origin vers le démon même en connaissant le token, et le
//!   token en query n'est de toute façon pas devinable)
//! - CSP stricte sur les pages HTML ; nosniff partout

mod http;
mod media;
mod ws;

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Component, PathBuf};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tracing::{info, warn};

use crate::server::Daemon;

/// Assets SPA embarqués à la compilation (build.rs → blob OUT_DIR) :
/// chemins '/'-séparés relatifs à dist → contenu.
static ASSETS_BLOB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/web-assets.bin"));

/// Source des assets SPA : embarqués (binaire autonome) ou dossier
/// disque (--web-dir / web_dir / root|exe|repo — dev & installs manuelles).
pub enum Assets {
    Embed(HashMap<String, &'static [u8]>),
    Dir(PathBuf),
}

/// CSP des pages HTML servies — adaptée de tauri.conf.json : 'self'
/// remplace https://tauri.localhost / ipc://, ws:/wss: couvre la
/// WebSocket même via un port forwardé, http(s) reste pour les images
/// liées dans les conversations (inlineImages) et les liens distants.
const CSP: &str = "default-src 'self'; \
    connect-src 'self' ws: wss: https://api.github.com; \
    img-src 'self' data: https: http:; \
    media-src 'self'; \
    font-src 'self' data:; \
    style-src 'self' 'unsafe-inline'; \
    script-src 'self'; \
    worker-src 'self' blob:";

/// Parse du blob embarqué → index chemin → contenu. `None` = build sans
/// frontend (dist absent) — le runtime tombera sur disque/placeholder.
pub fn embedded_assets() -> Option<HashMap<String, &'static [u8]>> {
    let b = ASSETS_BLOB;
    let n = u32::from_le_bytes(b.get(0..4)?.try_into().ok()?) as usize;
    if n == 0 {
        return None;
    }
    let mut m = HashMap::with_capacity(n);
    let mut i = 4usize;
    for _ in 0..n {
        let pl = u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?) as usize;
        i += 2;
        let path = std::str::from_utf8(b.get(i..i + pl)?).ok()?;
        i += pl;
        let dl = u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?) as usize;
        i += 4;
        m.insert(path.to_string(), b.get(i..i + dl)?);
        i += dl;
    }
    Some(m)
}

/// Contexte partagé du listener web.
pub struct WebCtx {
    /// Assets SPA : embed (prioritaire) ou dossier disque. None → placeholder.
    assets: Option<Assets>,
    /// Token d'accès (fichier web.token à la racine démon).
    token: String,
}

impl WebCtx {
    pub fn new(assets: Option<Assets>, token: String) -> Arc<Self> {
        Arc::new(Self { assets, token })
    }
}

/// Accept loop du listener HTTP — chaque connexion = une requête puis
/// close (ou une session WS pour /ws). Loopback only : le bind est
/// validé en amont (cfg::validate_web_addr).
pub async fn accept_http(daemon: Arc<Daemon>, listener: TcpListener, ctx: Arc<WebCtx>) {
    let local = listener.local_addr().map(|a| a.to_string()).unwrap_or_default();
    info!("web ui sur http://{local}/ (token dans web.token)");
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!("web accept: {e}");
                continue;
            }
        };
        let d = Arc::clone(&daemon);
        let c = Arc::clone(&ctx);
        tokio::spawn(async move {
            if let Err(e) = serve_http(d, stream, c).await {
                // "Connexion réinitialisée" & co : bruit usuel d'un
                // navigateur qui ferme un onglet — pas un incident.
                tracing::debug!("web conn {peer}: {e:#}");
            }
        });
    }
}

/// Une connexion HTTP : lit la tête, route, répond, ferme. /ws fait
/// escalader vers le pont WebSocket qui garde la socket jusqu'à close.
async fn serve_http(daemon: Arc<Daemon>, mut stream: TcpStream, ctx: Arc<WebCtx>) -> Result<()> {
    let head = http::read_head(&mut stream).await?;
    let (path, query) = head.path_query();
    let q = http::query_params(query);
    match (head.method.as_str(), path) {
        ("GET", "/ping") => {
            http::respond(&mut stream, 204, &[], &[]).await?;
        }
        ("GET", "/auth") => {
            if authed(&head, &q, &ctx) {
                http::respond(&mut stream, 204, &[], &[]).await?;
            } else {
                http::respond_err(&mut stream, 401, "token invalide").await?;
            }
        }
        ("GET", "/ws") => {
            if !authed(&head, &q, &ctx) {
                http::respond_err(&mut stream, 401, "token invalide").await?;
            } else if !origin_ok(&head) {
                http::respond_err(&mut stream, 403, "origine refusée").await?;
            } else {
                ws::upgrade_and_serve(daemon, stream, &head).await?;
            }
        }
        ("POST", "/rpc") => {
            if !authed(&head, &q, &ctx) {
                http::respond_err(&mut stream, 401, "token invalide").await?;
            } else if !origin_ok(&head) {
                http::respond_err(&mut stream, 403, "origine refusée").await?;
            } else {
                rpc(&daemon, &mut stream, &head).await?;
            }
        }
        ("GET" | "HEAD", "/media") => {
            if !authed(&head, &q, &ctx) {
                http::respond_err(&mut stream, 401, "token invalide").await?;
            } else {
                media::serve_media(&daemon, &mut stream, &head, &q).await?;
            }
        }
        ("GET" | "HEAD", "/download") => {
            if !authed(&head, &q, &ctx) {
                http::respond_err(&mut stream, 401, "token invalide").await?;
            } else {
                media::serve_download(&daemon, &mut stream, &head, &q).await?;
            }
        }
        ("GET" | "HEAD", _) => {
            // Pages statiques publiques (aucune donnée sensible : la SPA
            // est un coquillage vide sans le token — tout accès données
            // passe par /ws /rpc /media authentifiés).
            serve_static(&mut stream, &head, path, &ctx).await?;
        }
        _ => {
            http::respond_err(&mut stream, 404, "introuvable").await?;
        }
    }
    Ok(())
}

/// `POST /rpc` : un objet `Request` JSON → `Response` JSON via le
/// dispatch partagé. Terminaux/subscribe n'ont pas de couche connexion
/// ici → erreur propre côté démon (le front passe par /ws pour tout).
async fn rpc(daemon: &Arc<Daemon>, stream: &mut TcpStream, head: &http::Head) -> Result<()> {
    let body = match http::read_body(stream, head).await {
        Ok(b) => b,
        Err(e) => return http::respond_err(stream, 411, &format!("{e:#}")).await,
    };
    let parsed: Result<proto::Request, _> = serde_json::from_slice(&body);
    let resp = match parsed {
        Ok(req) => daemon.dispatch(req).await,
        Err(e) => proto::Response::err(0, format!("parse: {e}")),
    };
    let out = serde_json::to_vec(&resp)?;
    http::respond(
        stream,
        200,
        &[("Content-Type".into(), "application/json".into())],
        &out,
    )
    .await
}

/// Auth : query `?token=` (media/download/liens) ou `Authorization:
/// Bearer <t>` (fetch). Comparaison en temps constant — le token vit
/// sur loopback mais le header peut arriver par un forward SSH partagé.
fn authed(head: &http::Head, q: &[(String, String)], ctx: &WebCtx) -> bool {
    let tok = http::qparam(q, "token").map(str::to_string).or_else(|| {
        head.header("authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|v| v.trim().to_string())
    });
    tok.is_some_and(|t| const_eq(t.as_bytes(), ctx.token.as_bytes()))
}

/// Égalité sans early-exit — borne le side-channel de timing.
fn const_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Anti-CSWSH/CSRF sur les endpoints mutables (WS, RPC) : si le client
/// envoie Origin (navigateurs = toujours pour WS, toujours pour POST
/// cross/same-origin récent), son hôte:port doit égaler le header Host.
/// Les clients non-navigateur (curl, acctl) n'envoient pas Origin → ok.
fn origin_ok(head: &http::Head) -> bool {
    let Some(origin) = head.header("origin") else {
        return true;
    };
    let Some(host) = head.header("host") else {
        return false;
    };
    // origin = scheme://host[:port] — on isole l'autorité.
    let authority = origin
        .split("://")
        .nth(1)
        .unwrap_or(origin)
        .trim_end_matches('/');
    authority.eq_ignore_ascii_case(host.trim())
}

/// Sert la SPA : asset demandé s'il existe, sinon index.html (les
/// chemins « route » comme /session/x n'ont pas de fichier — la SPA
/// résout tout seule côté client), sinon page placeholder quand la SPA
/// n'est pas installée (daemon seul, build cargo sans frontend).
async fn serve_static(
    stream: &mut TcpStream,
    head: &http::Head,
    path: &str,
    ctx: &WebCtx,
) -> Result<()> {
    match &ctx.assets {
        Some(Assets::Embed(m)) => {
            let key = path.trim_start_matches('/');
            let (name, body) = m
                .get(key)
                .map(|b| (key, *b))
                .or_else(|| m.get("index.html").map(|b| ("index.html", *b)))
                .unwrap_or(("index.html", PLACEHOLDER.as_bytes()));
            let mut headers = vec![
                ("Content-Type".into(), mime_by_ext(name).into()),
                ("X-Content-Type-Options".into(), "nosniff".into()),
            ];
            if name.ends_with(".html") || body == PLACEHOLDER.as_bytes() {
                headers.push(("Content-Security-Policy".into(), CSP.into()));
            }
            if head.method == "HEAD" {
                http::respond_head(stream, 200, &headers, body.len()).await
            } else {
                http::respond(stream, 200, &headers, body).await
            }
        }
        Some(Assets::Dir(web_dir)) => {
            let p = safe_join(web_dir, path);
            let f = p.filter(|p| p.is_file()).or_else(|| {
                let idx = web_dir.join("index.html");
                idx.is_file().then_some(idx)
            });
            match f {
                Some(f) => send_static_file(stream, head.method == "HEAD", &f).await,
                None => {
                    http::respond(stream, 200, &html_headers(), PLACEHOLDER.as_bytes()).await
                }
            }
        }
        None => {
            http::respond(stream, 200, &html_headers(), PLACEHOLDER.as_bytes()).await
        }
    }
}

/// Joint un chemin URL à web_dir en restant dedans : percent-decode,
/// refus des `..`/séparateurs Windows/encodages imbriqués.
fn safe_join(base: &PathBuf, url_path: &str) -> Option<PathBuf> {
    let decoded = pct_decode(url_path.split('?').next().unwrap_or(""));
    let mut out = base.clone();
    for c in std::path::Path::new(&decoded).components() {
        match c {
            Component::Normal(seg) => {
                let seg = seg.to_string_lossy();
                if seg.is_empty() || seg == "." || seg == ".." || seg.contains('\\') {
                    return None;
                }
                out.push(seg.as_ref());
            }
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Fichier statique complet en mémoire (les assets SPA sont petits —
/// vite les bornes à ~1 Mio), puis Content-Type + CSP.
async fn send_static_file(stream: &mut TcpStream, head_only: bool, p: &PathBuf) -> Result<()> {
    let body = tokio::fs::read(p).await.with_context(|| format!("{}", p.display()))?;
    let name = p.file_name().unwrap_or_default().to_string_lossy();
    let mut headers: Vec<(String, String)> = vec![
        ("Content-Type".into(), mime_by_ext(&name).into()),
        ("X-Content-Type-Options".into(), "nosniff".into()),
    ];
    if name.ends_with(".html") {
        headers.push(("Content-Security-Policy".into(), CSP.into()));
    }
    let mut resp_head = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in &headers {
        resp_head.push_str(&format!("{k}: {v}\r\n"));
    }
    resp_head.push_str("\r\n");
    use tokio::io::AsyncWriteExt;
    stream.write_all(resp_head.as_bytes()).await?;
    if !head_only {
        stream.write_all(&body).await?;
    }
    Ok(())
}

fn html_headers() -> Vec<(String, String)> {
    vec![
        ("Content-Type".into(), "text/html; charset=utf-8".into()),
        ("Content-Security-Policy".into(), CSP.into()),
        ("X-Content-Type-Options".into(), "nosniff".into()),
    ]
}

fn mime_by_ext(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    match ext.to_ascii_lowercase().as_str() {
        "html" => "text/html",
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wasm" => "application/wasm",
        "map" => "application/json",
        _ => "application/octet-stream",
    }
}

/// Servi quand le daemon tourne sans assets SPA (build cargo nu ou
/// web_dir absent) — l'interface web reste joignable et explique la
/// suite au lieu d'un 404 nu.
const PLACEHOLDER: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>AgentControl — web UI</title>
<style>body{font-family:system-ui;margin:4rem auto;max-width:36rem;color:#ccc;background:#1a1a1a}
code{background:#333;padding:.1em .4em;border-radius:4px}</style></head>
<body><h1>AgentControl — web UI</h1>
<p>Le démon écoute, mais les assets web ne sont pas installés.</p>
<p>Installez la SPA dans <code>web/</code> (releases : extraire
<code>web.tar.gz</code> à côté du binaire ou dans le dossier de données)
ou configurez <code>web_dir</code> dans config.toml.</p>
</body></html>"#;
