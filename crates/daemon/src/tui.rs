//! Pilotage du TUI de l'agent dans un PTY pour les actions absentes de
//! l'ACP — typiquement le `/fork` de Devin (commande interne au REPL, pas
//! de méthode `session/fork` côté `devin acp`).
//!
//! Stratégie : `script(1)` (util-linux) alloue le PTY, `stty` fixe la
//! taille (sans tty parent le PTY naît en 0×0 et le TUI ne rend rien).
//! On attend le marqueur de prompt OSC `133;A` émis par le TUI quand la
//! saisie est prête — envoyer plus tôt perd les frappes (flush du tty au
//! chargement de la session) — puis on tape `/fork` + Entrée et on parse
//! l'id natif imprimé ("Run devin -r <id> to launch forked session").

use anyhow::{anyhow, Context, Result};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

/// Timeout global du fork TUI (load session + fork + réponse).
const FORK_TOTAL_TIMEOUT: Duration = Duration::from_secs(120);
/// Attente max du marqueur de prompt avant envoi de secours.
const PROMPT_FALLBACK: Duration = Duration::from_secs(25);
/// Attente max du résultat après l'envoi de `/fork`.
const FORK_REPLY_TIMEOUT: Duration = Duration::from_secs(45);

/// Fork `native_id` via le TUI de l'agent ; retourne l'id natif de la
/// session forkée. `bin` = binaire de l'agent (ex. `devin`).
pub async fn fork_via_tui(bin: &str, cwd: &str, native_id: &str) -> Result<String> {
    let inner = format!(
        "stty rows 40 cols 120 >/dev/null 2>&1; exec {} -r {} --respect-workspace-trust false",
        shq(bin),
        shq(native_id),
    );
    let mut child = Command::new("script")
        .args(["-qefc", &inner, "/dev/null"])
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn script(1) — util-linux requis pour le fork TUI")?;
    let mut stdin = child.stdin.take().context("stdin du PTY")?;
    let mut stdout = child.stdout.take().context("stdout du PTY")?;

    let mut raw: Vec<u8> = Vec::new();
    // Sortie reçue APRÈS l'envoi de /fork — c'est là seulement qu'on cherche
    // le résultat (l'historique rejoué peut contenir un ancien "forked").
    let mut post: Vec<u8> = Vec::new();
    let mut sent = false;
    let mut sent_at = Instant::now();
    let t0 = Instant::now();
    let deadline = t0 + FORK_TOTAL_TIMEOUT;
    let mut buf = [0u8; 16384];

    let result = loop {
        let now = Instant::now();
        if now >= deadline {
            break Err(anyhow!("timeout du fork TUI"));
        }
        if sent && now.duration_since(sent_at) > FORK_REPLY_TIMEOUT {
            break Err(anyhow!("pas de réponse au /fork (timeout)"));
        }
        let slice = (deadline - now).min(Duration::from_millis(400));
        match tokio::time::timeout(slice, stdout.read(&mut buf)).await {
            Ok(Ok(0)) => break Err(anyhow!("le TUI s'est terminé sans fork")),
            Ok(Ok(n)) => {
                raw.extend_from_slice(&buf[..n]);
                if sent {
                    post.extend_from_slice(&buf[..n]);
                }
            }
            Ok(Err(e)) => break Err(anyhow!("lecture PTY: {e}")),
            Err(_) => {}
        }

        // Prompt prêt : le TUI émet OSC 133;A à chaque cycle de saisie.
        // (fallback 25 s si le marqueur n'arrive jamais.)
        if !sent
            && (memmem(&raw, b"133;A") || now.duration_since(t0) > PROMPT_FALLBACK)
        {
            for b in b"/fork" {
                stdin.write_all(&[*b]).await?;
                tokio::time::sleep(Duration::from_millis(60)).await;
            }
            stdin.write_all(b"\r").await?;
            sent = true;
            sent_at = Instant::now();
        }

        if sent {
            let clean = strip_ansi(&post);
            if let Some(id) = find_forked_id(&clean) {
                break Ok(id);
            }
            if let Some(msg) = fork_error(&clean) {
                break Err(anyhow!(msg));
            }
        }
    };

    // script(1) mort → master PTY fermé → devin reçoit SIGHUP et sort.
    let _ = child.kill().await;
    result
}

/// Erreur de fork reconnaissable dans la sortie TUI nettoyée.
fn fork_error(clean: &str) -> Option<&'static str> {
    [
        ("Failed to fork", "le TUI a refusé le fork"),
        (
            "already open in another process",
            "session native verrouillée par un autre processus",
        ),
        (
            "Unknown command: /fork",
            "cette version de l'agent ne connaît pas /fork",
        ),
    ]
    .iter()
    .find(|(pat, _)| clean.contains(pat))
    .map(|(_, msg)| *msg)
}

/// Recherche "devin -r <id> to launch forked session" dans le texte nettoyé.
fn find_forked_id(clean: &str) -> Option<String> {
    let start = clean.find("devin -r ")? + "devin -r ".len();
    let rest = &clean[start..];
    let end = rest.find(" to launch forked")?;
    let id = rest[..end].trim();
    (!id.is_empty()
        && id.len() < 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
    .then(|| id.to_string())
}

/// `needle` dans `hay` (memmem minimal, pas de dépendance).
fn memmem(hay: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || hay.windows(needle.len()).any(|w| w == needle)
}

/// Quote simple pour la ligne de commande interne à `script -c`.
fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Retire CSI (`ESC [ … @-~`) et OSC (`ESC ] … BEL|ESC\`) d'un flux PTY.
fn strip_ansi(raw: &[u8]) -> String {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == 0x1b && i + 1 < raw.len() {
            match raw[i + 1] {
                b'[' => {
                    i += 2;
                    while i < raw.len() && !(0x40..=0x7e).contains(&raw[i]) {
                        i += 1;
                    }
                    i += 1;
                }
                b']' => {
                    i += 2;
                    while i < raw.len() {
                        if raw[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if raw[i] == 0x1b && i + 1 < raw.len() && raw[i + 1] == b'\\' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                _ => i += 2,
            }
        } else {
            out.push(raw[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_forked_id() {
        let clean = " ✓ Session forked\r\n   Run devin -r elite-anise to launch forked session\r\n";
        assert_eq!(find_forked_id(clean).as_deref(), Some("elite-anise"));
        assert_eq!(find_forked_id("no fork here"), None);
        assert_eq!(
            find_forked_id("devin -r bogus id! to launch forked"),
            None
        );
    }

    #[test]
    fn ansi_stripped() {
        let raw = b"\x1b[2Jok \x1b]133;A\x07prompt\x1b[?2026h end";
        assert_eq!(strip_ansi(raw), "ok prompt end");
    }
}
