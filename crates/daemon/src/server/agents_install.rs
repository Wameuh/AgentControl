//! Provisioning à la demande des adaptateurs ACP du registre (#277).
//!
//! `install_agent_adapter` exécute côté démon les commandes
//! d'installation déclarées par `agents_auto` (`npm i -g <pkg>`, argv
//! complet sans shell) — déclenché par l'utilisateur (bouton
//! « installer » du dialogue nouvelle session, `acctl agents install
//! <id>`), jamais automatique : pas d'install globale silencieuse.
//!
//! Garde-fous : jamais de sudo ; outil d'install (npm) absent →
//! candidat sauté, erreur explicite si tous manquent ; installation
//! dans le HOME de l'utilisateur du démon — quand le préfixe global
//! npm est hors HOME (node système → /usr/local ou /usr, non
//! inscriptible sans sudo), on passe `--prefix ~/.npm-global` (déjà
//! fouillé par `command_path`), avec le même repli si npm bute sur
//! EACCES ; timeout borné ; un adaptateur déjà résolvable n'est
//! jamais réinstallé (no-op `{already:true}`) ; les candidates sont
//! essayées dans l'ordre jusqu'à ce qu'un binaire de `commands`
//! resolve — npm gère les versions, aucun pin imposé.

use super::agents_auto::{AdapterSpec, KNOWN_ADAPTERS};
use super::misc::command_path;
use super::Daemon;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tracing::info;

/// `npm i -g` peut télécharger plusieurs Mo — large mais borné.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(300);
/// Sonde `npm config get prefix` — locale, pas de réseau.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// Queue des logs npm conservée pour le diagnostic d'erreur.
const LOG_TAIL: usize = 8 * 1024;

/// Sérialise les installs : deux `npm i -g` concurrents sur le même
/// préfixe se marchent dessus (et un retry EACCES bouclerait).
static INSTALL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

impl Daemon {
    /// `InstallAgentAdapter` : installe l'adaptateur ACP de `agent`
    /// (id du registre agents_auto — profil auto, builtin ou
    /// `[agents.*]` déclaré pour un adaptateur connu). Voir
    /// l'en-tête du module pour les garde-fous.
    pub(super) async fn h_install_agent_adapter(
        self: &Arc<Self>,
        agent: String,
    ) -> Result<Value> {
        let spec = KNOWN_ADAPTERS
            .iter()
            .find(|s| s.id == agent)
            .with_context(|| format!("unknown adapter in registry: {agent}"))?;
        if spec.install.is_empty() {
            bail!(
                "no automatic install known for \"{}\" — {}",
                spec.name,
                spec.install_hint
            );
        }
        // Déjà résolvable → no-op : jamais de réinstall superflue.
        // Le check porte sur les adaptateurs du spec (pas le profil) —
        // un `[agents.<id>]` custom n'y change rien.
        if adapter_resolved(spec) {
            return Ok(json!({
                "agent": agent,
                "installed": false,
                "already": true,
            }));
        }
        let _permit = INSTALL_LOCK.lock().await;
        // Préfixe global npm : hors HOME → `--prefix ~/.npm-global`
        // d'emblée (install 100 % user, dossier déjà dans
        // agent_path_dirs) ; indéterminable → tentative standard, le
        // repli EACCES ci-dessous couvre ce cas.
        let prefix_args = npm_prefix_args().await;
        let mut last_err = String::new();
        for argv in spec.install {
            let Some(prog) = argv.first().and_then(|b| command_path(b)) else {
                last_err = format!("{} not found on the daemon PATH", argv[0]);
                continue;
            };
            let base: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            let args = &base[1..];
            match attempt(spec, &agent, &prog, args, &prefix_args).await {
                Ok(v) => {
                    self.reload_agent_profiles().await;
                    return Ok(v);
                }
                Err(e) => {
                    last_err = e.clone();
                    // Échec de droits sur un préfixe qu'on n'a pas su
                    // deviner → une seconde chance en --prefix user.
                    if prefix_args.is_empty() && looks_like_permission(&e) {
                        let pa = user_prefix_args();
                        if !pa.is_empty() {
                            match attempt(spec, &agent, &prog, args, &pa).await {
                                Ok(v) => {
                                    self.reload_agent_profiles().await;
                                    return Ok(v);
                                }
                                Err(e2) => last_err = format!("{e}\n--prefix user : {e2}"),
                            }
                        }
                    }
                }
            }
        }
        bail!("installing adapter \"{agent}\" failed — {last_err}")
    }
}

/// Vrai si au moins une commande candidate du spec a un argv[0]
/// résolvable — l'adaptateur est déjà là, rien à installer.
fn adapter_resolved(spec: &AdapterSpec) -> bool {
    spec.commands
        .iter()
        .any(|c| c.first().map(|b| command_path(b).is_some()).unwrap_or(false))
}

/// Binaire d'adaptateur résolu après install (pour la réponse).
fn resolved_bin(spec: &AdapterSpec) -> Option<&'static str> {
    spec.commands
        .iter()
        .filter_map(|c| c.first())
        .find(|b| command_path(b).is_some())
        .copied()
}

/// Une tentative d'install : `Ok(json)` quand la commande réussit ET
/// qu'un adaptateur résout ensuite ; `Err(message)` sinon (échec de
/// la commande — queue de logs jointe — ou package sans le binaire
/// attendu). `label` = argv affiché dans la réponse.
async fn attempt(
    spec: &AdapterSpec,
    agent: &str,
    prog: &Path,
    args: &[String],
    prefix_args: &[String],
) -> Result<Value, String> {
    let label = format!("{} {}", prog.display(), args.join(" "));
    let label = if prefix_args.is_empty() {
        label
    } else {
        format!("{label} {}", prefix_args.join(" "))
    };
    match run_install(prog, args, prefix_args, INSTALL_TIMEOUT).await {
        Err(e) => Err(format!("`{label}` : {e:#}")),
        Ok(out) => {
            if let Some(bin) = resolved_bin(spec) {
                info!("adapter \"{agent}\" installed via `{label}`");
                Ok(json!({
                    "agent": agent,
                    "installed": true,
                    "command": label,
                    "resolved": bin,
                    "output": tail(&out),
                }))
            } else {
                Err(format!(
                    "`{label}` succeeded but no adapter binary resolves:\n{}",
                    tail(&out)
                ))
            }
        }
    }
}

/// `~/.npm-global` si HOME est connu — préfixe de repli 100 % user.
fn user_npm_prefix() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".npm-global"))
}

/// Args `--prefix <dir>` forçant l'install dans `~/.npm-global`.
fn user_prefix_args() -> Vec<String> {
    user_npm_prefix()
        .map(|p| vec!["--prefix".into(), p.display().to_string()])
        .unwrap_or_default()
}

/// Args `--prefix` à ajouter à `npm i -g` : vide quand le préfixe
/// global de npm est déjà sous HOME (ou indéterminable — le repli
/// EACCES couvre ce cas) ; `--prefix ~/.npm-global` sinon (node
/// système → /usr/local ou /usr, non inscriptible sans sudo).
async fn npm_prefix_args() -> Vec<String> {
    let (Some(npm), Some(home)) = (
        command_path("npm"),
        std::env::var_os("HOME").map(PathBuf::from),
    ) else {
        return Vec::new();
    };
    let out = Command::new(&npm)
        .args(["config", "get", "prefix"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let Ok(Ok(out)) = tokio::time::timeout(PROBE_TIMEOUT, out).await else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let prefix = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if prefix.is_empty() {
        return Vec::new();
    }
    let p = PathBuf::from(&prefix);
    // Préfixe déjà utilisateur (sous HOME — ex. nvm, ou ~/.npm-global
    // même si HOME n'est pas son parent lexical, home symlinké) →
    // `npm i -g` standard. Sinon forcer le préfixe user.
    let under_home = p.starts_with(&home)
        || user_npm_prefix().is_some_and(|u| p == u || p.starts_with(&u));
    if under_home {
        Vec::new()
    } else {
        user_prefix_args()
    }
}

/// Exécute une commande d'installation (argv sans shell, env héritée
/// — HOME du user du démon), timeout `dur`. Erreur = spawn impossible,
/// timeout ou code de sortie ≠ 0 (queue de logs jointe au message).
async fn run_install(
    prog: &Path,
    args: &[String],
    prefix_args: &[String],
    dur: Duration,
) -> Result<Vec<u8>> {
    let child = Command::new(prog)
        .args(args)
        .args(prefix_args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawn install")?;
    let out = match tokio::time::timeout(dur, child.wait_with_output()).await {
        Ok(r) => r.context("install wait")?,
        Err(_) => bail!("install timed out ({dur:?})"),
    };
    if !out.status.success() {
        let mut log = String::from_utf8_lossy(&out.stderr).to_string();
        if log.trim().is_empty() {
            log = String::from_utf8_lossy(&out.stdout).to_string();
        }
        bail!("code {} — {}", out.status, tail_str(&log));
    }
    Ok(out.stdout)
}

/// Sortie tronquée à `LOG_TAIL` octets (fin — les erreurs npm sont en
/// queue de stderr).
fn tail(out: &[u8]) -> String {
    tail_str(&String::from_utf8_lossy(out))
}

fn tail_str(s: &str) -> String {
    if s.len() <= LOG_TAIL {
        return s.trim().to_string();
    }
    let mut start = s.len() - LOG_TAIL;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &s[start..])
}

/// Vrai si l'échec ressemble à un problème de droits d'écriture —
/// déclenche le repli `--prefix ~/.npm-global`.
fn looks_like_permission(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("eacces") || m.contains("eperm") || m.contains("permission denied")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_borne_la_sortie() {
        let big = "x".repeat(LOG_TAIL * 2);
        let t = tail(big.as_bytes());
        assert!(t.len() < LOG_TAIL + 8);
        assert!(t.starts_with('…'));
        assert_eq!(tail(b"ok"), "ok");
    }

    #[test]
    fn detection_permission() {
        assert!(looks_like_permission(
            "EACCES: permission denied, mkdir '/usr/lib'"
        ));
        assert!(looks_like_permission("Error: EPERM operation not permitted"));
        assert!(!looks_like_permission("npm ERR! 404 Not Found"));
    }
}
