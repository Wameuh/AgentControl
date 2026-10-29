//! `acctl comp` (alias `components`) — composants managés du démon
//! (docs/components-integration.md §2.4) : ls / info / start / stop /
//! restart / logs / catalog / install / add / rm. Miroir terminal des
//! RPC `Component*` pour agents, scripts et l'orchestrateur lui-même.

use crate::Conn;
use anyhow::{bail, Context, Result};
use serde_json::Value;

/// Valeur d'un flag `--xxx` (None si absent) — même helper que pr.rs.
fn flag_val<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).map(String::as_str))
}

/// Toutes les occurrences d'un flag à paramètre (`--env K=V` répété).
fn flag_vals<'a>(args: &'a [String], name: &str) -> Vec<&'a str> {
    args.iter()
        .enumerate()
        .filter(|(_, a)| a.as_str() == name)
        .filter_map(|(i, _)| args.get(i + 1).map(String::as_str))
        .collect()
}

/// Uptime compacte : `-` sans process, sinon `42s`/`3m05s`/`2h10m`/`1d03h`.
fn fmt_uptime(secs: u64) -> String {
    match secs {
        0 => "-".into(),
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        s if s < 86400 => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
        s => format!("{}d{:02}h", s / 86400, s % 86400 / 3600),
    }
}

/// Découpe `--cmd "<argv>"` en argv : espaces séparateurs, quotes
/// simples/doubles pour grouper (mini-shell — pas d'expansion $ ni glob).
fn split_argv(s: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote = None;
    // `touched` : un arg a commencé (char ou quote ouverte) — permet
    // l'argument vide `''` et ignore les espaces de tête.
    let mut touched = false;
    for ch in s.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(ch);
                touched = true;
            }
            (None, c) if c.is_whitespace() => {
                if touched {
                    out.push(std::mem::take(&mut cur));
                    touched = false;
                }
            }
            (None, c) => {
                cur.push(c);
                touched = true;
            }
        }
    }
    anyhow::ensure!(quote.is_none(), "--cmd : quote non fermée");
    if touched {
        out.push(cur);
    }
    Ok(out)
}

/// `comp ls` — une ligne par composant : nom, kind, état, pid, uptime,
/// restarts (+ `exit=N` sur échec), description en queue — style
/// `acctl list`. `source` distingue file (components.toml) d'internal.
fn ls(items: &[Value]) {
    for it in items {
        let st = &it["status"];
        let state = st["state"].as_str().unwrap_or("?");
        let state = match st["last_exit"].as_i64().filter(|c| *c != 0) {
            Some(code) => format!("{state}(exit {code})"),
            None => state.to_string(),
        };
        let pid = st["pid"].as_u64().map(|p| p.to_string()).unwrap_or_else(|| "-".into());
        let up = fmt_uptime(st["uptime_s"].as_u64().unwrap_or(0));
        let restarts = st["restarts"].as_u64().unwrap_or(0);
        let src = match it["source"].as_str() {
            Some("internal") => " [internal]",
            _ => "",
        };
        println!(
            "{:<22} {:<8} {:<16} {:>7} {:>8} {:>4}  {}{}",
            it["name"].as_str().unwrap_or(""),
            it["kind"].as_str().unwrap_or("?"),
            state,
            pid,
            up,
            restarts,
            it["description"].as_str().unwrap_or(""),
            src,
        );
    }
}

/// `comp catalog` — nom, installed (version sondée), latest_version
/// upstream, description ; `repo` entre parenthèses quand présent.
fn catalog(entries: &[Value]) {
    for e in entries {
        let installed = if e["installed"].as_bool().unwrap_or(false) {
            e["version"]
                .as_str()
                .map(|v| format!("installé {v}"))
                .unwrap_or_else(|| "installé".into())
        } else {
            "—".into()
        };
        let latest = e["latest_version"].as_str().unwrap_or("—");
        println!(
            "{:<22} {:<28} {:<12} {}",
            e["name"].as_str().unwrap_or(""),
            installed,
            latest,
            e["description"].as_str().unwrap_or(""),
        );
        if let Some(repo) = e["repo"].as_str() {
            println!("{:>26} {}", "", repo);
        }
    }
}

/// Sonde `--hc` : `none` | `process` | `tcp:ADDR` | `http:URL` |
/// `cmd:"<argv>"` (argv quoté comme --cmd).
fn parse_healthcheck(s: &str) -> Result<proto::Healthcheck> {
    Ok(match s {
        "none" => proto::Healthcheck::None,
        "process" => proto::Healthcheck::Process,
        _ if s.starts_with("tcp:") => proto::Healthcheck::Tcp {
            addr: s["tcp:".len()..].to_string(),
        },
        _ if s.starts_with("http:") || s.starts_with("https:") => {
            proto::Healthcheck::Http { url: s.to_string() }
        }
        _ if s.starts_with("cmd:") => proto::Healthcheck::Command {
            argv: split_argv(&s["cmd:".len()..])?,
        },
        _ => bail!("--hc none|process|tcp:ADDR|http:URL|cmd:\"<argv>\" (reçu: {s})"),
    })
}

/// `comp add` — construit la `ComponentSpec` depuis les flags.
fn spec_from_args(name: &str, args: &[String]) -> Result<proto::ComponentSpec> {
    let kind = match flag_val(args, "--kind").unwrap_or("process") {
        "process" => proto::ComponentKind::Process,
        "external" => proto::ComponentKind::External,
        "tool" => proto::ComponentKind::Tool,
        "internal" => bail!("--kind internal : réservé aux composants déclarés en code"),
        other => bail!("--kind process|external|tool (reçu: {other})"),
    };
    let command = match flag_val(args, "--cmd") {
        Some(s) => split_argv(s)?,
        None => Vec::new(),
    };
    if matches!(kind, proto::ComponentKind::Process | proto::ComponentKind::Tool)
        && command.is_empty()
    {
        bail!("--cmd \"<argv>\" requis pour kind={}", match kind {
            proto::ComponentKind::Process => "process",
            _ => "tool",
        });
    }
    let healthcheck = match flag_val(args, "--hc") {
        Some(s) => parse_healthcheck(s)?,
        None => proto::Healthcheck::None,
    };
    // Mêmes gardes que check_usable/check_editable côté démon : une
    // sonde `process` exige un pid managé, et un `external` sans sonde
    // ne serait jamais sondé — inerte à vie.
    if matches!(healthcheck, proto::Healthcheck::Process)
        && kind != proto::ComponentKind::Process
    {
        bail!("--hc process exige kind=process (pid managé)");
    }
    if kind == proto::ComponentKind::External
        && matches!(healthcheck, proto::Healthcheck::None)
    {
        bail!("kind=external sans sonde resterait inerte — passez \
               --hc tcp:ADDR|http:URL|cmd:\"<argv>\"");
    }
    let mut env = std::collections::HashMap::new();
    for kv in flag_vals(args, "--env") {
        let (k, v) = kv.split_once('=').context("--env K=V attendu")?;
        anyhow::ensure!(!k.is_empty(), "--env K=V attendu (clé vide)");
        env.insert(k.to_string(), v.to_string());
    }
    Ok(proto::ComponentSpec {
        name: name.to_string(),
        kind,
        command,
        env,
        cwd: flag_val(args, "--cwd").map(String::from),
        autostart: args.iter().any(|a| a == "--autostart"),
        restart: proto::RestartPolicy::OnFailure,
        restart_sec: 5,
        healthcheck,
        description: flag_val(args, "--desc").map(String::from),
        queries: std::collections::HashMap::new(),
    })
}

/// Argument positionnel `<name>` — un flag `--x` à cette place est une
/// erreur d'usage, pas un nom.
fn name_arg<'a>(args: &'a [String], usage: &'static str) -> Result<&'a str> {
    let n = args.get(2).context(usage)?;
    anyhow::ensure!(!n.starts_with("--"), "nom de composant invalide: {n}");
    Ok(n)
}

/// `acctl comp [ls] [--json] | info <n> | start|stop|restart <n> |
/// logs <n> [--tail N] | catalog [--json] [--refresh] | install <n> |
/// add <n> [--kind K] [--cmd "<argv>"] [--hc S] [--env K=V …]
/// [--cwd C] [--desc D] [--autostart] | rm <n>`.
pub(crate) async fn run(args: &[String], c: &mut Conn) -> Result<()> {
    // Un flag à valeur en dernière position est une erreur d'usage —
    // mieux qu'une absorption silencieuse.
    const VAL_FLAGS: [&str; 7] =
        ["--kind", "--cmd", "--hc", "--env", "--cwd", "--desc", "--tail"];
    if let Some(f) = args.last().filter(|a| VAL_FLAGS.contains(&a.as_str())) {
        bail!("{f} sans valeur");
    }
    let sub = args.get(1).map(String::as_str).unwrap_or("ls");
    match sub {
        "ls" | "--json" => {
            let v = c.call(proto::ReqKind::ListComponents).await?;
            if args.iter().any(|a| a == "--json") {
                println!("{v}");
            } else {
                ls(v.as_array().map(|a| a.as_slice()).unwrap_or(&[]));
            }
        }
        "info" => {
            let name = name_arg(args, "comp info <name>")?;
            let v = c
                .call(proto::ReqKind::ComponentInfo { name: name.into() })
                .await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        "start" | "stop" | "restart" => {
            let name = name_arg(args, "comp start|stop|restart <name>")?;
            let req = match sub {
                "start" => proto::ReqKind::ComponentStart { name: name.into() },
                "stop" => proto::ReqKind::ComponentStop { name: name.into() },
                _ => proto::ReqKind::ComponentRestart { name: name.into() },
            };
            let v = c.call(req).await?;
            println!(
                "{} → {}",
                v["name"].as_str().unwrap_or(name),
                v["state"].as_str().unwrap_or("?"),
            );
        }
        "logs" => {
            let name = name_arg(args, "comp logs <name> [--tail N]")?;
            let tail: Option<usize> = flag_val(args, "--tail")
                .map(|s| s.parse().context("--tail N invalide"))
                .transpose()?;
            let v = c
                .call(proto::ReqKind::ComponentLogs {
                    name: name.into(),
                    offset: None,
                    // --tail N borne par la page serveur : le cap max
                    // (256 Kio) couvre bien plus de lignes que le défaut.
                    limit: tail.map(|_| 256 * 1024),
                })
                .await?;
            use base64::Engine;
            let data = base64::engine::general_purpose::STANDARD
                .decode(v["data"].as_str().unwrap_or(""))?;
            let text = String::from_utf8_lossy(&data);
            match tail {
                Some(n) => {
                    let lines: Vec<&str> = text.lines().collect();
                    let start = lines.len().saturating_sub(n);
                    for l in &lines[start..] {
                        println!("{l}");
                    }
                }
                None => print!("{text}"),
            }
            eprintln!(
                "(size={} offset={}{})",
                v["size"].as_u64().unwrap_or(0),
                v["offset"].as_u64().unwrap_or(0),
                if v["truncated"].as_bool().unwrap_or(false) {
                    " — tronqué en amont"
                } else {
                    ""
                },
            );
        }
        "catalog" => {
            let refresh = args.iter().any(|a| a == "--refresh");
            let v = c
                .call(proto::ReqKind::ComponentCatalog {
                    refresh: refresh.then_some(true),
                })
                .await?;
            if args.iter().any(|a| a == "--json") {
                println!("{v}");
            } else {
                catalog(v.as_array().map(|a| a.as_slice()).unwrap_or(&[]));
            }
        }
        "install" => {
            let name = name_arg(args, "comp install <name>")?;
            let v = c
                .call(proto::ReqKind::ComponentInstall { name: name.into() })
                .await?;
            println!(
                "{} — installed={} state={}",
                v["name"].as_str().unwrap_or(name),
                v["installed"].as_bool().unwrap_or(false),
                v["state"].as_str().unwrap_or("-"),
            );
            for n in v["notes"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
                if let Some(t) = n.as_str() {
                    println!("note: {t}");
                }
            }
        }
        "add" => {
            let name = name_arg(
                args,
                "comp add <name> [--kind K] [--cmd \"<argv>\"] [--hc S] \
                 [--env K=V …] [--cwd C] [--desc D] [--autostart]",
            )?;
            let spec = spec_from_args(name, &args[3..])?;
            let v = c
                .call(proto::ReqKind::ComponentUpsert {
                    name: name.into(),
                    spec,
                })
                .await?;
            println!(
                "{} → {}",
                v["name"].as_str().unwrap_or(name),
                v["state"].as_str().unwrap_or("?"),
            );
        }
        "rm" => {
            let name = name_arg(args, "comp rm <name>")?;
            c.call(proto::ReqKind::ComponentRemove { name: name.into() })
                .await?;
            println!("{name} retiré");
        }
        other => bail!(
            "comp: sous-commande inconnue: {other} \
             (ls|info|start|stop|restart|logs|catalog|install|add|rm)"
        ),
    }
    Ok(())
}
