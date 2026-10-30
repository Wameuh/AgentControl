//! WebSocket (RFC 6455) — codec minimal sans dépendance web :
//! handshake Sec-WebSocket-Accept (SHA-1 base64), frames binaires/texte
//! fragmentables, ping/pong, close. Chaque connexion WS est pontée sur
//! `Daemon::serve_conn` via une paire de duplex mémoire — le navigateur
//! parle exactement le protocole NDJSON du socket TCP/Unix, les lignes
//! JSON sont transportées comme payload de frames texte.
//!
//! La réutilisation de `serve_conn` donne gratuitement : dispatch des
//! requêtes, terminaux (term_data/term_input), et surtout `subscribe` →
//! events pushed en continu = le canal temps réel du front web.

use anyhow::{Context, Result};
use sha1::{Digest, Sha1};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::server::Daemon;
use std::sync::Arc;

/// Frame max acceptée — write_file ≈ 22 Mio base64 → marge généreuse.
const FRAME_MAX: u64 = 32 * 1024 * 1024;
/// Message complet max (agrégation des fragments).
const MSG_MAX: usize = 48 * 1024 * 1024;
/// Files mémoire pont ↔ serve_conn.
const DUPLEX_CAP: usize = 256 * 1024;
/// Messages sortants en file vers le writer (frames push/dispatch).
const OUTBOUND_CAP: usize = 64;

/// GUID imposé par la RFC pour le hash d'acceptation.
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

fn accept_key(key: &str) -> String {
    let mut h = Sha1::new();
    h.update(key.as_bytes());
    h.update(WS_GUID.as_bytes());
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(h.finalize())
}

/// Une frame sortante vers le navigateur : (opcode, payload).
type OutFrame = (u8, Vec<u8>);

/// Termine le handshake HTTP 101 puis pont WS ↔ NDJSON ↔ `serve_conn`
/// jusqu'à la fin de la connexion (close frame, socket morte ou
/// `serve_conn` terminé — p. ex. démon en shutdown).
pub async fn upgrade_and_serve(
    daemon: Arc<Daemon>,
    mut stream: TcpStream,
    head: &super::http::Head,
) -> Result<()> {
    let key = head
        .header("sec-websocket-key")
        .context("Sec-WebSocket-Key manquant")?;
    let upgrade = head
        .header("upgrade")
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    let version_ok = head.header("sec-websocket-version") == Some("13");
    let conn_hdr = head
        .header("connection")
        .is_some_and(|v| v.to_ascii_lowercase().contains("upgrade"));
    if !upgrade || !version_ok || !conn_hdr {
        anyhow::bail!("upgrade websocket invalide");
    }
    let rsp = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        accept_key(key)
    );
    stream.write_all(rsp.as_bytes()).await?;

    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "?".into());
    let (rd, wr) = tokio::io::split(stream);
    // a_in : frames WS → octets NDJSON vers serve_conn.
    // b_out : octets NDJSON de serve_conn → frames WS.
    let (a_in, srv_in) = tokio::io::duplex(DUPLEX_CAP);
    let (srv_out, b_out) = tokio::io::duplex(DUPLEX_CAP);
    let (otx, orx) = mpsc::channel::<OutFrame>(OUTBOUND_CAP);

    tracing::info!("conn ws open {peer}");
    let d = Arc::clone(&daemon);
    let mut conn = tokio::spawn(async move {
        let t0 = std::time::Instant::now();
        let r = d.serve_conn(tokio::io::join(srv_in, srv_out)).await;
        let secs = t0.elapsed().as_secs();
        match &r {
            Ok(()) => tracing::info!("conn ws {peer} close ({secs}s) — client parti"),
            Err(e) => tracing::info!("conn ws {peer} close ({secs}s): {e:#}"),
        }
        r
    });
    let mut reader = tokio::spawn(read_frames(rd, a_in, otx.clone()));
    let mut writer = tokio::spawn(write_frames(wr, b_out, orx));

    // Le premier qui termine clôt la session WS (et ses pairs via les
    // drops de duplex : EOF serve_conn → abort le reste proprement).
    tokio::select! {
        _ = &mut conn => {}
        _ = &mut reader => {}
        _ = &mut writer => {}
    }
    conn.abort();
    reader.abort();
    writer.abort();
    Ok(())
}

/// Lecteur WS : décode les frames, alimente `sink` (NDJSON vers
/// serve_conn), répond ping→pong, honore close. Les messages JSON sont
/// transmis ligne à ligne terminés par '\n' — `serve_conn` lit en
/// lignes, la fragmentation WS est transparente.
async fn read_frames(
    mut rd: tokio::io::ReadHalf<TcpStream>,
    mut sink: tokio::io::DuplexStream,
    out: mpsc::Sender<OutFrame>,
) -> Result<()> {
    let mut msg: Vec<u8> = Vec::with_capacity(64 * 1024);
    loop {
        let mut h = [0u8; 2];
        rd.read_exact(&mut h).await.context("frame header")?;
        let fin = h[0] & 0x80 != 0;
        let op = h[0] & 0x0f;
        let masked = h[1] & 0x80 != 0;
        let mut len = (h[1] & 0x7f) as u64;
        if len == 126 {
            let mut e = [0u8; 2];
            rd.read_exact(&mut e).await?;
            len = u16::from_be_bytes(e) as u64;
        } else if len == 127 {
            let mut e = [0u8; 8];
            rd.read_exact(&mut e).await?;
            len = u64::from_be_bytes(e);
        }
        if !masked {
            let _ = out.try_send((0x8, close_payload(1002)));
            anyhow::bail!("frame non masquée (RFC : client → serveur masque toujours)");
        }
        // Frames de contrôle : payload ≤ 125 octets, jamais fragmentées.
        if op >= 0x8 && (len > 125 || !fin) {
            let _ = out.try_send((0x8, close_payload(1002)));
            anyhow::bail!("frame de contrôle invalide");
        }
        if len > FRAME_MAX {
            let _ = out.try_send((0x8, close_payload(1009)));
            anyhow::bail!("frame trop grande ({len} o)");
        }
        let mut mask = [0u8; 4];
        rd.read_exact(&mut mask).await?;
        let mut payload = vec![0u8; len as usize];
        rd.read_exact(&mut payload).await.context("frame payload")?;
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i % 4];
        }
        match op {
            // Continuation / texte / binaire : agrégat NDJSON.
            0x0 | 0x1 | 0x2 => {
                msg.extend_from_slice(&payload);
                if msg.len() > MSG_MAX {
                    let _ = out.try_send((0x8, close_payload(1009)));
                    anyhow::bail!("message trop grand (> {MSG_MAX} o)");
                }
                if fin {
                    // Une ligne JSON par message — le client n'envoie
                    // jamais plusieurs lignes dans une frame.
                    sink.write_all(&msg).await?;
                    sink.write_all(b"\n").await?;
                    msg.clear();
                }
            }
            0x8 => {
                let _ = out.try_send((0x8, payload));
                return Ok(());
            }
            0x9 => {
                let _ = out.try_send((0xA, payload));
            }
            0xA => {}
            _ => {
                let _ = out.try_send((0x8, close_payload(1002)));
                anyhow::bail!("opcode inconnu {op:#x}");
            }
        }
    }
}

fn close_payload(code: u16) -> Vec<u8> {
    code.to_be_bytes().to_vec()
}

/// Writer WS : lignes NDJSON de `src` (sortie serve_conn) → frames
/// texte ; messages de contrôle `ctl` (pong/close) prioritaires.
async fn write_frames(
    mut wr: tokio::io::WriteHalf<TcpStream>,
    src: tokio::io::DuplexStream,
    mut ctl: mpsc::Receiver<OutFrame>,
) -> Result<()> {
    let mut lines = BufReader::new(src).lines();
    loop {
        tokio::select! {
            biased;
            m = ctl.recv() => {
                match m {
                    Some((op, payload)) => {
                        write_frame(&mut wr, op, &payload).await?;
                        if op == 0x8 {
                            return Ok(());
                        }
                    }
                    None => return Ok(()),
                }
            }
            l = lines.next_line() => {
                match l.context("lecture serve_conn")? {
                    Some(line) => write_frame(&mut wr, 0x1, line.as_bytes()).await?,
                    None => {
                        // serve_conn terminé → close propre côté WS.
                        let _ = write_frame(&mut wr, 0x8, &[]).await;
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Écrit une frame serveur→client : FIN=1, pas de masque (le serveur
/// ne masque jamais), longueur étendue selon la taille.
async fn write_frame(
    wr: &mut tokio::io::WriteHalf<TcpStream>,
    op: u8,
    payload: &[u8],
) -> Result<()> {
    let len = payload.len();
    let mut head = Vec::with_capacity(10);
    head.push(0x80 | op);
    if len < 126 {
        head.push(len as u8);
    } else if len <= u16::MAX as usize {
        head.push(126);
        head.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        head.push(127);
        head.extend_from_slice(&(len as u64).to_be_bytes());
    }
    wr.write_all(&head).await?;
    wr.write_all(payload).await?;
    Ok(())
}
