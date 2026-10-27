//! Smoke test client-core : connexion, daemon_status, session_info, events.
//! Usage: AGENTCONTROL_SOCK=/tmp/ac-test/daemon.sock cargo run --example smoke [session_id]

use client_core::{Client, Transport};
use proto::ReqKind;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let t = if let Ok(addr) = std::env::var("AGENTCONTROL_TCP") {
        Transport::Tcp(addr)
    } else {
        let p = std::env::var("AGENTCONTROL_SOCK").unwrap_or_else(|_| {
            format!(
                "{}/.agentcontrol/daemon.sock",
                std::env::var("HOME").unwrap_or_default()
            )
        });
        Transport::Unix(PathBuf::from(p))
    };
    let c = Client::connect(&t).await?;
    println!("status: {}", c.call(ReqKind::DaemonStatus).await?);
    println!("sessions: {}", c.call(ReqKind::ListSessions).await?);
    if let Some(sid) = std::env::args().nth(1) {
        println!(
            "info: {}",
            c.call(ReqKind::SessionInfo { session: sid.clone() }).await?
        );
        let v = c
            .call(ReqKind::Events {
                session: sid,
                before_seq: None,
                after_seq: None,
                limit: 3,
            })
            .await?;
        println!("last events: {}", serde_json::to_string_pretty(&v)?);
    }
    Ok(())
}
