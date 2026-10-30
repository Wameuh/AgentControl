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
    /// Seuil de contexte franchi : enfile `/compact` EN TÊTE de file
    /// (provenance Auto — livré sans user_message, la retenue s'armera
    /// au drain avec le flag auto → « continue your task » à la fin).
    /// En tête plutôt qu'en queue : la protection contexte prime sur
    /// les messages déjà en attente (ils patientent pendant le compact).
    pub(super) fn request_auto_compact(&self, used: u64, size: u64) {
        if self.compacting.lock().unwrap().is_some() {
            return; // un compact (manuel ou auto) tourne déjà
        }
        // Agent qui annonce ses commandes sans y lister /compact : le
        // prompt tomberait dans le timeout de 180 s pour rien —
        // court-circuit. Liste vide = agent muet sur ses capacités →
        // on tente quand même (le timeout borne l'échec).
        {
            let m = self.meta.lock().unwrap();
            if !m.available_commands.is_empty()
                && !m.available_commands.iter().any(|c| c.name() == "compact")
            {
                drop(m);
                self.emit(EventKind::System {
                    text: format!(
                        "seuil de contexte atteint ({}) — compact auto impossible : /compact non supporté par l'agent",
                        describe_usage(used, size)
                    ),
                });
                return;
            }
        }
        // Déjà un /compact en attente (franchissement double — ex.
        // baseline reset par un spawn entre trigger et drain) : un
        // doublon relancerait un second compact juste après le premier.
        if self
            .prompt_queue
            .lock()
            .unwrap()
            .iter()
            .any(|(t, _)| is_compact_cmd(t))
        {
            return;
        }
        self.emit(EventKind::System {
            text: format!(
                "seuil de contexte atteint ({}) — compact automatique",
                describe_usage(used, size)
            ),
        });
        self.enqueue_prompt("/compact", true, PromptSrc::Auto);
        self.spawn_drain();
    }

    /// Arme la retenue compact : le prochain retour à idle devient
    /// "compacting" et les prompts sont retenus en file jusqu'au signal
    /// de fin (contexte réduit, message agent, cancel) ou au timeout.
    /// `auto` = déclenché par le seuil démon → la fin relance l'agent.
    pub(super) fn arm_compact(&self, auto: bool) {
        let mut c = self.compacting.lock().unwrap();
        if let Some(old) = c.take() {
            old.timer.abort();
        }
        let used_at_start = self.meta.lock().unwrap().context_used;
        let me = self.me.clone();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(COMPACT_TIMEOUT_SECS)).await;
            if let Some(s) = me.upgrade() {
                s.end_compact("timeout", false);
            }
        });
        *c = Some(CompactHold {
            used_at_start,
            timer,
            auto,
        });
    }

    /// Retire la retenue et son garde-fou — renvoie le hold pour que
    /// l'appelant lise `auto` (end_compact) ; None si rien d'armé.
    fn take_hold(&self) -> Option<CompactHold> {
        let h = self.compacting.lock().unwrap().take();
        if let Some(h) = &h {
            h.timer.abort();
        }
        h
    }

    /// Stoppe la retenue compact (annule le garde-fou) sans rien livrer.
    /// Retourne vrai si une retenue était active.
    pub(super) fn take_compact_hold(&self) -> bool {
        self.take_hold().is_some()
    }

    /// Fin normale du compact : journalise, repasse idle (si l'agent
    /// vit encore) et relance le drain pour livrer les retenus.
    /// Compact auto → « continue your task » repart en tête de file —
    /// sauf `cancelled` (cancel utilisateur exprès) : relancer derrière
    /// son dos serait se battre avec lui. Enfilé même agent mort : le
    /// drain post-respawn livrera (un respawn purge la retenue, pas la
    /// file). La file reste volatile : un redémarrage du démon la perd
    /// — le reset de baseline au spawn re-déclenchera un compact si le
    /// seuil est toujours dépassé.
    pub(super) fn end_compact(&self, reason: &str, cancelled: bool) {
        let Some(h) = self.take_hold() else {
            return;
        };
        self.emit(EventKind::System {
            text: format!("compact terminé — {reason}"),
        });
        if h.auto && !cancelled {
            self.emit(EventKind::System {
                text: "reprise — envoi « continue your task »".into(),
            });
            self.enqueue_prompt("continue your task", true, PromptSrc::Auto);
        }
        if self.is_alive() {
            self.set_state("idle");
            self.spawn_drain();
        }
    }

}

/// `184 000/200 000 tokens` pour les messages de seuil.
fn describe_usage(used: u64, size: u64) -> String {
    if size > 0 {
        format!("{used}/{size} tokens")
    } else {
        format!("{used} tokens")
    }
}
