//! acctl — client CLI de test pour agentcontrol-daemon.
//!
//! Usage :
//!   acctl status | agents [install <id>] | list
//!   acctl new <agent> <cwd> [prompt...] [--title T] [--mode M] [--model X]
//!               [--origin O]   (provenance service — badge « via » client ;
//!               ex. --origin service:backlog-orchestrator)
//!   acctl send <session> <text...>
//!   acctl msg <from> <to> [--queue] <text...>   (message agent→agent —
//!       steer par défaut ; --steer accepté, sans effet)
//!   acctl spawn <from> [task...] [--agent A] [--cwd C] [--title T]
//!               [--mode M] [--model X] [--group G]   (session agent→agent)
//!   acctl events <session> [before_seq] [limit]
//!   acctl tail <session> [after_seq]
//!   acctl resume [--fresh] <session> [native_id]
//!       (resume = respawn + session/load ; --fresh = abandonne le
//!        contexte natif → session/new, journal conservé — utile quand
//!        le replay d'une très longue session native est trop long)
//!   acctl cancel|kill|clear|sleep <session>
//!       (sleep = libère le process, la session reste — réveil auto au
//!        prochain envoi)
//!   acctl mode <session> <mode>
//!   acctl respond <session> <request_id> [option_id | --answer JSON]
//!   acctl blob <session> <blob> [offset] [limit]
//!   acctl download <session> <path> [dest]     (dossier → zip auto)
//!   acctl pr [ls] [--all]            — registre PR ↔ session du démon
//!   acctl pr add <N|I<cid>> [--session S] [--repo R]
//!       [--forge github|gitlab|gerrit] [--host H[:port]] [--note txt]
//!   acctl pr note <N|I<cid>> <texte...> | acctl pr rm <N|I<cid>> | acctl pr sync
//!   acctl pr review <N> [--repo R]       — review native (pr_review du
//!       démon) : même pipeline que `/mr review`, verdict notifié en
//!       message si lancé depuis une session agent
//!   acctl steps set [--session S] "a:todo,b:wip,…" | steps get [--session S]
//!                                     — étapes de la tâche (skill ac-steps ;
//!                                     session = $AGENTCONTROL_SESSION sinon)
//!   acctl comp [ls] [--json] | info <n> | start|stop|restart <n>
//!       | logs <n> [--tail N] | catalog [--json] [--refresh]
//!       | install <n> | add <n> [--kind K] [--cmd "<argv>"] [--hc S]
//!       [--env K=V ...] [--cwd C] [--desc D] [--vargv "<argv>"]
//!       [--vmin V] [--autostart] | rm <n>
//!                                     — composants managés du démon
//!   acctl orch [on|off|status|config|set <clé> <val>…]
//!       | metrics [--window 24h|7d|30d|all]
//!                                     — orchestrateur backlog natif
//!                                     (on|off : bascule runtime ;
//!                                     config/set : édition persistée ;
//!                                     metrics : série + agrégats)
//!
//!   acctl token rotate                    — régénère daemon.token et
//!       révoque les connexions TCP ouvertes (ré-auth au reconnect ;
//!       le jeton se relit dans le fichier — jamais affiché par RPC)
//!
//! Connexion : $AGENTCONTROL_SOCK (défaut ~/.agentcontrol/daemon.sock)
//!             ou $AGENTCONTROL_TCP host:port — port TCP authentifié :
//!             jeton pris dans $AGENTCONTROL_TOKEN, sinon relu dans
//!             ${AGENTCONTROL_HOME:-$HOME/.agentcontrol}/daemon.token
//!             (le fichier vit sur la machine du démon — acctl doit
//!             donc tourner dessus, ou l'env doit porter le jeton)

mod comp;
mod pr;

use anyhow::{bail, Context, Result};
use proto::Request;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

pub(crate) struct Conn {
    w: Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
    lines: tokio::io::Lines<BufReader<Box<dyn tokio::io::AsyncRead + Unpin + Send>>>,
    next_id: u64,
}

/// Chemin du `daemon.token` local : ${AGENTCONTROL_HOME:-$HOME/.agentcontrol}/daemon.token.
fn daemon_token_path() -> std::path::PathBuf {
    let root = std::env::var("AGENTCONTROL_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                .join(".agentcontrol")
        });
    root.join("daemon.token")
}

/// Handshake d'ouverture TCP : `auth` en 1re ligne quand un jeton est
/// disponible (le démon récent l'exige ; un vieux démon répond
/// « unknown variant `auth` » → mode pré-auth, la conn reste ouverte).
/// Sans jeton, on laisse le premier appel recevoir « auth-required »
/// — l'erreur porte alors la piste de résolution.
async fn tcp_auth(c: &mut Conn) -> Result<()> {
    let token = std::env::var("AGENTCONTROL_TOKEN")
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .or_else(|| {
            std::fs::read_to_string(daemon_token_path())
                .ok()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
        });
    let Some(token) = token else { return Ok(()) };
    match c.call(proto::ReqKind::Auth { token }).await {
        Ok(_) => Ok(()),
        Err(e) => {
            let msg = format!("{e:#}");
            if msg.contains("unknown variant `auth`") {
                // Démon pré-B1 : pas d'auth — la conn reste utilisable.
                return Ok(());
            }
            Err(e).context("tcp auth refused — check AGENTCONTROL_TOKEN or the remote daemon.token")
        }
    }
}

async fn connect() -> Result<Conn> {
    if let Ok(addr) = std::env::var("AGENTCONTROL_TCP") {
        let s = TcpStream::connect(&addr)
            .await
            .with_context(|| format!("tcp {addr}"))?;
        let (r, w) = s.into_split();
        let mut c = Conn {
            w: Box::new(w),
            lines: BufReader::new(Box::new(r) as Box<dyn tokio::io::AsyncRead + Unpin + Send>)
                .lines(),
            next_id: 1,
        };
        tcp_auth(&mut c).await?;
        return Ok(c);
    }
    #[cfg(unix)]
    {
        let path = std::env::var("AGENTCONTROL_SOCK").unwrap_or_else(|_| {
            format!(
                "{}/.agentcontrol/daemon.sock",
                std::env::var("HOME").unwrap_or_else(|_| ".".into())
            )
        });
        let s = tokio::net::UnixStream::connect(&path)
            .await
            .with_context(|| format!("unix {path} — is the daemon running?"))?;
        let (r, w) = s.into_split();
        Ok(Conn {
            w: Box::new(w),
            lines: BufReader::new(Box::new(r) as Box<dyn tokio::io::AsyncRead + Unpin + Send>)
                .lines(),
            next_id: 1,
        })
    }
    #[cfg(not(unix))]
    {
        bail!("no unix socket here — use AGENTCONTROL_TCP=host:port")
    }
}

impl Conn {
    pub(crate) async fn call(&mut self, req: proto::ReqKind) -> Result<Value> {
        let r = Request {
            id: self.next_id,
            req,
        };
        self.next_id += 1;
        let mut s = serde_json::to_string(&r)?;
        s.push('\n');
        self.w.write_all(s.as_bytes()).await?;
        self.w.flush().await?;
        let line = self.lines.next_line().await?.context("connection closed")?;
        let resp: proto::Response = serde_json::from_str(&line)?;
        if resp.ok {
            Ok(resp.result.unwrap_or(Value::Null))
        } else {
            let err = resp.error.unwrap_or_else(|| "?".into());
            if err == proto::ERR_AUTH_REQUIRED {
                // Le démon ferme la conn après cette réponse — le jeton
                // manquait ou était faux (auth skip sans jeton, ou
                // rotation entre-temps).
                bail!(
                    "{err} — token expected on the first line (AGENTCONTROL_TOKEN ou {})",
                    daemon_token_path().display()
                );
            }
            bail!("{err}")
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

/// Badge compact `#N`/`!N`/`c#N` — partagé avec le client.
fn badge_of(e: &Value) -> String {
    pr::badge_of(e)
}

fn print_event(ev: &Value) {
    let seq = ev["seq"].as_u64().unwrap_or(0);
    let ty = ev["type"].as_str().unwrap_or("?");
    match ty {
        "user_message" => println!(
            "\x1b[36m[{seq}] vous{}{}> {}\x1b[0m",
            if ev["steer"].as_bool().unwrap_or(false) {
                " (steer)"
            } else {
                ""
            },
            ev["origin"]
                .as_str()
                .map(|o| format!(" (via {o})"))
                .unwrap_or_default(),
            ev["text"].as_str().unwrap_or("")
        ),
        "peer_message" => println!(
            "\x1b[35m[{seq}] {}→{} («{}»→«{}»)> {}\x1b[0m",
            ev["from_session"].as_str().unwrap_or("?"),
            ev["to_session"].as_str().unwrap_or("?"),
            ev["from_title"].as_str().unwrap_or("?"),
            ev["to_title"].as_str().unwrap_or("?"),
            ev["text"].as_str().unwrap_or("")
        ),
        "agent_text" => println!("[{seq}] {}", ev["text"].as_str().unwrap_or("")),
        "agent_thought" => println!(
            "\x1b[90m[{seq}] (thought) {}\x1b[0m",
            ev["text"].as_str().unwrap_or("")
        ),
        "tool_call" | "tool_call_update" => {
            let title = ev["title"]
                .as_str()
                .unwrap_or_else(|| ev["call_id"].as_str().unwrap_or("?"));
            let status = ev["status"].as_str().unwrap_or("?");
            println!("\x1b[33m[{seq}] ⚙ {title} — {status}\x1b[0m");
            if let Some(x) = ev["excerpt"].as_str() {
                for l in x.lines().take(5) {
                    println!("    \x1b[90m{l}\x1b[0m");
                }
            }
            if let Some(b) = ev["blob"].as_str() {
                println!(
                    "    \x1b[90m[full output: {b}, {} B]\x1b[0m",
                    ev["size"].as_u64().unwrap_or(0)
                );
            }
        }
        "permission_request" => {
            println!(
                "\x1b[31m[{seq}] PERMISSION {}\x1b[0m",
                ev["title"].as_str().unwrap_or("")
            );
            if let Some(d) = ev["detail"].as_str() {
                for l in d.lines().take(10) {
                    println!("    \x1b[90m{l}\x1b[0m");
                }
            }
            for o in ev["options"]
                .as_array()
                .map(|a| a.as_slice())
                .unwrap_or(&[])
            {
                println!(
                    "    - {} ({})",
                    o["option_id"].as_str().unwrap_or(""),
                    o["name"].as_str().unwrap_or("")
                );
            }
            println!(
                "    → acctl respond <session> {} <option_id>",
                ev["request_id"].as_str().unwrap_or("")
            );
        }
        "question_request" => {
            println!(
                "\x1b[35m[{seq}] QUESTION {}\x1b[0m",
                ev["message"].as_str().unwrap_or("")
            );
            println!(
                "    → acctl respond <session> {} --answer '{{...}}'",
                ev["request_id"].as_str().unwrap_or("")
            );
        }
        "permission_resolved" | "question_resolved" => {
            println!(
                "[{seq}] resolved: {}",
                ev["outcome"]
                    .as_str()
                    .or_else(|| ev["answer"].as_str())
                    .unwrap_or("")
            );
        }
        "mode_changed" => println!("[{seq}] mode → {}", ev["mode"].as_str().unwrap_or("")),
        "commands_available" => {
            let names: Vec<String> = ev["commands"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|c| {
                            format!(
                                "/{}",
                                c.get("name")
                                    .and_then(Value::as_str)
                                    .or_else(|| c.as_str())
                                    .unwrap_or("")
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            println!("[{seq}] commandes: {}", names.join(" "));
        }
        "status_changed" => println!(
            "\x1b[90m[{seq}] state → {}\x1b[0m",
            ev["state"].as_str().unwrap_or("")
        ),
        "steps" => {
            let n = ev["steps"].as_array().map(|a| a.len()).unwrap_or(0);
            let cur = ev["steps"].as_array().and_then(|a| {
                a.iter()
                    .find(|s| s["status"].as_str() == Some("in_progress"))
            });
            println!(
                "\x1b[36m[{seq}] ▤ steps: {n}{}\x1b[0m",
                cur.and_then(|s| s["name"].as_str())
                    .map(|n| format!(" — en cours: {n}"))
                    .unwrap_or_default()
            );
        }
        "session_cleared" => println!("\x1b[90m[{seq}] ── context cleared ──\x1b[0m"),
        "error" => println!(
            "\x1b[31m[{seq}] ERREUR {}\x1b[0m",
            ev["message"].as_str().unwrap_or("")
        ),
        "system" => println!(
            "\x1b[90m[{seq}] ⦿ {}\x1b[0m",
            ev["text"].as_str().unwrap_or("")
        ),
        _ => println!("[{seq}] {ty} {ev}"),
    }
}

/// Commandes top-level canoniques — source de la suggestion
/// « vouliez-vous dire … » sur nom inconnu (les alias wr/dl/
/// components/orchestrator restent valides mais ne sont pas
/// suggérés : la forme canonique est à privilégier).
const COMMANDS: &[&str] = &[
    "status",
    "daemon-update",
    "agents",
    "list",
    "new",
    "send",
    "msg",
    "spawn",
    "events",
    "tail",
    "search",
    "find",
    "create",
    "resume",
    "attach",
    "fork",
    "cancel",
    "kill",
    "clear",
    "sleep",
    "rm",
    "mode",
    "model",
    "setcfg",
    "steps",
    "respond",
    "blob",
    "download",
    "write",
    "pr",
    "comp",
    "orch",
    "token",
];

/// Distance d'édition de Levenshtein (deux rangées, noms courts).
fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j + 1] + 1)
                .min(cur[j] + 1)
                .min(prev[j] + usize::from(ca != cb));
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Commande la plus proche de `input`, si elle est assez proche :
/// ≤ 2 fautes (ex. transposition « daemon-uptate ») ou ≤ 1/3 de la
/// longueur pour les noms longs — en dessous on préfère ne rien
/// suggérer plutôt qu'une piste trompeuse.
fn suggest_command(input: &str) -> Option<&'static str> {
    // Un flag (« -v ») n'est pas une commande — ne rien suggérer.
    if input.starts_with('-') {
        return None;
    }
    let mut best: Option<(&'static str, usize)> = None;
    for &cmd in COMMANDS {
        let d = edit_distance(input, cmd);
        if best.is_none_or(|(_, bd)| d < bd) {
            best = Some((cmd, d));
        }
    }
    let (cmd, d) = best?;
    let len = input.chars().count().max(cmd.len());
    (d <= 2 || d * 3 <= len).then_some(cmd)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: acctl status|agents|list|new|send|msg|spawn|events|tail|search|find|create|resume|attach|fork|cancel|kill|clear|sleep|mode|respond|blob|download|write|pr|comp|orch|steps|token|daemon-update");
        std::process::exit(2);
    }
    let mut c = connect().await?;
    match args[0].as_str() {
        "status" => println!("{}", c.call(proto::ReqKind::DaemonStatus).await?),
        // Auto-update du démon vers la version passée (défaut : "latest"
        // = dernière release publiée, résolue par le démon — la version
        // locale peut n'exister qu'en build de branche → 404).
        // Le démon se remplace puis est relancé par son superviseur ;
        // la connexion peut couper juste après la réponse.
        "daemon-update" => {
            let version = args.get(1).cloned().unwrap_or_else(|| "latest".into());
            println!(
                "{}",
                c.call(proto::ReqKind::UpdateDaemon { version }).await?
            );
        }
        // Rotation du jeton TCP : nouveau daemon.token côté démon +
        // révocation des conns ouvertes. Un vieux démon répond
        // « unknown variant `token_rotate` » — erreur claire.
        "token" => match args.get(1).map(String::as_str) {
            Some("rotate") => println!("{}", c.call(proto::ReqKind::TokenRotate).await?),
            _ => {
                eprintln!("usage: acctl token rotate");
                std::process::exit(2);
            }
        },
        "agents" => {
            // `agents install <id>` : provisioning de l'adaptateur ACP
            // côté démon (npm i -g déclaré par le registre — #277) ;
            // sans argument → liste des profils comme avant.
            if args.get(1).map(String::as_str) == Some("install") {
                let id = args.get(2).context("usage: acctl agents install <id>")?;
                println!(
                    "{}",
                    c.call(proto::ReqKind::InstallAgentAdapter { agent: id.clone() })
                        .await?
                );
            } else {
                println!("{}", c.call(proto::ReqKind::ListAgents).await?);
            }
        }
        "list" => {
            let v = c.call(proto::ReqKind::ListSessions).await?;
            // Registre PR (tolère un vieux démon qui n'a pas pr_list).
            let prs = c
                .call(proto::ReqKind::PrList { session: None })
                .await
                .ok()
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default();
            let rows: Vec<&Value> = v.as_array().map(|a| a.iter().collect()).unwrap_or_default();
            // Sous-agents : spawned_by pointant une ligne de la liste →
            // la session s'imprime sous sa créatrice, indentée (↳,
            // estompée). Les orphelines restent racines.
            // id vide exclu : sinon un spawned_by "" s'imbriquerait sous
            // une ligne sans identité, et `seen` les dédoublonnerait à tort.
            let ids: std::collections::HashSet<&str> = rows
                .iter()
                .filter_map(|s| s["id"].as_str())
                .filter(|i| !i.is_empty())
                .collect();
            let mut children: std::collections::HashMap<&str, Vec<&Value>> = Default::default();
            let mut roots: Vec<&Value> = Vec::new();
            for s in &rows {
                match s["spawned_by"].as_str().filter(|p| ids.contains(p)) {
                    Some(p) => children.entry(p).or_default().push(*s),
                    None => roots.push(*s),
                }
            }
            let mut seen = std::collections::HashSet::new();
            // `rows` en second passage : un cycle spawned_by (meta
            // corrompu) réinjecte ses membres en racines plutôt que
            // de les faire disparaître.
            for seed in roots.iter().chain(rows.iter()) {
                let mut stack = vec![(*seed, 0usize)];
                while let Some((s, depth)) = stack.pop() {
                    let sid = s["id"].as_str().unwrap_or("");
                    // Dédoublonnage par identité de ligne : une ligne sans
                    // id s'imprime quand même, et un cycle repassant par la
                    // même entrée de `rows` est bloqué.
                    if !seen.insert(s as *const Value) {
                        continue;
                    }
                    let badge: Vec<String> = prs
                        .iter()
                        .filter(|p| {
                            p["session"].as_str() == Some(sid)
                                && !matches!(p["status"].as_str(), Some("merged" | "closed"))
                                // « non résoluble » = probable fantôme :
                                // pas de badge de session (onglet seul).
                                && (p["sync_fails"].as_u64().unwrap_or(0)
                                    < proto::PR_UNRESOLVED_AFTER as u64)
                        })
                        .map(|p| {
                            let st = p["status"].as_str().unwrap_or("");
                            format!(
                                "{}{}",
                                badge_of(p),
                                if st.is_empty() || st == "open" {
                                    String::new()
                                } else {
                                    format!("({st})")
                                }
                            )
                        })
                        .collect();
                    // Cap d'indentation comme la sidebar (4 niveaux puis
                    // retrait fixe) — les arbres très profonds restent
                    // lisibles dans un terminal étroit.
                    let indent = if depth == 0 {
                        String::new()
                    } else {
                        format!("{}↳ ", "  ".repeat((depth - 1).min(4)))
                    };
                    // État affiché — « error » porte le kind en
                    // suffixe (error:rate_limit…) pour distinguer un
                    // retry programmé d'une panne à réparer.
                    let state = match s["state"].as_str().unwrap_or("") {
                        "error" => match s["last_error"]["kind"].as_str() {
                            Some(k) => format!("error:{k}"),
                            None => "error".to_string(),
                        },
                        st => st.to_string(),
                    };
                    let line = format!(
                        "{}{}  {:<12} {:<14} {}  {}{}{}",
                        indent,
                        sid,
                        state,
                        s["agent"].as_str().unwrap_or(""),
                        s["created_at"]
                            .as_str()
                            .unwrap_or("")
                            .get(..16)
                            .unwrap_or(""),
                        s["title"].as_str().unwrap_or(""),
                        if badge.is_empty() {
                            String::new()
                        } else {
                            format!("  [{}]", badge.join(" "))
                        },
                        // Provenance service (meta.origin) — même format
                        // « [via X] » que les badges PR, ignoré du titre.
                        s["origin"]
                            .as_str()
                            .map(|o| format!("  [via {o}]"))
                            .unwrap_or_default()
                    );
                    if depth == 0 {
                        println!("{line}");
                    } else {
                        println!("\x1b[2m{line}\x1b[0m");
                    }
                    if let Some(kids) = children.get(sid) {
                        for c in kids.iter().rev() {
                            stack.push((*c, depth + 1));
                        }
                    }
                }
            }
        }
        "new" => {
            // acctl new <agent> <cwd> [prompt...] [--title T] [--mode M]
            //             [--model X] [--origin O] [--group G]
            if args.len() < 3 {
                bail!("new <agent> <cwd> [prompt...]");
            }
            let agent = args[1].clone();
            let cwd = args[2].clone();
            let mut prompt_parts = Vec::new();
            let mut title = None;
            let mut mode = None;
            let mut model = None;
            let mut origin = None;
            let mut group = None;
            let mut i = 3;
            while i < args.len() {
                match args[i].as_str() {
                    "--title" => {
                        title = args.get(i + 1).cloned();
                        i += 2;
                    }
                    "--mode" => {
                        mode = args.get(i + 1).cloned();
                        i += 2;
                    }
                    "--model" => {
                        model = args.get(i + 1).cloned();
                        i += 2;
                    }
                    "--origin" => {
                        origin = args.get(i + 1).cloned();
                        i += 2;
                    }
                    "--group" => {
                        group = args.get(i + 1).cloned();
                        i += 2;
                    }
                    p => {
                        prompt_parts.push(p.to_string());
                        i += 1;
                    }
                }
            }
            let v = c
                .call(proto::ReqKind::NewSession {
                    agent,
                    cwd,
                    title,
                    model,
                    mode,
                    origin,
                    group,
                    prompt: if prompt_parts.is_empty() {
                        None
                    } else {
                        Some(prompt_parts.join(" "))
                    },
                })
                .await?;
            println!("{}", v["session"].as_str().unwrap_or(""));
        }
        "send" => {
            // acctl send <session> [--steer|--force] <text...>
            if args.len() < 3 {
                bail!("send <session> [--steer|--force] <text...>");
            }
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
            })
            .await?;
        }
        "msg" => {
            // acctl msg <from> <to> [--queue|--steer] <text...>
            // steer est le défaut agent→agent — --steer reste accepté
            // (compat scripts) mais n'a plus d'effet. Flags en tête
            // seulement : « --queue » dans le corps reste du texte.
            if args.len() < 4 {
                bail!("msg <from> <to> [--queue|--steer] <text...>");
            }
            let mut queue = false;
            let mut i = 3;
            while i < args.len() {
                match args[i].as_str() {
                    "--steer" => i += 1,
                    "--queue" => {
                        queue = true;
                        i += 1;
                    }
                    _ => break,
                }
            }
            if i >= args.len() {
                bail!("msg <from> <to> [--queue|--steer] <text...>");
            }
            println!(
                "{}",
                c.call(proto::ReqKind::PeerSend {
                    from: args[1].clone(),
                    session: args[2].clone(),
                    text: args[i..].join(" "),
                    delivery: if queue {
                        "queue".into()
                    } else {
                        "steer".into()
                    },
                })
                .await?
            );
        }
        "spawn" => {
            // acctl spawn <from> [task...] [--agent A] [--cwd C] [--title T]
            //             [--mode M] [--model X] [--group G]
            if args.len() < 2 {
                bail!("spawn <from> [task...] [--agent A] [--cwd C] [--title T] [--mode M] [--model X] [--group G]");
            }
            let mut prompt_parts = Vec::new();
            let (mut agent, mut cwd, mut title, mut mode, mut model, mut group) =
                (None, None, None, None, None, None);
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--agent" => {
                        agent = args.get(i + 1).cloned();
                        i += 2;
                    }
                    "--cwd" => {
                        cwd = args.get(i + 1).cloned();
                        i += 2;
                    }
                    "--title" => {
                        title = args.get(i + 1).cloned();
                        i += 2;
                    }
                    "--mode" => {
                        mode = args.get(i + 1).cloned();
                        i += 2;
                    }
                    "--model" => {
                        model = args.get(i + 1).cloned();
                        i += 2;
                    }
                    "--group" => {
                        group = args.get(i + 1).cloned();
                        i += 2;
                    }
                    p => {
                        prompt_parts.push(p.to_string());
                        i += 1;
                    }
                }
            }
            println!(
                "{}",
                c.call(proto::ReqKind::PeerSpawn {
                    from: args[1].clone(),
                    agent,
                    cwd,
                    title,
                    model,
                    mode,
                    prompt: if prompt_parts.is_empty() {
                        None
                    } else {
                        Some(prompt_parts.join(" "))
                    },
                    group,
                })
                .await?
            );
        }
        "events" => {
            let before = args
                .get(2)
                .and_then(|s| s.parse().ok())
                .filter(|n: &u64| *n > 0);
            let limit = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(200);
            let v = c
                .call(proto::ReqKind::Events {
                    session: args[1].clone(),
                    before_seq: before,
                    after_seq: None,
                    limit,
                })
                .await?;
            for ev in v["events"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
                print_event(ev);
            }
            eprintln!("(last_seq={})", v["last_seq"].as_u64().unwrap_or(0));
        }
        "tail" => {
            let after = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
            c.tail(&args[1], after).await?;
        }
        "search" => {
            // acctl search <session> <query...> — scan plein-texte du
            // journal, plus récent d'abord (curseur non exposé pour
            // l'instant : un appel couvre ~20k events scannés).
            if args.len() < 3 {
                bail!("search <session> <query...>");
            }
            let v = c
                .call(proto::ReqKind::SearchEvents {
                    session: args[1].clone(),
                    query: args[2..].join(" "),
                    before_seq: None,
                    limit: 50,
                })
                .await?;
            for m in v["matches"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
                println!(
                    "[{}] {} {}",
                    m["seq"].as_u64().unwrap_or(0),
                    m["type"].as_str().unwrap_or(""),
                    m["excerpt"].as_str().unwrap_or("")
                );
            }
            if !v["done"].as_bool().unwrap_or(true) {
                eprintln!(
                    "(partial scan — {} events scanned)",
                    v["scanned"].as_u64().unwrap_or(0)
                );
            }
        }
        "find" => {
            // acctl find <session> <query...> — recherche par nom de
            // fichier dans le workspace (récursif, insensible à la casse).
            if args.len() < 3 {
                bail!("find <session> <query...>");
            }
            let v = c
                .call(proto::ReqKind::FindFiles {
                    session: args[1].clone(),
                    path: String::new(),
                    query: args[2..].join(" "),
                    limit: None,
                })
                .await?;
            for m in v["matches"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
                println!(
                    "{}{}",
                    m["path"].as_str().unwrap_or(""),
                    if m["dir"].as_bool().unwrap_or(false) {
                        "/"
                    } else {
                        ""
                    },
                );
            }
            if v["truncated"].as_bool().unwrap_or(false) {
                eprintln!(
                    "(truncated results — {} entries scanned)",
                    v["scanned"].as_u64().unwrap_or(0)
                );
            }
        }
        "create" => {
            // acctl create <session> <path> [--dir] — fichier vide ou
            // dossier (création exclusive : échoue si la cible existe).
            if args.len() < 3 {
                bail!("create <session> <path> [--dir]");
            }
            let dir = args[3..].iter().any(|a| a == "--dir" || a == "-d");
            let v = c
                .call(proto::ReqKind::CreatePath {
                    session: args[1].clone(),
                    path: args[2].clone(),
                    dir,
                })
                .await?;
            println!(
                "{}{}",
                v["path"].as_str().unwrap_or(""),
                if dir { "/" } else { "" },
            );
        }
        "resume" => {
            // acctl resume [--fresh] <session> [native_id] — --fresh
            // abandonne le contexte natif (session/new, journal conservé).
            let mut fresh = false;
            let mut pos: Vec<String> = Vec::new();
            for a in args.iter().skip(1) {
                if a == "--fresh" {
                    fresh = true;
                } else {
                    pos.push(a.clone());
                }
            }
            if pos.is_empty() {
                bail!("resume [--fresh] <session> [native_id]");
            }
            println!(
                "{}",
                c.call(proto::ReqKind::ResumeSession {
                    session: pos[0].clone(),
                    native_id: pos.get(1).cloned(),
                    fresh,
                })
                .await?
            )
        }
        "attach" => println!(
            "{}",
            c.call(proto::ReqKind::AttachSession {
                agent: args[1].clone(),
                cwd: args[2].clone(),
                native_id: args[3].clone(),
                title: args.get(4).cloned(),
            })
            .await?
        ),
        "fork" => println!(
            "{}",
            c.call(proto::ReqKind::ForkSession {
                session: args[1].clone(),
            })
            .await?
        ),
        "cancel" => println!(
            "{}",
            c.call(proto::ReqKind::Cancel {
                session: args[1].clone()
            })
            .await?
        ),
        "kill" => println!(
            "{}",
            c.call(proto::ReqKind::KillSession {
                session: args[1].clone()
            })
            .await?
        ),
        "clear" => println!(
            "{}",
            c.call(proto::ReqKind::ClearSession {
                session: args[1].clone()
            })
            .await?
        ),
        "sleep" => println!(
            "{}",
            c.call(proto::ReqKind::SleepSession {
                session: args[1].clone()
            })
            .await?
        ),
        "rm" => println!(
            "{}",
            c.call(proto::ReqKind::DeleteSession {
                session: args[1].clone()
            })
            .await?
        ),
        "mode" => println!(
            "{}",
            c.call(proto::ReqKind::SetMode {
                session: args[1].clone(),
                mode: args[2].clone()
            })
            .await?
        ),
        "model" => println!(
            "{}",
            c.call(proto::ReqKind::SetModel {
                session: args[1].clone(),
                model: args[2].clone()
            })
            .await?
        ),
        "setcfg" => println!(
            "{}",
            c.call(proto::ReqKind::SetConfigOption {
                session: args[1].clone(),
                config_id: args[2].clone(),
                value: args[3].clone(),
            })
            .await?
        ),
        "steps" => {
            // acctl steps set [--session S] <spec>   — spec = "a:todo,b:wip,…"
            //   (statuts todo|in_progress|wip|done|abandoned|abandon) ou JSON
            //   '[{"name":..,"status":..}]' ; "-" ou "[]" vide la liste.
            // acctl steps get [--session S]  — liste courante.
            // Session = --session ou $AGENTCONTROL_SESSION.
            if args.len() < 2 {
                bail!("steps set [--session S] <spec> | steps get [--session S]");
            }
            let mut session = None;
            let mut rest: Vec<String> = Vec::new();
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--session" => {
                        session = args.get(i + 1).cloned();
                        i += 2;
                    }
                    _ => {
                        rest.push(args[i].clone());
                        i += 1;
                    }
                }
            }
            let session = session
                .or_else(|| std::env::var("AGENTCONTROL_SESSION").ok())
                .context("no session: --session S or AGENTCONTROL_SESSION")?;
            match args[1].as_str() {
                "set" => {
                    let spec = rest.join(" ");
                    if spec.is_empty() {
                        bail!("steps set [--session S] <spec>");
                    }
                    let steps = parse_steps_spec(&spec)?;
                    let v = c
                        .call(proto::ReqKind::SessionSetSteps { session, steps })
                        .await?;
                    println!("{}", serde_json::to_string_pretty(&v)?);
                }
                "get" => {
                    // Pas de req dédiée : session_info porte `steps`.
                    let v = c.call(proto::ReqKind::SessionInfo { session }).await?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&v["steps"]).unwrap_or_else(|_| "null".into())
                    );
                }
                other => bail!("steps: unknown sub-command \"{other}\" (set|get)"),
            }
        }
        "respond" => {
            // respond <session> <request_id> [option_id | --answer JSON]
            let mut option_id = None;
            let mut answer = None;
            if args.len() > 3 {
                if args[3] == "--answer" {
                    answer = Some(serde_json::from_str(
                        args.get(4).map(String::as_str).unwrap_or("null"),
                    )?);
                } else {
                    option_id = Some(args[3].clone());
                }
            }
            println!(
                "{}",
                c.call(proto::ReqKind::Respond {
                    session: args[1].clone(),
                    request_id: args[2].clone(),
                    option_id,
                    answer,
                })
                .await?
            );
        }
        "blob" => {
            let offset = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
            let limit = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(65536);
            let v = c
                .call(proto::ReqKind::GetBlob {
                    session: args[1].clone(),
                    blob: args[2].clone(),
                    offset,
                    limit,
                })
                .await?;
            print!("{}", v["data"].as_str().unwrap_or(""));
            eprintln!("(total={} offset={})", v["total"], v["offset"]);
        }
        "download" | "dl" => {
            // acctl download <session> <path> [dest] — dossier → zip côté démon
            if args.len() < 3 {
                bail!("download <session> <path> [dest]");
            }
            let (session, path) = (args[1].clone(), args[2].clone());
            let info = c
                .call(proto::ReqKind::DownloadPath {
                    session: session.clone(),
                    path: path.clone(),
                    offset: 0,
                    limit: 0,
                    archive: None,
                })
                .await?;
            let dest = args
                .get(3)
                .cloned()
                .unwrap_or_else(|| info["name"].as_str().unwrap_or("download").to_string());
            if std::path::Path::new(&dest).is_dir() {
                bail!("destination is a directory: {dest}");
            }
            let mut out: Option<std::fs::File> = None;
            let mut archive: Option<String> = None;
            let mut offset = 0u64;
            use base64::Engine;
            use std::io::Write;
            loop {
                let v = c
                    .call(proto::ReqKind::DownloadPath {
                        session: session.clone(),
                        path: path.clone(),
                        offset,
                        limit: 1 << 20,
                        archive: archive.clone(),
                    })
                    .await?;
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
                if offset >= total {
                    break;
                }
            }
            eprintln!("\n→ {dest} ({offset} o)");
        }
        "write" | "wr" => {
            // acctl write <session> <path> [src] — src absent ou "-" = stdin.
            // Remplace tout le fichier (delete_len clampé serveur) ou le crée.
            if args.len() < 3 {
                bail!("write <session> <path> [src]");
            }
            let bytes = match args.get(3).map(String::as_str) {
                None | Some("-") => {
                    let mut b = Vec::new();
                    std::io::Read::read_to_end(&mut std::io::stdin(), &mut b)?;
                    b
                }
                Some(src) => std::fs::read(src).with_context(|| format!("source: {src}"))?,
            };
            use base64::Engine;
            let v = c
                .call(proto::ReqKind::WriteFile {
                    session: args[1].clone(),
                    path: args[2].clone(),
                    offset: 0,
                    delete_len: u64::MAX,
                    data: base64::engine::general_purpose::STANDARD.encode(&bytes),
                    if_mtime_ms: None,
                })
                .await?;
            eprintln!(
                "→ {} ({} o)",
                v["path"].as_str().unwrap_or(""),
                v["size"].as_u64().unwrap_or(0),
            );
        }
        "pr" => pr::run(&args, &mut c).await?,
        "comp" | "components" => comp::run(&args, &mut c).await?,
        // `acctl orch` — orchestrateur backlog natif : bascule runtime
        // on|off (orchestrator_set), `status` = component_info de
        // l'interne (phase, ledger workers, gardes), `config` = lecture
        // de la config éditable, `set` = patch persisté
        // (orchestrator_config), `metrics` = série + agrégats
        // (backlog_metrics).
        "orch" | "orchestrator" => {
            let sub = args.get(1).map(String::as_str).unwrap_or("status");
            match sub {
                "on" | "enable" | "start" => {
                    println!("{}", c.call(proto::ReqKind::OrchestratorSet { enabled: true }).await?);
                }
                "off" | "disable" | "stop" => {
                    println!("{}", c.call(proto::ReqKind::OrchestratorSet { enabled: false }).await?);
                }
                "status" => {
                    println!("{}", c.call(proto::ReqKind::ComponentInfo {
                        name: "backlog-orchestrator".into(),
                    }).await?);
                }
                // Série + agrégats du journal métriques : fenêtre
                // positionnelle ou `--window` (défaut 7d).
                "metrics" => {
                    let mut window = "7d".to_string();
                    let mut i = 2;
                    while i < args.len() {
                        match args[i].as_str() {
                            "--window" | "-w" => {
                                i += 1;
                                window = args
                                    .get(i)
                                    .cloned()
                                    .context("--window <24h|7d|30d|all>")?;
                            }
                            other if !other.starts_with('-') => window = other.into(),
                            other => bail!("unknown option: {other}"),
                        }
                        i += 1;
                    }
                    let v = c
                        .call(proto::ReqKind::BacklogMetrics { window: Some(window) })
                        .await?;
                    println!("{}", serde_json::to_string_pretty(&v)?);
                }
                "config" | "get" => {
                    let v = c.call(proto::ReqKind::OrchestratorConfig { patch: None }).await?;
                    println!("{}", serde_json::to_string_pretty(&v)?);
                }
                "set" => {
                    // Paires positionnelles `clé valeur` — une ou
                    // plusieurs. La valeur passe en JSON natif quand
                    // elle parse (bool/int/objet repo_workspaces)
                    // sinon en chaîne — le démon reconvertit selon la
                    // clé dans les deux cas (même validation).
                    let pairs = &args[2..];
                    if pairs.is_empty() || !pairs.len().is_multiple_of(2) {
                        bail!("usage: acctl orch set <key> <value> [<key> <value>…]");
                    }
                    let mut patch = std::collections::BTreeMap::new();
                    for kv in pairs.as_chunks::<2>().0 {
                        let v = serde_json::from_str::<Value>(&kv[1])
                            .unwrap_or_else(|_| Value::String(kv[1].clone()));
                        patch.insert(kv[0].clone(), v);
                    }
                    let v = c.call(proto::ReqKind::OrchestratorConfig {
                        patch: Some(patch),
                    }).await?;
                    println!("{}", serde_json::to_string_pretty(&v)?);
                }
                other => bail!("usage: acctl orch [on|off|status|config|set <key> <val>…|metrics [--window W]] (unknown: {other})"),
            }
        }
        other => bail!(
            "unknown command: {other}{}",
            suggest_command(other)
                .map(|c| format!(" — did you mean \"{c}\"?"))
                .unwrap_or_default()
        ),
    }
    Ok(())
}

/// Parse le spec de `acctl steps set` en Vec<SessionStep> :
///   "-" ou "[]"            → liste vide (clear)
///   '[{"name":..,"status":..}]' → JSON complet (statuts canoniques
///                               snake_case, tolère les alias)
///   "a:todo,b:wip,…"       → forme compacte, `nom:statut` séparés par
///                            des virgules ; le statut est après le
///                            DERNIER ':' (un nom peut contenir ':').
/// Alias de statut acceptés : wip|in-progress → in_progress,
/// done|complete → done, abandon|skip|skipped → abandoned.
fn parse_steps_spec(spec: &str) -> Result<Vec<proto::SessionStep>> {
    let spec = spec.trim();
    if spec == "-" || spec == "[]" {
        return Ok(Vec::new());
    }
    if spec.starts_with('[') {
        let raw: Vec<Value> = serde_json::from_str(spec)
            .context("invalid JSON spec — expected [{\"name\":..,\"status\":..}]")?;
        return raw
            .iter()
            .map(|v| {
                let name = v["name"]
                    .as_str()
                    .context("step JSON missing \"name\"")?
                    .to_string();
                let status = parse_step_status(
                    v["status"]
                        .as_str()
                        .context("step JSON missing \"status\"")?,
                )?;
                Ok(proto::SessionStep { name, status })
            })
            .collect();
    }
    spec.split(',')
        .map(|part| {
            let part = part.trim();
            if part.is_empty() {
                bail!("empty step entry in '{spec}'");
            }
            let (name, st) = part
                .rsplit_once(':')
                .with_context(|| format!("step '{part}' has no status — expected name:status"))?;
            let name = name.trim();
            if name.is_empty() {
                bail!("unnamed step in '{part}'");
            }
            Ok(proto::SessionStep {
                name: name.to_string(),
                status: parse_step_status(st.trim())?,
            })
        })
        .collect()
}

fn parse_step_status(s: &str) -> Result<proto::StepStatus> {
    match s.trim().to_ascii_lowercase().as_str() {
        "todo" | "to_do" => Ok(proto::StepStatus::Todo),
        "in_progress" | "in-progress" | "wip" | "progress" | "doing" => {
            Ok(proto::StepStatus::InProgress)
        }
        "done" | "complete" | "completed" | "finished" => Ok(proto::StepStatus::Done),
        "abandoned" | "abandon" | "skip" | "skipped" => Ok(proto::StepStatus::Abandoned),
        other => bail!("unknown step status '{other}' — expected todo|in_progress|done|abandoned"),
    }
}
