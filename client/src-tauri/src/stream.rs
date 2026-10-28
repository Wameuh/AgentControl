//! Schéma `acvideo` — streaming vidéo borné par ranges HTTP.
//!
//! Le `<video>` de la webview demande des ranges (`Range: bytes=a-b`) ;
//! chaque requête est traduite en lecture(s) `read_file` bornée(s) vers
//! le démon (≤ 4 Mio par RPC). Le lecteur ne transfère donc que ce
//! qu'il bufferise réellement — métadonnées + quelques secondes au
//! chargement, la range de seek ensuite — sans rapatrier le fichier
//! entier (plusieurs Gio possibles) en mémoire ni en JSON/base64.
//!
//! URL : `acvideo://localhost/?session=…&conn=…&path=…`
//! (Windows/Android : `http://acvideo.localhost/?…` — forme imposée
//! par ces webviews, construite côté frontend).

use proto::ReqKind;
use serde_json::Value;
use tauri::http::{header, Method, Request, Response, StatusCode};
use tauri::{AppHandle, Manager, UriSchemeContext, UriSchemeResponder};

use client_core::Client;

use crate::state::{get_conn, AppState};

/// Données max servies par requête webview — borne mémoire. Couvrir
/// moins que la range demandée est légal : le lecteur repart avec une
/// nouvelle requête pour la suite.
const MAX_SERVE: u64 = 16 * 1024 * 1024;
/// Tranche max par RPC read_file (le démon borne déjà à DL_CHUNK).
const RPC_CHUNK: u64 = 4 * 1024 * 1024;

/// Enregistré sur le Builder — chaque requête est résolue dans le
/// runtime tokio (le responder est `Send`, pas de blocage du webview).
pub(crate) fn handler(
    ctx: UriSchemeContext<'_, tauri::Wry>,
    req: Request<Vec<u8>>,
    responder: UriSchemeResponder,
) {
    let app = ctx.app_handle().clone();
    tauri::async_runtime::spawn(async move {
        responder.respond(serve(&app, &req).await);
    });
}

struct Query {
    session: String,
    conn: Option<String>,
    path: String,
}

async fn serve(app: &AppHandle, req: &Request<Vec<u8>>) -> Response<Vec<u8>> {
    let Some(q) = parse_query(req.uri().query().unwrap_or("")) else {
        return err(StatusCode::BAD_REQUEST, "params session/path requis");
    };
    let state = app.state::<AppState>();
    let client = match get_conn(&state, q.conn.as_deref()).await {
        Ok(c) => c,
        Err(e) => return err(StatusCode::BAD_GATEWAY, &e),
    };
    // HEAD (sondes) et ranges suffixes (`bytes=-N`) ont besoin de la
    // taille avant de répondre — sonde d'1 octet.
    let head = *req.method() == Method::HEAD;
    let range = req
        .headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_range);
    let mut probe_size = None;
    if head || matches!(range, Some(Range::Suffix(_))) {
        match chunk(&client, &q, 0, 1).await {
            Ok((_, size)) => probe_size = Some(size),
            Err(e) => return err(StatusCode::NOT_FOUND, &e),
        }
    }
    if head {
        return Response::builder()
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::CONTENT_LENGTH, probe_size.unwrap_or(0))
            .header(header::CONTENT_TYPE, mime(&q.path))
            .body(Vec::new())
            .unwrap_or_else(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "response"));
    }
    // Range → [start, quantité voulue]. Absente → début du flux.
    // Suffixe = les n derniers octets : une couverture partielle doit
    // finir à l'EOF → servir la fin de la range demandée.
    let (start, want) = match range {
        Some(Range::Span(a, b)) if b >= a => {
            (a, b.saturating_sub(a).saturating_add(1).min(MAX_SERVE))
        }
        Some(Range::Span(..)) => return unsatisfiable(probe_size.unwrap_or(0)),
        Some(Range::From(a)) => (a, MAX_SERVE),
        Some(Range::Suffix(n)) => {
            let size = probe_size.unwrap_or(0);
            if n == 0 || size == 0 {
                return unsatisfiable(size);
            }
            // Couvrir la FIN du suffixe — les lecteurs demandent `bytes=-n`
            // pour atteindre les métadonnées en queue de fichier (moov).
            let m = n.min(size).min(MAX_SERVE);
            (size - m, m)
        }
        None => (0, MAX_SERVE),
    };
    // Assemble la tranche demandée en RPC chaînées (borne MAX_SERVE —
    // une couverture partielle de la range est une réponse 206 valide).
    let mut body = Vec::new();
    let mut size = probe_size.unwrap_or(0);
    while (body.len() as u64) < want {
        let off = start + body.len() as u64;
        let lim = (want - body.len() as u64).min(RPC_CHUNK);
        match chunk(&client, &q, off, lim).await {
            Ok((bytes, total)) => {
                size = total;
                if bytes.is_empty() {
                    break;
                }
                body.extend_from_slice(&bytes);
            }
            Err(e) => return err(StatusCode::NOT_FOUND, &e),
        }
    }
    if start >= size || body.is_empty() {
        return unsatisfiable(size);
    }
    Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_TYPE, mime(&q.path))
        .header(header::CONTENT_LENGTH, body.len())
        .header(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{}", start, start + body.len() as u64 - 1, size),
        )
        .header(header::CACHE_CONTROL, "no-store")
        .body(body)
        .unwrap_or_else(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "response"))
}

/// Tranche read_file bornée → (octets, taille totale du fichier).
async fn chunk(client: &Client, q: &Query, offset: u64, limit: u64) -> Result<(Vec<u8>, u64), String> {
    let v: Value = client
        .call(ReqKind::ReadFile {
            session: q.session.clone(),
            path: q.path.clone(),
            binary: true,
            offset,
            limit: Some(limit),
        })
        .await
        .map_err(|e| format!("{e:#}"))?;
    // Un démon plus ancien ignore offset/limit → forme de réponse sans
    // `offset` (fichier entier plafonné à 12 Mio) : inutilisable pour
    // servir une range — exiger la forme bornée.
    if v.get("offset").and_then(Value::as_u64) != Some(offset) {
        return Err("streaming non supporté — démon trop ancien".into());
    }
    let b64 = v["data"].as_str().unwrap_or_default();
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| e.to_string())?;
    Ok((bytes, v["size"].as_u64().unwrap_or(0)))
}

/// Paramètres `?session=&conn=&path=` percent-encodés.
fn parse_query(q: &str) -> Option<Query> {
    let (mut session, mut conn, mut path) = (None, None, None);
    for (k, v) in url::form_urlencoded::parse(q.as_bytes()) {
        match &*k {
            "session" => session = Some(v.into_owned()),
            "conn" => conn = Some(v.into_owned()),
            "path" => path = Some(v.into_owned()),
            _ => {}
        }
    }
    Some(Query {
        session: session?,
        conn,
        path: path?,
    })
}

enum Range {
    /// bytes=a-b (bornes incluses).
    Span(u64, u64),
    /// bytes=a- (jusqu'à la fin).
    From(u64),
    /// bytes=-n (les n derniers octets).
    Suffix(u64),
}

fn parse_range(v: &str) -> Option<Range> {
    let v = v.strip_prefix("bytes=")?.split(',').next()?.trim();
    let (a, b) = v.split_once('-')?;
    if a.is_empty() {
        return b.trim().parse().ok().map(Range::Suffix);
    }
    let a = a.trim().parse().ok()?;
    Some(if b.is_empty() {
        Range::From(a)
    } else {
        Range::Span(a, b.trim().parse().ok()?)
    })
}

/// MIME par extension — les lecteurs refusent un type erroné.
fn mime(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "ogv" | "ogg" => "video/ogg",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        _ => "application/octet-stream",
    }
}

fn unsatisfiable(size: u64) -> Response<Vec<u8>> {
    Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::CONTENT_RANGE, format!("bytes */{size}"))
        .body(Vec::new())
        .unwrap_or_else(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "response"))
}

fn err(status: StatusCode, msg: &str) -> Response<Vec<u8>> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(msg.as_bytes().to_vec())
        .unwrap_or_else(|_| Response::new(Vec::new()))
}
