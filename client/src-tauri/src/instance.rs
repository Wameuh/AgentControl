//! Registre des instances client vivantes (~/.agentcontrol/instances).
//! But : un flap post-update peut venir d'un relaunch qui chevauche
//! l'ancienne instance ou d'un spawn_instance concurrent — sans registre,
//! client.log montre des transports qui se croisent sans dire qu'ils
//! appartiennent à des process différents.

use client_core::Level;
use serde_json::json;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn instance_dir() -> Option<PathBuf> {
    client_core::config_path()
        .parent()
        .map(|p| p.join("instances"))
}

fn refresh_sys() -> sysinfo::System {
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    sys
}

fn proc_meta(sys: &sysinfo::System, pid: u32) -> Option<(u32, Option<PathBuf>)> {
    let p = sys.process(sysinfo::Pid::from_u32(pid))?;
    Some((
        p.parent().map(|x| x.as_u32()).unwrap_or(0),
        p.exe().map(|x| x.to_path_buf()),
    ))
}

fn alive(sys: &sysinfo::System, pid: u32, exe: Option<&str>) -> bool {
    let Some((_, cur)) = proc_meta(sys, pid) else { return false };
    match (exe, cur.as_deref()) {
        (Some(want), Some(got)) => {
            want.is_empty() || got == std::path::Path::new(want)
        }
        _ => true,
    }
}

/// Enregistre cette instance puis journalise les autres vivantes.
/// Appelée une fois au setup : le fichier <pid>.json survit à la mort
/// du process mais la vérification pid/exe au prochain boot le purge.
pub(crate) fn init(cli_conn: Option<&str>) {
    let Some(dir) = instance_dir() else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let pid = std::process::id();
    let version = crate::commands::system::app_version();
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let sys = refresh_sys();
    let ppid = proc_meta(&sys, pid).map(|x| x.0).unwrap_or(0);
    let started = now_ms();
    let me = json!({
        "pid": pid,
        "ppid": ppid,
        "version": version,
        "exe": exe,
        "conn": cli_conn,
        "started_ms": started,
    });
    let mine = dir.join(format!("{pid}.json"));
    let tmp = dir.join(format!(".{pid}.tmp"));
    if std::fs::write(&tmp, me.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &mine);
    }
    client_core::log(
        Level::Info,
        format!(
            "instance démarrée pid={pid} ppid={ppid} version={version} exe={exe} conn={}",
            cli_conn.unwrap_or("-")
        ),
    );

    let mut others = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for ent in rd.flatten() {
            let path = ent.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Ok(txt) = std::fs::read_to_string(&path) else {
                let _ = std::fs::remove_file(&path);
                continue;
            };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else {
                let _ = std::fs::remove_file(&path);
                continue;
            };
            let other_pid = v.get("pid").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
            if other_pid == 0 || other_pid == pid {
                continue;
            }
            let exe = v.get("exe").and_then(|x| x.as_str()).map(String::from);
            if alive(&sys, other_pid, exe.as_deref()) {
                others.push(v);
            } else {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    if others.is_empty() {
        client_core::log(Level::Info, "aucune autre instance client vivante");
        return;
    }
    client_core::log(
        Level::Warn,
        format!("{} autre(s) instance(s) client vivante(s) détectée(s)", others.len()),
    );
    for v in others {
        client_core::log(
            Level::Warn,
            format!(
                "instance concurrente pid={} ppid={} version={} exe={} conn={} démarrée={} ms",
                v.get("pid").and_then(|x| x.as_u64()).unwrap_or(0),
                v.get("ppid").and_then(|x| x.as_u64()).unwrap_or(0),
                v.get("version").and_then(|x| x.as_str()).unwrap_or("?"),
                v.get("exe").and_then(|x| x.as_str()).unwrap_or("?"),
                v.get("conn").and_then(|x| x.as_str()).unwrap_or("-"),
                now_ms().saturating_sub(v.get("started_ms").and_then(|x| x.as_u64()).unwrap_or(0)),
            ),
        );
    }
}
