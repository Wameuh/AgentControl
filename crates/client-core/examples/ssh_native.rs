//! Test du transport SSH natif (russh) :
//! `cargo run -p client-core --example ssh_native <user@host[:port]>`
//! Ajoutez d'abord la pubkey affichée dans authorized_keys du serveur.

use anyhow::Context;
use client_core::{ssh_native, Client, Transport};
use proto::ReqKind;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let pk = ssh_native::ensure_keypair()?;
    println!("clé: {} → {}", pk.display(), ssh_native::pubkey_line()?);

    let arg = std::env::args().nth(1).context("usage: ssh_native <user@host[:port]>")?;
    let (auth, port) = match arg.rsplit_once(':') {
        Some((a, p)) if p.chars().all(|c| c.is_ascii_digit()) => (a.to_string(), p.parse::<u16>()?),
        _ => (arg.clone(), 22),
    };
    let (user, host) = auth
        .split_once('@')
        .map(|(u, h)| (u.to_string(), h.to_string()))
        .unwrap_or_else(|| (whoami(), auth));

    let c = Client::connect(&Transport::SshNative {
        host,
        port,
        user,
        remote_port: 7331,
        password: None,
        key_path: None,
    })
    .await?;
    let r = c.call(ReqKind::DaemonStatus).await?;
    println!("DaemonStatus: {r:?}");

    let (code, out) = ssh_native::ssh_exec(
        std::env::var("SSH_HOST").ok().as_deref().unwrap_or("127.0.0.1"),
        port,
        std::env::var("USER").unwrap_or_else(|_| "user".into()).as_str(),
        None,
        None,
        "echo native-exec-ok && uname -m",
    )
    .await?;
    println!("exec[{code}]: {out}");
    Ok(())
}

fn whoami() -> String {
    std::env::var("USER").unwrap_or_else(|_| "user".into())
}
