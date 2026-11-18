//! Sonde de connexion — repro/validation de la télémétrie de mort (#128).
//!
//!   conn_probe ssh <host> [-- <args ssh…>]     ex : ssh dev@127.0.0.1 -- -p 2299 -i /tmp/k
//!   conn_probe native <user@host[:port]> [-k <clé>]
//!   conn_probe tcp <addr>
//!
//! Poll `daemon_status` toutes les 5 s comme le watchdog de l'app ;
//! à la mort du transport, affiche la cause post-mortem sur stdout
//! (le détail complet part aussi dans ~/.agentcontrol/logs/client.log).

use anyhow::{Context, Result};
use client_core::{Client, Transport};
use proto::ReqKind;
use std::path::PathBuf;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("");
    let t = match mode {
        "ssh" => {
            let host = args
                .get(1)
                .context("usage: conn_probe ssh <host> [-- args…]")?
                .clone();
            let extra = match args.iter().position(|a| a == "--") {
                Some(i) => args[i + 1..].to_vec(),
                None => Vec::new(),
            };
            Transport::Ssh {
                host,
                remote_port: 7331,
                args: extra,
            }
        }
        "native" => {
            let auth = args
                .get(1)
                .context("usage: conn_probe native <user@host[:port]> [-k clé]")?
                .clone();
            let (auth, port) = match auth.rsplit_once(':') {
                Some((a, p)) if p.chars().all(|c| c.is_ascii_digit()) => {
                    (a.to_string(), p.parse::<u16>()?)
                }
                _ => (auth, 22),
            };
            let (user, host) = auth
                .split_once('@')
                .map(|(u, h)| (u.to_string(), h.to_string()))
                .unwrap_or_else(|| {
                    (std::env::var("USER").unwrap_or_else(|_| "dev".into()), auth)
                });
            let key_path = args
                .iter()
                .position(|a| a == "-k")
                .and_then(|i| args.get(i + 1))
                .map(PathBuf::from);
            Transport::SshNative {
                host,
                port,
                user,
                remote_port: 7331,
                password: None,
                key_path,
                expected_hk: None,
            }
        }
        "tcp" | "close" => Transport::Tcp(
            args.get(1)
                .context("usage: conn_probe tcp|close <addr>")?
                .clone(),
        ),
        _ => {
            eprintln!("usage: conn_probe ssh <host> [-- args] | native <u@h[:p]> [-k clé] | tcp <addr> | close <addr>");
            std::process::exit(2);
        }
    };
    let c = Client::connect(&t).await?;
    if mode == "close" {
        // Vérifie que close() solde les appels et libère le transport.
        c.call(ReqKind::DaemonStatus).await?;
        c.close();
        println!("close(): dead={} why={:?}", *c.dead.borrow(), c.death_reason());
        match c.call(ReqKind::DaemonStatus).await {
            Err(e) => println!("call post-close: {e:#} (attendu)"),
            Ok(_) => println!("call post-close: réponse inattendue"),
        }
        return Ok(());
    }
    eprintln!("connecté — daemon_status toutes les 5 s, Ctrl-C pour sortir");
    let mut dead = c.dead.clone();
    loop {
        tokio::select! {
            r = dead.wait_for(|d| *d) => {
                r.ok();
                println!(
                    "DEAD: {}",
                    c.death_reason().unwrap_or_else(|| "?".into())
                );
                return Ok(());
            }
            r = c.call(ReqKind::DaemonStatus) => match r {
                Ok(v) => println!(
                    "status ok (version {})",
                    v.get("version").and_then(|x| x.as_str()).unwrap_or("?")
                ),
                Err(e) => println!("status err: {e:#}"),
            },
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}
