//! GC des sessions composant orphelines (item #250).
//!
//! Les sessions `PRReview-*` (`origin: feature:pr-review`) et les
//! sessions du pipeline (`origin: component:backlog-orchestrator` —
//! `Backlog-W*`/`Backlog-P*`/`Backlog-R*`) sont censées être supprimées
//! par le mécanisme qui les a produites (verdict review, ménage
//! worker, sweep reviewer). Plusieurs chemins avalaient l'erreur de
//! `h_delete_session` ou ne couvraient pas tous les états → après un
//! restart du démon elles reapparaissent `interrupted`/`dead` et
//! survivent indéfiniment (constat du 13 nov : PRReview-302/310/316/
//! 321×3/322/323 et Backlog-W170).
//!
//! Cette passe est le filet de sécurité : à chaque tick (donc au
//! démarrage, premier tick immédiat), toute session d'origine composant
//! NON référencée par l'état vivant est supprimée :
//!
//!   - référencée par une review PR `running` (`pr_reviews.sid`, ou
//!     `reported_by` — la session reviewer dont le verdict A2A est
//!     posé mais pas encore posté doit vivre jusqu'au finish, sa
//!     suppression anticipée couperait le retour du tool call) ;
//!   - référencée par le pipeline plan (`st.reviewers` — reviewers
//!     persistants endormis entre rounds) ;
//!   - référencée par un item non soldé de la file (`agent_id` hors
//!     `done`/`pending` — ces statuts gardent l'agent_id historique
//!     sans faire vivre la session) ;
//!   - référencée par une PR du registre non terminale (worker dont
//!     la PR reste à surveiller/merger) ;
//!   - en grâce de ménage (`st.clean_since` — ne sauter que la
//!     fenêtre CLEAN_GRACE_S du « propre sauf PR non enregistrées »,
//!     jamais la supprimer avant : casse l'attribution PR du watcher) ;
//!   - sous waiter `ac-wait` vivant (son ping la réveillerait — la
//!     supprimer annulerait le waiter de toute façon).
//!
//! Session « suivie par l'entretien » (titre matchant le préfixe
//! worker, ou planner/reviewer quand `plan_review` est actif) : la
//! GC la saute — son cycle de vie (grâce, rm soldé, nudges, dead
//! stale) appartient au ménage worker, le dupliquer ici contournerait
//! la grâce. La GC couvre donc les sessions que PERSONNE ne suit :
//! PRReview déréférencées et sessions composant hors classification
//! (préfixe renommé, état perdu, rôle non couvert par le mode actif).
//!
//! `tasks = None` (file agentbacklog illisible/absente) : seules les
//! sessions `feature:pr-review` sont collectables — leurs références
//! vivent entièrement dans `pr_reviews`, alors que celles des sessions
//! composant exigent la file pour ne pas supprimer un worker porteur
//! d'un item wip.
//!
//! Une session `working`/`starting` déréférencée est supprimée comme
//! les autres : la vérification de déréférencement EST la borne — une
//! review ou un item légitime en vol reste référencé. Le worktree
//! `pr-reviews/*` d'un `PRReview-*` part avec lui.

use super::orch::*;
use super::orch_plan::{PLANNER_PREFIX, REVIEWER_PREFIX};
use super::orch_review::{pr_reviews_root, remove_review_worktree, EntryState, PR_REVIEW_ORIGIN};
use super::*;
use proto::{PrEntry, SessionMeta};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use tracing::{info, warn};

impl Daemon {
    /// Passe GC — voir l'en-tête de module. `tasks`/`prs` = snapshots
    /// du cycle appelant (None/vide → périmètre réduit aux PRReview).
    /// Jamais fatale : chaque suppression est best-effort.
    pub(crate) async fn orch_gc_orphans(self: &Arc<Self>, tasks: Option<&[Task]>, prs: &[PrEntry]) {
        let cfg = self.orch.cfg.read().unwrap().clone();
        let metas: Vec<SessionMeta> = {
            let sessions = self.sessions.read().await;
            sessions.values().map(|s| s.meta_snapshot()).collect()
        };
        if metas.is_empty() {
            return;
        }
        // ---- Références vivantes.
        let mut referenced: HashSet<String> = HashSet::new();
        {
            let st = self.orch.state.lock().unwrap();
            for v in st.pr_reviews.values() {
                if EntryState::of(v) != EntryState::Running {
                    continue;
                }
                for f in ["sid", "reported_by"] {
                    if let Some(sid) = v.get(f).and_then(Value::as_str) {
                        if !sid.is_empty() {
                            referenced.insert(sid.to_string());
                        }
                    }
                }
            }
            referenced.extend(st.reviewers.values().cloned());
        }
        if let Some(tasks) = tasks {
            for t in tasks {
                // `done`/`pending` exclus : agent_id persiste après
                // done (historique) et survit à un release — ni l'un
                // ni l'autre ne fait vivre la session.
                if matches!(task_status(t), "done" | "pending") {
                    continue;
                }
                let aid = task_agent(t);
                if aid.starts_with("s-") {
                    referenced.insert(aid.to_string());
                }
                // Reviewer du pipeline plan (`reviewer_id`, distinct
                // d'`agent_id` = planner/worker) — sa session vit tant
                // que l'item n'est pas soldé.
                if let Some(rid) = t.get("reviewer_id").and_then(Value::as_str) {
                    if rid.starts_with("s-") {
                        referenced.insert(rid.to_string());
                    }
                }
            }
        }
        for p in prs {
            if !matches!(p.status.as_str(), "closed" | "merged") && !p.session.is_empty() {
                referenced.insert(p.session.clone());
            }
        }
        // Grâce de ménage : seules les entrées DANS la fenêtre
        // CLEAN_GRACE_S comptent — une entrée stale (session renommée
        // hors préfixe depuis, plus rien ne la purge) ne doit pas
        // bloquer la GC à jamais.
        let now = now_secs();
        let in_grace: HashSet<String> = {
            let st = self.orch.state.lock().unwrap();
            st.clean_since
                .iter()
                .filter(|(_, since)| now - **since < CLEAN_GRACE_S)
                .map(|(sid, _)| sid.clone())
                .collect()
        };
        // ---- Candidates.
        let mut deleted: Vec<String> = Vec::new();
        for m in &metas {
            let origin = m.origin.as_deref().unwrap_or("");
            let is_pr_review = origin == PR_REVIEW_ORIGIN;
            // Sans file, une session composant ne peut pas être prouvée
            // déréférencée (ses items vivent dans agentbacklog) — on
            // la garde ; les PRReview n'ont besoin que de pr_reviews.
            let is_component = origin == COMPONENT_ORIGIN && tasks.is_some();
            if !is_pr_review && !is_component {
                continue;
            }
            // Session suivie par l'entretien worker/plan : son ménage
            // (grâce, rm soldé, dead stale) reste le sien — la GC ne
            // couvre que ce que personne ne balaie.
            if !is_pr_review && gc_upkeep_visible(m, &cfg) {
                continue;
            }
            if referenced.contains(&m.id) || in_grace.contains(&m.id) || acwait_pending(&m.id) {
                continue;
            }
            info!(
                "orchestrateur: GC session orpheline {} « {} » ({}, état {})",
                m.id, m.title, origin, m.state
            );
            match self.h_delete_session(m.id.clone()).await {
                Ok(_) => deleted.push(m.id.clone()),
                Err(e) => {
                    // Déjà partie entre le snapshot et l'appel → rien
                    // à signaler ; sinon l'échec est loggé et la passe
                    // suivante retentera.
                    if self.get_session(&m.id).await.is_err() {
                        continue;
                    }
                    warn!("orchestrateur: GC rm {}: {e:#}", m.id);
                }
            }
            if is_pr_review {
                self.gc_review_worktree(&m.cwd, &cfg).await;
            }
        }
        if deleted.is_empty() {
            return;
        }
        // Purge des maps indexées par sid — sinon elles garderaient
        // des entrées mortes jusqu'au prochain retain de fin de cycle.
        {
            let mut live = self.orch.state.lock().unwrap();
            let gone: HashSet<&str> = deleted.iter().map(String::as_str).collect();
            for sid in &gone {
                live.nudges.remove(*sid);
                live.plan_nudges.remove(*sid);
                live.clean_since.remove(*sid);
                live.assignments.remove(*sid);
            }
            live.reviewers.retain(|_, v| !gone.contains(v.as_str()));
            let snap = live.clone();
            drop(live);
            self.orch_save(&snap);
        }
        info!(
            "orchestrateur: GC — {} session(s) orpheline(s) supprimée(s)",
            deleted.len()
        );
    }

    /// Worktree `pr-reviews/*` d'un PRReview supprimé : le cwd de la
    /// session EST le worktree — on tente le `worktree remove` depuis
    /// chaque clone connu (repo_workspaces ∪ workspace) jusqu'à ce que
    /// le dossier parte. Best-effort : un dossier étranger au modèle
    /// n'est jamais touché.
    async fn gc_review_worktree(self: &Arc<Self>, cwd: &str, cfg: &cfg::OrchCfg) {
        let p = Path::new(cwd);
        if !p.starts_with(pr_reviews_root()) || !p.is_dir() {
            return;
        }
        let mut wss: Vec<String> = vec![cfg.workspace.clone()];
        for v in cfg.repo_workspaces.values() {
            if !wss.contains(v) {
                wss.push(v.clone());
            }
        }
        for ws in wss {
            if !p.exists() {
                break;
            }
            remove_review_worktree(Path::new(&ws), p);
        }
    }
}

/// La session entre-t-elle dans le champ d'une file d'entretien ?
/// Legacy (`plan_review` off) : préfixe worker seul — les sessions
/// `Backlog-P*`/`Backlog-R*` résiduelles ne sont vues par PERSONNE.
/// Plan actif : les trois rôles (miroir de `is_backlog`/`role_of`).
fn gc_upkeep_visible(m: &SessionMeta, cfg: &cfg::OrchCfg) -> bool {
    if m.title.starts_with(&cfg.title_prefix) {
        return true;
    }
    cfg.plan_review && (m.title.starts_with(PLANNER_PREFIX) || m.title.starts_with(REVIEWER_PREFIX))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(title: &str) -> SessionMeta {
        SessionMeta {
            id: "s-x".into(),
            agent: "devin".into(),
            title: title.into(),
            cwd: "/workspace".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            native_session_id: None,
            mode: None,
            model: None,
            model_source: None,
            model_override_sid: None,
            supports_load: None,
            available_modes: Vec::new(),
            available_commands: Vec::new(),
            available_models: Vec::new(),
            advisor: None,
            advisor_models: Vec::new(),
            config_options: Vec::new(),
            pending_config: Default::default(),
            desired_config: Default::default(),
            state: "idle".into(),
            group: None,
            spawned_by: None,
            origin: Some(COMPONENT_ORIGIN.to_string()),
            context_used: None,
            context_size: None,
            last_error: None,
            wait: None,
        }
    }

    /// La GC ne doit sauter que ce que l'entretien suit réellement :
    /// préfixe worker toujours, préfixe planner/reviewer seulement en
    /// mode plan_review — sinon ces sessions ne sont vues par PERSONNE
    /// et doivent être collectables.
    #[test]
    fn upkeep_visible_selon_mode() {
        let mut c = cfg::OrchCfg::default();
        // Mode legacy (plan_review off) : seul le préfixe worker est
        // suivi — un Backlog-P/R résiduel est orphelin pour la GC.
        assert!(gc_upkeep_visible(&mk("Backlog-W12 #3 · t"), &c));
        assert!(!gc_upkeep_visible(&mk("Backlog-P2"), &c));
        assert!(!gc_upkeep_visible(&mk("Backlog-R4"), &c));
        assert!(!gc_upkeep_visible(&mk("renommée hors schéma"), &c));
        // Mode plan : les trois rôles relèvent de l'entretien.
        c.plan_review = true;
        assert!(gc_upkeep_visible(&mk("Backlog-P2"), &c));
        assert!(gc_upkeep_visible(&mk("Backlog-R4"), &c));
        // Un titre custom worker prime sur les lettres pipeline.
        let c2 = cfg::OrchCfg {
            title_prefix: "Work-".into(),
            plan_review: true,
            ..c
        };
        assert!(gc_upkeep_visible(&mk("Work-9"), &c2));
        assert!(!gc_upkeep_visible(&mk("Backlog-W9"), &c2));
    }
}
