//! Étapes de tâche publiées par l'agent (`session_set_steps`, skill
//! ac-steps / `acctl steps set`) : liste persistée dans `steps.json`
//! (dossier de session) — même pattern que pinned.json. Le contrat est
//! un remplacement COMPLET à chaque appel (l'agent garde sa liste et
//! repousse tout — pas de drift d'index) ; chaque changement est aussi
//! journalisé en event `steps` pour le live update côté clients.
//! La liste est vidée au /clear et au rebind vers une autre session
//! native : le journal repart à zéro, l'état des étapes avec (sinon
//! meta.json mentirait sur un cycle qui n'existe plus).

use super::*;
use proto::SessionStep;

/// Borne d'état par session : les étapes restent un repère de
/// progression, pas un second journal — au-delà c'est une bad
/// organisation que l'agent doit corriger.
pub(crate) const STEPS_MAX: usize = 100;
/// Nom d'étape borné : un nom est un libellé court, pas une phrase —
/// au-delà on tronque (l'état reste borné même si l'agent exagère).
pub(crate) const STEP_NAME_MAX: usize = 200;

impl Session {
    /// steps.json initialisé au chargement — absent/vide/illisible →
    /// liste vide (un fichier corrompu n'empêche pas l'ouverture).
    pub(super) fn open_steps(dir: &std::path::Path) -> Mutex<Vec<SessionStep>> {
        let steps = std::fs::read_to_string(dir.join("steps.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Mutex::new(steps)
    }

    fn save_steps(&self) {
        // .tmp + rename : pas de steps.json tronqué si le démon meurt
        // en pleine écriture (même convention que pinned.json).
        let tmp = self.dir.join("steps.json.tmp");
        let st = self.steps.lock().unwrap();
        if std::fs::write(&tmp, serde_json::to_string_pretty(&*st).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, self.dir.join("steps.json"));
        }
    }

    /// Remplace la liste d'étapes entière (idempotent) : normalise les
    /// noms (trim, borne), persiste steps.json, journalise l'event
    /// `steps` (live update + reconstruction) et renvoie la liste
    /// stockée. Liste identique à la courante → no-op silencieux (un
    /// agent qui republie la même liste ne spamme pas le journal).
    pub fn set_steps(&self, mut steps: Vec<SessionStep>) -> Result<Vec<SessionStep>> {
        if steps.len() > STEPS_MAX {
            anyhow::bail!(
                "trop d'étapes : {} > {STEPS_MAX} — fusionne ou abandonne les étapes soldées",
                steps.len()
            );
        }
        for s in &mut steps {
            s.name = s.name.trim().to_string();
            anyhow::ensure!(!s.name.is_empty(), "étape sans nom");
            if s.name.chars().count() > STEP_NAME_MAX {
                s.name = s.name.chars().take(STEP_NAME_MAX).collect();
            }
        }
        {
            let mut cur = self.steps.lock().unwrap();
            if *cur == steps {
                return Ok(steps);
            }
            *cur = steps.clone();
        }
        self.save_steps();
        self.emit(EventKind::Steps {
            steps: steps.clone(),
        });
        Ok(steps)
    }

    /// Liste courante — relue par session_info (`steps`) et le handler
    /// set (réponse = liste normalisée stockée).
    pub fn steps_snapshot(&self) -> Vec<SessionStep> {
        self.steps.lock().unwrap().clone()
    }

    /// Vide la liste (reset de cycle : /clear, rebind) — n'écrit et
    /// n'émet que si des étapes existaient. Retourne vrai si vidé.
    pub fn clear_steps(&self) -> bool {
        let had = {
            let mut st = self.steps.lock().unwrap();
            if st.is_empty() {
                return false;
            }
            st.clear();
            true
        };
        if !had {
            return false;
        }
        self.save_steps();
        self.emit(EventKind::Steps { steps: vec![] });
        true
    }
}
