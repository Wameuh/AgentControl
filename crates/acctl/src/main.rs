//! acctl — client CLI de test pour agentcontrol-daemon.
//!
//! Usage :
//!   acctl status | agents | list
//!   acctl new <agent> <cwd> [prompt...] [--title T] [--mode M] [--model X]
//!   acctl send <session> <text...>
//!   acctl msg <from> <to> [--steer] <text...>   (message agent→agent)
//!   acctl events <session> [before_seq] [limit]
//!   acctl tail <session> [after_seq]
//!   acctl resume|cancel|kill <session>
//!   acctl mode <session> <mode>
//!   acctl respond <session> <request_id> [option_id | --answer JSON]
//!   acctl blob <session> <blob> [offset] [limit]
//!   acctl download <session> <path> [dest]     (dossier → zip auto)
//!
//! Connexion : $AGENTCONTROL_SOCK (défaut ~/.agentcontrol/daemon.sock)
//!             ou $AGENTCONTROL_TCP host:port

use anyhow::{bail, Context, Result};
use proto::Request;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

struct Conn {
    w: Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
    lines: tokio::io::Lines<BufReader<Box<dyn tokio::io::AsyncRead + Unpin + Send>>>,
    next_id: u64,
}

async fn connect() -> Result<Conn> {
    if let Ok(addr) = std::env::var("AGENTCONTROL_TCP") {
        let s = TcpStream::connect(&addr).await.with_context(|| format!("tcp {addr}"))?;
        let (r, w) = s.into_split();
        return Ok(Conn {
            w: Box::new(w),
            lines: BufReader::new(Box::new(r) as Box<dyn tokio::io::AsyncRead + Unpin + Send>).lines(),
            next_id: 1,
        });
    }
    #[cfg(unix)]
    {
        let path = std::env::var("AGENTCONTROL_SOCK").unwrap_or_else(|_| {
            format!("{}/.agentcontrol/daemon.sock", std::env::var("HOME").unwrap_or_else(|_| ".".into()))
        });
        let s = tokio::net::UnixStream::connect(&path)
            .await
            .with_context(|| format!("unix {path} — le démon tourne-t-il ?"))?;
        let (r, w) = s.into_split();
        return Ok(Conn {
            w: Box::new(w),
            lines: BufReader::new(Box::new(r) as Box<dyn tokio::io::AsyncRead + Unpin + Send>).lines(),
            next_id: 1,
        });
    }
    #[cfg(not(unix))]
    {
        bail!("pas de socket unix ici — utilisez AGENTCONTROL_TCP=host:port")
    }
}

impl Conn {
    async fn call(&mut self, req: proto::ReqKind) -> Result<Value> {
        let r = Request {
            id: self.next_id,
            req,
        };
        self.next_id += 1;
        let mut s = serde_json::to_string(&r)?;
        s.push('\n');
        self.w.write_all(s.as_bytes()).await?;
        self.w.flush().await?;
        let line = self.lines.next_line().await?.context("connexion fermée")?;
        let resp: proto::Response = serde_json::from_str(&line)?;
        if resp.ok {
            Ok(resp.result.unwrap_or(Value::Null))
        } else {
            bail!("{}", resp.error.unwrap_or_else(|| "?".into()))
        }
    }

    /// Envoie un subscribe puis imprime les push events.
    async fn tail(&mut self, session: &str, after_seq: u64) -> Result<()> {
        let r = Request {
            id: self.next_id,
            req: proto::ReqKind::Subscribe {
                session: session.into(),
                after_seq,
            },
        };
        self.next_id += 1;
        let mut s = serde_json::to_string(&r)?;
        s.push('\n');
        self.w.write_all(s.as_bytes()).await?;
        self.w.flush().await?;
        loop {
            let line = match self.lines.next_line().await? {
                Some(l) => l,
                None => break,
            };
            let v: Value = serde_json::from_str(&line)?;
            if v.get("push").is_some() {
                print_event(&v["event"]);
            }
        }
        Ok(())
    }
}

fn print_event(ev: &Value) {
    let seq = ev["seq"].as_u64().unwrap_or(0);
    let ty = ev["type"].as_str().unwrap_or("?");
    match ty {
        "user_message" => println!("\x1b[36m[{seq}] vous> {}\x1b[0m", ev["text"].as_str().unwrap_or("")),
        "peer_message" => println!(
            "\x1b[35m[{seq}] {}→{} («{}»→«{}»)> {}\x1b[0m",
            ev["from_session"].as_str().unwrap_or("?"),
            ev["to_session"].as_str().unwrap_or("?"),
            ev["from_title"].as_str().unwrap_or("?"),
            ev["to_title"].as_str().unwrap_or("?"),
            ev["text"].as_str().unwrap_or("")
        ),
        "agent_text" => println!("[{seq}] {}", ev["text"].as_str().unwrap_or("")),
        "agent_thought" => println!("\x1b[90m[{seq}] (pensée) {}\x1b[0m", ev["text"].as_str().unwrap_or("")),
        "tool_call" | "tool_call_update" => {
            let title = ev["title"].as_str().unwrap_or_else(|| ev["call_id"].as_str().unwrap_or("?"));
            let status = ev["status"].as_str().unwrap_or("?");
            println!("\x1b[33m[{seq}] ⚙ {title} — {status}\x1b[0m");
            if let Some(x) = ev["excerpt"].as_str() {
                for l in x.lines().take(5) {
                    println!("    \x1b[90m{l}\x1b[0m");
                }
            }
            if let Some(b) = ev["blob"].as_str() {
                println!("    \x1b[90m[sortie complète: {b}, {} o]\x1b[0m", ev["size"].as_u64().unwrap_or(0));
            }
        }
        "permission_request" => {
            println!("\x1b[31m[{seq}] PERMISSION {}\x1b[0m", ev["title"].as_str().unwrap_or(""));
            if let Some(d) = ev["detail"].as_str() {
                for l in d.lines().take(10) {
                    println!("    \x1b[90m{l}\x1b[0m");
                }
            }
            for o in ev["options"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
                println!("    - {} ({})", o["option_id"].as_str().unwrap_or(""), o["name"].as_str().unwrap_or(""));
            }
            println!("    → acctl respond <session> {} <option_id>", ev["request_id"].as_str().unwrap_or(""));
        }
        "question_request" => {
            println!("\x1b[35m[{seq}] QUESTION {}\x1b[0m", ev["message"].as_str().unwrap_or(""));
            println!("    → acctl respond <session> {} --answer '{{...}}'", ev["request_id"].as_str().unwrap_or(""));
        }
        "permission_resolved" | "question_resolved" => {
            println!("[{seq}] résolu: {}", ev["outcome"].as_str().or_else(|| ev["answer"].as_str()).unwrap_or(""));
        }
        "mode_changed" => println!("[{seq}] mode → {}", ev["mode"].as_str().unwrap_or("")),
        "commands_available" => {
            let names: Vec<String> = ev["commands"].as_array().map(|a| {
                a.iter().map(|c| {
                    format!("/{}", c.get("name").and_then(Value::as_str).or_else(|| c.as_str()).unwrap_or(""))
                }).collect()
            }).unwrap_or_default();
            println!("[{seq}] commandes: {}", names.join(" "));
        }
        "status_changed" => println!("\x1b[90m[{seq}] état → {}\x1b[0m", ev["state"].as_str().unwrap_or("")),
        "error" => println!("\x1b[31m[{seq}] ERREUR {}\x1b[0m", ev["message"].as_str().unwrap_or("")),
        "system" => println!("\x1b[90m[{seq}] ⦿ {}\x1b[0m", ev["text"].as_str().unwrap_or("")),
        _ => println!("[{seq}] {ty} {ev}"),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: acctl status|agents|list|new|send|events|tail|resume|attach|fork|cancel|kill|mode|respond|blob|download");
        std::process::exit(2);
    }
    let mut c = connect().await?;
    match args[0].as_str() {
        "status" => println!("{}", c.call(proto::ReqKind::DaemonStatus).await?),
        "agents" => println!("{}", c.call(proto::ReqKind::ListAgents).await?),
        "list" => {
            let v = c.call(proto::ReqKind::ListSessions).await?;
            for s in v.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
                println!(
                    "{}  {:<12} {:<14} {}  {}",
                    s["id"].as_str().unwrap_or(""),
                    s["state"].as_str().unwrap_or(""),
                    s["agent"].as_str().unwrap_or(""),
                    s["created_at"].as_str().unwrap_or("").get(..16).unwrap_or(""),
                    s["title"].as_str().unwrap_or("")
                );
            }
        }
        "new" => {
            // acctl new <agent> <cwd> [prompt...] [--title T] [--mode M] [--model X]
            if args.len() < 3 {
                bail!("new <agent> <cwd> [prompt...]");
            }
            let agent = args[1].clone();
            let cwd = args[2].clone();
            let mut prompt_parts = Vec::new();
            let mut title = None;
            let mut mode = None;
            let mut model = None;
            let mut i = 3;
            while i < args.len() {
                match args[i].as_str() {
                    "--title" => { title = args.get(i + 1).cloned(); i += 2; }
                    "--mode" => { mode = args.get(i + 1).cloned(); i += 2; }
                    "--model" => { model = args.get(i + 1).cloned(); i += 2; }
                    p => { prompt_parts.push(p.to_string()); i += 1; }
                }
            }
            let v = c.call(proto::ReqKind::NewSession {
                agent, cwd, title, model, mode,
                prompt: if prompt_parts.is_empty() { None } else { Some(prompt_parts.join(" ")) },
            }).await?;
            println!("{}", v["session"].as_str().unwrap_or(""));
        }
        "send" => {
            // acctl send <session> [--steer|--force] <text...>
            if args.len() < 3 { bail!("send <session> [--steer|--force] <text...>"); }
            let mut steer = false;
            let mut force = false;
            let mut parts = Vec::new();
            for a in &args[2..] {
                match a.as_str() {
                    "--steer" => steer = true,
                    "--force" => force = true,
                    _ => parts.push(a.clone()),
                }
            }
            c.call(proto::ReqKind::Send {
                session: args[1].clone(),
                text: parts.join(" "),
                force,
                steer,
            }).await?;
        }
        "msg" => {
            // acctl msg <from> <to> [--steer] <text...>
            if args.len() < 4 { bail!("msg <from> <to> [--steer] <text...>"); }
            let mut steer = false;
            let mut parts = Vec::new();
            for a in &args[3..] {
                match a.as_str() {
                    "--steer" => steer = true,
                    _ => parts.push(a.clone()),
                }
            }
            println!("{}", c.call(proto::ReqKind::PeerSend {
                from: args[1].clone(),
                session: args[2].clone(),
                text: parts.join(" "),
                delivery: if steer { "steer".into() } else { "queue".into() },
            }).await?);
        }
        "events" => {
            let before = args.get(2).and_then(|s| s.parse().ok()).filter(|n: &u64| *n > 0);
            let limit = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(200);
            let v = c.call(proto::ReqKind::Events {
                session: args[1].clone(), before_seq: before, limit,
            }).await?;
            for ev in v["events"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
                print_event(ev);
            }
            eprintln!("(last_seq={})", v["last_seq"].as_u64().unwrap_or(0));
        }
        "tail" => {
            let after = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
            c.tail(&args[1], after).await?;
        }
        "resume" => println!("{}", c.call(proto::ReqKind::ResumeSession {
            session: args[1].clone(),
            native_id: args.get(2).cloned(),
        }).await?),
        "attach" => println!("{}", c.call(proto::ReqKind::AttachSession {
            agent: args[1].clone(),
            cwd: args[2].clone(),
            native_id: args[3].clone(),
            title: args.get(4).cloned(),
        }).await?),
        "fork" => println!("{}", c.call(proto::ReqKind::ForkSession {
            session: args[1].clone(),
        }).await?),
        "cancel" => println!("{}", c.call(proto::ReqKind::Cancel { session: args[1].clone() }).await?),
        "kill" => println!("{}", c.call(proto::ReqKind::KillSession { session: args[1].clone() }).await?),
        "rm" => println!("{}", c.call(proto::ReqKind::DeleteSession { session: args[1].clone() }).await?),
        "mode" => println!("{}", c.call(proto::ReqKind::SetMode { session: args[1].clone(), mode: args[2].clone() }).await?),
        "model" => println!("{}", c.call(proto::ReqKind::SetModel { session: args[1].clone(), model: args[2].clone() }).await?),
        "setcfg" => println!("{}", c.call(proto::ReqKind::SetConfigOption {
            session: args[1].clone(),
            config_id: args[2].clone(),
            value: args[3].clone(),
        }).await?),
        "respond" => {
            // respond <session> <request_id> [option_id | --answer JSON]
            let mut option_id = None;
            let mut answer = None;
            if args.len() > 3 {
                if args[3] == "--answer" {
                    answer = Some(serde_json::from_str(args.get(4).map(String::as_str).unwrap_or("null"))?);
                } else {
                    option_id = Some(args[3].clone());
                }
            }
            println!("{}", c.call(proto::ReqKind::Respond {
                session: args[1].clone(),
                request_id: args[2].clone(),
                option_id,
                answer,
            }).await?);
        }
        "blob" => {
            let offset = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
            let limit = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(65536);
            let v = c.call(proto::ReqKind::GetBlob {
                session: args[1].clone(), blob: args[2].clone(), offset, limit,
            }).await?;
            print!("{}", v["data"].as_str().unwrap_or(""));
            eprintln!("(total={} offset={})", v["total"], v["offset"]);
        }
        "download" | "dl" => {
            // acctl download <session> <path> [dest] — dossier → zip côté démon
            if args.len() < 3 { bail!("download <session> <path> [dest]"); }
            let (session, path) = (args[1].clone(), args[2].clone());
            let info = c.call(proto::ReqKind::DownloadPath {
                session: session.clone(), path: path.clone(),
                offset: 0, limit: 0, archive: None,
            }).await?;
            let dest = args.get(3).cloned()
                .unwrap_or_else(|| info["name"].as_str().unwrap_or("download").to_string());
            if std::path::Path::new(&dest).is_dir() {
                bail!("destinataire est un dossier: {dest}");
            }
            let mut out: Option<std::fs::File> = None;
            let mut archive: Option<String> = None;
            let mut offset = 0u64;
            use base64::Engine;
            use std::io::Write;
            loop {
                let v = c.call(proto::ReqKind::DownloadPath {
                    session: session.clone(), path: path.clone(),
                    offset, limit: 1 << 20, archive: archive.clone(),
                }).await?;
                if archive.is_none() {
                    archive = v["archive"].as_str().map(String::from);
                }
                let total = v["total"].as_u64().unwrap_or(0);
                let data = base64::engine::general_purpose::STANDARD
                    .decode(v["data"].as_str().unwrap_or(""))?;
                // Fichier créé au premier chunk reçu — pas de fichier vide
                // résiduel si le démon échoue d'emblée.
                if out.is_none() {
                    out = Some(std::fs::File::create(&dest)?);
                }
                out.as_mut().unwrap().write_all(&data)?;
                offset += data.len() as u64;
                eprint!("\r{offset}/{total} o");
                if offset >= total { break; }
            }
            eprintln!("\n→ {dest} ({offset} o)");
        }
        other => bail!("commande inconnue: {other}"),
    }
    Ok(())
}
