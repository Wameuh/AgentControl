//! Mini-HTTP/1.1 — le strict nécessaire au front web du démon :
//! tête de requête bornée, query string, construction de réponses.
//! Pas de framework : le périmètre est trois endpoints JSON + des GET
//! statiques/streamés, et les réponses partent en `Connection: close`
//! (le navigateur rouvre une socket par requête — transparent sur
//! loopback, et l'état partagé n'existe pas).

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Borne de la tête de requête (request line + headers). Un navigateur
/// n'émet jamais plus de quelques Ko — au-delà = client hostile/défectueux.
const HEAD_MAX: usize = 32 * 1024;
/// Corps max d'un POST /rpc — write_file embarque ~22 Mio de base64.
pub const BODY_MAX: usize = 32 * 1024 * 1024;

/// Tête de requête décodée.
pub struct Head {
    pub method: String,
    /// Cible brute "/path?query".
    pub target: String,
    /// (nom minuscule, valeur) dans l'ordre reçu.
    pub headers: Vec<(String, String)>,
    /// Octets déjà lus au-delà de la tête (début du corps POST).
    pub rest: Vec<u8>,
}

impl Head {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.trim())
    }
    /// Chemin + query éclatés : "/media?a=b" → ("/media", "a=b").
    pub fn path_query(&self) -> (&str, &str) {
        match self.target.split_once('?') {
            Some((p, q)) => (p, q),
            None => (self.target.as_str(), ""),
        }
    }
}

/// Lit request line + headers jusqu'à CRLFCRLF (borné). Renvoie la tête
/// parsée ; les octets excédentaires (début du corps) restent dans `rest`.
pub async fn read_head(stream: &mut TcpStream) -> Result<Head> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let end = loop {
        let n = stream
            .read(&mut chunk)
            .await
            .context("lecture requête")?;
        if n == 0 {
            anyhow::bail!("connexion fermée avant la fin de la tête");
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = find_head_end(&buf) {
            break i;
        }
        if buf.len() > HEAD_MAX {
            anyhow::bail!("tête trop grande (> {HEAD_MAX} o)");
        }
    };
    let rest = buf.split_off(end + 4);
    let text = String::from_utf8_lossy(&buf[..end]);
    let mut lines = text.split("\r\n");
    let req = lines.next().unwrap_or_default();
    let mut it = req.split_whitespace();
    let method = it.next().unwrap_or_default().to_ascii_uppercase();
    let target = it.next().unwrap_or_default().to_string();
    if method.is_empty() || target.is_empty() {
        anyhow::bail!("request line invalide: {req}");
    }
    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    Ok(Head {
        method,
        target,
        headers,
        rest,
    })
}

/// Position du premier "\r\n\r\n" dans buf, s'il est complet.
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Lit le corps POST : `head.rest` (octets lus au-delà de la tête) +
/// le complément jusqu'à Content-Length (borné BODY_MAX). `chunked`
/// n'est pas supporté — les clients concernés envoient Content-Length.
pub async fn read_body(stream: &mut TcpStream, head: &Head) -> Result<Vec<u8>> {
    if head
        .header("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
    {
        anyhow::bail!("chunked non supporté — envoyez Content-Length");
    }
    let want = head
        .header("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    if want > BODY_MAX {
        anyhow::bail!("corps trop grand (> {BODY_MAX} o)");
    }
    let mut body = head.rest.clone();
    body.truncate(want);
    let mut chunk = vec![0u8; 64 * 1024];
    while body.len() < want {
        let room = (want - body.len()).min(chunk.len());
        let n = stream
            .read(&mut chunk[..room])
            .await
            .context("lecture corps")?;
        if n == 0 {
            anyhow::bail!("corps tronqué");
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Ok(body)
}

/// Query string → paires décodées (percent-encoding, `+` = espace).
pub fn query_params(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (pct_decode(k), pct_decode(v))
        })
        .collect()
}

/// Paramètre de query unique.
pub fn qparam<'a>(q: &'a [(String, String)], name: &str) -> Option<&'a str> {
    q.iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let h = hex_val(b[i + 1]);
                let l = hex_val(b[i + 2]);
                if let (Some(h), Some(l)) = (h, l) {
                    out.push((h << 4) | l);
                    i += 3;
                } else {
                    out.push(b[i]);
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Statuts utilisés par le front.
pub fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        204 => "No Content",
        206 => "Partial Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Payload Too Large",
        416 => "Range Not Satisfiable",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "OK",
    }
}

/// Écrit une réponse complète (tête + corps mémoire) puis rend la main —
/// l'appelant ferme la socket (Connection: close systématique).
pub async fn respond(
    stream: &mut TcpStream,
    code: u16,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<()> {
    let mut out = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        code,
        status_text(code),
        body.len()
    );
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    stream.write_all(out.as_bytes()).await?;
    stream.write_all(body).await?;
    Ok(())
}

/// Tête seule avec Content-Length réel (réponse HEAD sur une ressource
/// dont le corps n'est pas envoyé).
pub async fn respond_head(
    stream: &mut TcpStream,
    code: u16,
    headers: &[(String, String)],
    len: usize,
) -> Result<()> {
    let mut out = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {len}\r\nConnection: close\r\n",
        code,
        status_text(code)
    );
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    stream.write_all(out.as_bytes()).await?;
    Ok(())
}

/// Réponse JSON courte d'erreur — convention `{"error": "…"}`.
pub async fn respond_err(stream: &mut TcpStream, code: u16, msg: &str) -> Result<()> {
    let body = serde_json::json!({"error": msg}).to_string();
    respond(
        stream,
        code,
        &[
            ("Content-Type".into(), "application/json".into()),
            ("X-Content-Type-Options".into(), "nosniff".into()),
        ],
        body.as_bytes(),
    )
    .await
}
