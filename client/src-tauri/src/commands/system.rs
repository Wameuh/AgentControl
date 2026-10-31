//! Infos système/client : plateforme, clé SSH générée, stats process,
//! tail du log client, icône fenêtre dynamique.

use anyhow::Result;
use client_core::Level;
use serde_json::{json, Value};
use tauri::State;

use crate::state::AppState;

/// Version « identité » du client : `X.Y.Z` du manifeste, suffixée
/// `-rc.K` quand le binaire a été compilé pour un tag rc (env
/// AGENTCONTROL_RC=K posé par build.yml — l'injection fichier de
/// bump_version.py est impossible sur Windows/macOS : MSI et
/// CFBundleVersion exigent du numérique). Linux/Android ont déjà le
/// suffixe dans Cargo.toml → pas de double suffixe. Sans ce suffixe
/// l'updater restait « à jour » à vie : « X.Y.Z » local > « -rc.K »
/// distant (prerelease < stable à X.Y.Z égal). Le gate mineur
/// démon↔client reste sain : minorOf() masque le suffixe.
#[tauri::command]
pub(crate) fn app_version() -> String {
    const V: &str = env!("CARGO_PKG_VERSION");
    // Re-validation ici et pas seulement à l'émission (build.rs) :
    // option_env! lit aussi l'env AMBIANTE de rustc — un
    // `AGENTCONTROL_RC=abc cargo build` passerait sinon tel quel.
    match option_env!("AGENTCONTROL_RC") {
        Some(k) if !V.contains('-') && k.bytes().all(|b| b.is_ascii_digit()) => {
            format!("{V}-rc.{k}")
        }
        _ => V.to_string(),
    }
}

/// "android" | "windows" | "macos" | "linux" — pour les gates frontend.
#[tauri::command]
pub(crate) fn platform() -> &'static str {
    if cfg!(target_os = "android") {
        "android"
    } else if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

/// Connexion demandée à cette instance (`--conn` / AGENTCONTROL_CONN au
/// lancement). `None` = comportement par défaut : la dernière connexion
/// utilisée (première entrée de connections.json).
#[tauri::command]
pub(crate) fn cli_conn(state: State<'_, AppState>) -> Option<String> {
    state.cli_conn.clone()
}

/// Lance une AUTRE instance du client liée à `conn` (clé connKey —
/// `ssh{,-native}:h:p` | `tcp:addr` | `local`) : le modèle multi-instances
/// est « 1 processus = 1 connexion », plusieurs instances en parallèle
/// chacune sur son démon, distinguée par sa couleur. L'instance courante
/// n'est pas modifiée ; la nouvelle hérite de --conn <clé>.
/// Limites : Android exclu (pas de multi-processus applicatif) ; macOS ok
/// (exec direct du binaire dans le .app, indépendant de LaunchServices).
#[tauri::command]
#[allow(unused_variables)]
pub(crate) fn spawn_instance(conn: String) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        return Err("multi-instances indisponible sur Android".to_string());
    }
    #[cfg(not(target_os = "android"))]
    {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut cmd = tokio::process::Command::new(exe);
        cmd.arg("--conn")
            .arg(&conn)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        client_core::hide_console(&mut cmd);
        // Détaché : l'enfant survit à la fermeture de cette instance.
        cmd.spawn().map_err(|e| format!("{e}"))?;
        client_core::log(Level::Info, format!("spawn_instance conn={conn}"));
        Ok(())
    }
}

/// Ligne authorized_keys de la clé SSH générée par l'app (SSH natif).
#[tauri::command]
pub(crate) fn ssh_pubkey() -> Result<String, String> {
    client_core::ssh_native::pubkey_line().map_err(|e| format!("{e:#}"))
}

/// Stats process client : RSS Mo + CPU % (delta depuis l'appel précédent).
#[tauri::command]
pub(crate) async fn sys_stats(state: State<'_, AppState>) -> Result<Value, String> {
    let pid = sysinfo::get_current_pid().map_err(|e| e.to_string())?;
    let mut sys = state.sys.lock().await;
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    let p = sys.process(pid).ok_or("process introuvable")?;
    // Débit : fenêtre glissante des 5 dernières secondes sur les compteurs
    // cumulés de client-core (toutes connexions confondues).
    let now = std::time::Instant::now();
    let rx = client_core::RX_TOTAL.load(std::sync::atomic::Ordering::Relaxed);
    let tx = client_core::TX_TOTAL.load(std::sync::atomic::Ordering::Relaxed);
    let mut q = state.net.lock().await;
    q.push_back((now, rx, tx));
    while q.front().map(|(t, _, _)| now.duration_since(*t).as_secs_f64() > 5.5).unwrap_or(false) {
        q.pop_front();
    }
    let (t0, rx0, tx0) = *q.front().unwrap();
    let dt = now.duration_since(t0).as_secs_f64().max(0.001);
    Ok(json!({
        "rss_mb": (p.memory() as f64 / 1_048_576.0).round(),
        "cpu": (p.cpu_usage() * 10.0).round() / 10.0,
        "rx_bps": ((rx - rx0) as f64 / dt).round(),
        "tx_bps": ((tx - tx0) as f64 / dt).round(),
    }))
}

/// Les N dernières lignes du log client.
#[tauri::command]
pub(crate) fn log_tail(n: Option<usize>) -> String {
    client_core::log_tail(n.unwrap_or(200))
}

/// Écrit une ligne dans client.log depuis le frontend (erreurs UI,
/// diagnostics webview — préfixées `ui:`). Niveau tolérant, info si inconnu.
/// file vide : la callsite Rust ne dirait rien d'utile sur la source TS.
#[tauri::command]
pub(crate) fn log_write(level: &str, msg: &str) {
    let lv = client_core::Level::parse(level).unwrap_or(client_core::Level::Info);
    client_core::log_at(lv, "", 0, format!("ui: {msg}"));
}

/// Icône fenêtre dynamique selon l'état agrégé des agents :
/// "base" (sauge), "gold" (agent actif), "violet" (compaction),
/// "red" (question en attente), "grey" (aucun agent).
/// No-op sur Android (icône launcher fixe).
#[tauri::command]
#[allow(unused_variables)]
pub(crate) fn set_app_icon(window: tauri::WebviewWindow, state: &str) -> Result<(), String> {
    #[cfg(not(target_os = "android"))]
    {
        let bytes: &[u8] = match state {
            "gold" => include_bytes!("../../icons/icon-gold.rgba"),
            "violet" => include_bytes!("../../icons/icon-violet.rgba"),
            "red" => include_bytes!("../../icons/icon-red.rgba"),
            "grey" => include_bytes!("../../icons/icon-grey.rgba"),
            _ => include_bytes!("../../icons/icon.rgba"),
        };
        // Contrat avec tools/gem_icon.py : RGBA brut 512×512.
        debug_assert_eq!(bytes.len(), 512 * 512 * 4);
        window
            .set_icon(tauri::image::Image::new_owned(bytes.to_vec(), 512, 512))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

