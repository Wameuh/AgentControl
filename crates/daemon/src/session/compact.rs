//! Retenue de compact : un `/compact` agent-side rend la main
//! immédiatement — les prompts envoyés pendant le compact sont gardés
//! en file jusqu'au signal de fin (contexte réduit, message de clôture,
//! cancel ou timeout garde-fou).

use super::*;
use proto::EventKind;

/// Garde-fou de la retenue compact si l'agent ne signale jamais la fin
/// (ni usage_update réduit, ni message "compacted", ni cancel).
const COMPACT_TIMEOUT_SECS: u64 = 180;

/// `/compact` (avec ou sans arguments) déclenche une compaction côté
/// agent — le prompt rend la main immédiatement mais la compaction
/// continue en arrière-plan.
pub(super) fn is_compact_cmd(text: &str) -> bool {
    text.trim_start()
        .strip_prefix("/compact")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

/// Heuristique de fin de compact sur un message agent : un mot en
/// "compact…" + un terme de clôture, en bornes de mot (évite de matcher
/// "uncompacted" ou un "done" noyé dans une sous-chaîne).
pub(super) fn compact_done_text(text: &str) -> bool {
    let l = text.to_lowercase();
    let mut compact = false;
    let mut done = false;
    for w in l.split(|c: char| !c.is_alphanumeric()) {
        compact |= w.starts_with("compact");
        done |= matches!(
            w,
            "compacted" | "compacté" | "compactée" | "cancel" | "cancelled" | "canceled"
                | "annulé" | "annulée" | "abort" | "aborted" | "finish" | "finished" | "done"
                | "interrupt" | "interrupted" | "error" | "errors" | "fail" | "failed"
                | "failure" | "échoué" | "cancellation" | "complete" | "completed"
                | "terminé" | "terminée"
        );
    }
    compact && done
}

impl Session {
    /// Arme la retenue compact : le prochain retour à idle devient
    /// "compacting" et les prompts sont retenus en file jusqu'au signal
    /// de fin (contexte réduit, message agent, cancel) ou au timeout.
    pub(super) fn arm_compact(&self) {
        let mut c = self.compacting.lock().unwrap();
        if let Some(old) = c.take() {
            old.timer.abort();
        }
        let used_at_start = self.meta.lock().unwrap().context_used;
        let me = self.me.clone();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(COMPACT_TIMEOUT_SECS)).await;
            if let Some(s) = me.upgrade() {
                s.end_compact("timeout");
            }
        });
        *c = Some(CompactHold {
            used_at_start,
            timer,
        });
    }

    /// Stoppe la retenue compact (annule le garde-fou) sans rien livrer.
    /// Retourne vrai si une retenue était active.
    pub(super) fn take_compact_hold(&self) -> bool {
        if let Some(h) = self.compacting.lock().unwrap().take() {
            h.timer.abort();
            true
        } else {
            false
        }
    }

    /// Fin normale du compact : journalise, repasse idle (si l'agent
    /// vit encore) et relance le drain pour livrer les retenus.
    pub(super) fn end_compact(&self, reason: &str) {
        if !self.take_compact_hold() {
            return;
        }
        self.emit(EventKind::System {
            text: format!("compact terminé — {reason}"),
        });
        if self.is_alive() {
            self.set_state("idle");
            self.spawn_drain();
        }
    }

}
