//! Failover provider de l'orchestrateur (item #193).
//!
//! Changer un profil `*_agent` à chaud (`acctl comp set`, UI) ne
//! touche que les PROCHAINS spawns : les sessions déjà en backoff
//! `rate_limit` (`last_error.kind == "rate_limit"`, `error_hold` armé)
//! retenteraient indéfiniment sur l'ancien provider — et le mode
//! sériel (`rate_limited_count > 0`) gèlerait toute la pipeline.
//!
//! À chaque cycle, une session du composant (`origin:
//! component:backlog-orchestrator`) remplissant TOUS ces critères
//! est remplacée par une session équivalente sur le profil courant
//! de son rôle :
//!
//!   - `state == "error"` et `last_error.kind == "rate_limit"` — les
//!     holds `model`/`auth`/`other` relèvent d'une intervention
//!     manuelle, jamais d'un respawn automatique ;
//!   - `meta.agent` ≠ agent effectif du rôle (`agent_or`) — c'est le
//!     drift post-`comp set` qui déclenche le failover. Self-healing
//!     par construction : un restart entre le set et le tick, ou un
//!     spawn manqué, est rattrapé au cycle suivant sans file
//!     d'événements à rejouer ;
//!   - pas de waiter `ac-wait` vivant (`acwait_pending`) — une session
//!     endormie sous waiter doit continuer à être réveillée par le
//!     ping sur l'ancien provider ;
//!   - sessions actives (`working`, `compacting`, `waiting_input`,
//!     `idle`, `asleep` hors ac-wait) : jamais ciblées — elles
//!     finissent leur tour sur l'ancien provider.
//!
//! Par rôle :
//!
//!   - **worker** (`Backlog-W*`) : items `wip` possédés réassignés au
//!     nouveau SID (`--reassign --force`), PRs ouvertes du registre
//!     réattribuées (`prs.track` — `via: manual`, attribution
//!     délibérée), puis spawn d'un worker de reprise dont le prompt
//!     récapitule items + PRs + worktrees.
//!   - **planner/implementer** (`Backlog-P*`) : items `wip` réassignés
//!     au remplaçant — prompt planner (phase `planning`, avec le
//!     chemin du brouillon `PLAN_FILE` retrouvé dans le journal de
//!     l'ancienne session) ou implémenteur (phase `implementing`).
//!     `plan_marker_seq` de l'item est réinitialisé : le journal du
//!     nouveau SID repart à seq 1 et le watermark de l'ancien le
//!     rendrait invisible. Les items `plan_review`/`reviewing`
//!     gardent l'ancien `agent_id` — les chemins « planner mort »
//!     existants (verdict → release/re-spawn) les résorbent.
//!   - **reviewer** (`Backlog-R*`) : items `reviewing` →
//!     `--unclaim-review` (retour `plan_review`, le dispatch respawn
//!     un reviewer sur le nouveau profil), mappings `st.reviewers` /
//!     watermarks du SID abandonné — pas de spawn direct, le pipeline
//!     pourvoit.
//!   - **PR reviewer** (`PRReview-*`) : géré dans
//!     `orch_pr_review_tick` (orch_review.rs) — l'entrée
//!     `pr_reviews` repasse en `retry_wait` immédiat, la passe retry
//!     relance sur le nouveau profil.
//!
//! NB : le failover n'est PAS automatique côté provider — si le
//! nouveau profil partage le même compte/quota, le rate limit se
//! reproduira (et la session restera en backoff normal sur le bon
//! profil, sans nouvelle suppression : `meta.agent` correspond
//! désormais à l'agent effectif). C'est le choix du profil cible qui
//! appartient à l'utilisateur.

use super::orch::*;
use super::orch_plan;
use super::*;
use proto::SessionMeta;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use tracing::{info, warn};

/// Rôle pipeline d'une session pour le failover — déduit du titre
/// (même convention que `role_of`, ordre reviewer → planner →
/// worker pour rester correct sous un `title_prefix` custom
/// recouvrant les préfixes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailRole {
    Worker,
    Planner,
    Reviewer,
}

/// La session est-elle un candidat failover ? Retourne le rôle et
/// l'agent effectif attendu — `None` sinon. Voir l'en-tête de module
/// pour la liste exhaustive des critères.
fn fail_role<'a>(m: &SessionMeta, cfg: &'a cfg::OrchCfg) -> Option<(FailRole, &'a str)> {
    if m.origin.as_deref() != Some(COMPONENT_ORIGIN)
        || m.state != "error"
        || !m
            .last_error
            .as_ref()
            .is_some_and(|e| e.kind == "rate_limit")
        || acwait_pending(&m.id)
    {
        return None;
    }
    let (role, effective) = if m.title.starts_with(orch_plan::REVIEWER_PREFIX) {
        (FailRole::Reviewer, cfg.agent_or(&cfg.reviewer_agent))
    } else if m.title.starts_with(orch_plan::PLANNER_PREFIX) {
        (FailRole::Planner, cfg.agent_or(&cfg.planner_agent))
    } else if m.title.starts_with(&cfg.title_prefix) {
        (FailRole::Worker, cfg.agent_or(&cfg.worker_agent))
    } else {
        return None;
    };
    if effective.is_empty() || m.agent == effective {
        return None;
    }
    Some((role, effective))
}

/// Prompt du worker de reprise : le contexte de l'ancienne session
/// est perdu — items `wip` et PRs sont réinjectés explicitement.
const WORKER_FAILOVER_PROMPT: &str = r#"[Session creee automatiquement par le service backlog-orchestrator — worker de REPRISE apres changement de profil provider (failover rate_limit), pas par un agent humain ni une session parente.]

Tu es un worker sur le repo (cwd {WS}) — tu remplaces une session detruite car bloquee en rate_limit sur l'ancien profil. Son contexte est perdu ; le travail est recoverable via la file AgentBacklog, les worktrees et le registre PR.

Commence par lire {WS}/AGENTS.md — le workflow PR/CI qu'il decrit est obligatoire.

## ITEMS A REPRENDRE (wip, deja assignes a toi)
{ITEMS}

## PR OUVERTES A SUIVRE (deja attribuees a toi dans le registre)
{PRS}

## MISSION
1. Pour chaque item ci-dessus : verifie son etat (`agentbacklog --list --json`), retrouve ou cree le worktree dedie (`git worktree add ~/worktrees/ac-<feat> -b <branch> origin/next_release` — JAMAIS dans le checkout principal), continue le travail et mene la PR selon le cycle complet de AGENTS.md.
2. Continue jusqu'a ce que tes items soient `--done` et tes PRs soldees (mergees ou cloturees), puis ARRETE-toi purement et simplement. Tu NE LANCES JAMAIS `agentbacklog --pop` toi-meme : l'orchestrateur dispatche le travail suivant (un worker = un item ; un pop contournerait la revue de plan en mode plan_review et l'invariant « contexte frais par item »).
3. `agentbacklog --done <ID> --agent-id "$AGENTCONTROL_SESSION"` UNIQUEMENT quand ta PR est effectivement MERGED ; blocage insurmontable : `agentbacklog --release <ID> --agent-id "$AGENTCONTROL_SESSION"`, explique la raison, puis arrete.

Regles absolues : ne jamais stopper/redemarrer/toucher le daemon agentcontrol-daemon ; aucun secret commite ; push uniquement sur ta branche de PR ; nettoie ton worktree a la fin ; reponds en francais."#;

/// Note ajoutée aux prompts planner/implementer pour expliciter la
/// reprise — le reste du prompt standard s'applique tel quel.
const FAILOVER_NOTE: &str = r#"

## REPRISE (failover provider)
Tu reprends le role d'une session detruite car bloquee en rate_limit sur l'ancien profil — son contexte est perdu. L'item t'est reassigne : n'appelle ni --claim ni --pop pour celui-ci, poursuis directement le travail."#;

impl Daemon {
    /// Passe failover provider — appelée en tête de `orch_cycle`
    /// (avant le dispatch, pour que le pipeline profite du profil
    /// libéré dans le même tick). Traite workers, planners et
    /// reviewers ; les `PRReview-*` relèvent de `orch_review.rs`.
    pub(crate) async fn orch_failover_tick(
        self: &Arc<Self>,
        bin: &str,
        cfg: &cfg::OrchCfg,
        ws: &Path,
        tasks: &[Task],
        metas: &[SessionMeta],
        prs: &[proto::PrEntry],
        st: &mut OrchPersist,
    ) {
        // Snapshot des cibles : les suppressions/spawns mutent
        // `self.sessions` mais pas `metas` — itérer la liste figée.
        let targets: Vec<(SessionMeta, FailRole, String)> = metas
            .iter()
            .filter_map(|m| fail_role(m, cfg).map(|(r, a)| (m.clone(), r, a.to_string())))
            .collect();
        for (m, role, effective) in targets {
            if !self.orch.is_enabled() {
                return;
            }
            info!(
                "orchestrateur: failover provider — {} ({}) rate_limit sur « {} » → « {} »",
                m.title, m.id, m.agent, effective
            );
            match role {
                FailRole::Worker => {
                    self.failover_worker(bin, cfg, ws, &m, tasks, prs, st).await
                }
                FailRole::Planner => {
                    self.failover_planner(bin, cfg, ws, &m, tasks, st).await
                }
                FailRole::Reviewer => self.failover_reviewer(bin, ws, &m, tasks, st).await,
            }
        }
    }

    /// Suppression + purge des maps d'état indexées par SID — commune
    /// à tous les rôles (nudges, clean_since, assignments ; les
    /// watermarks par item sont gérés par l'appelant).
    async fn failover_rm(self: &Arc<Self>, sid: &str, st: &mut OrchPersist) {
        if let Err(e) = self.h_delete_session(sid.to_string()).await {
            warn!("orchestrateur: failover rm {sid}: {e:#}");
        }
        st.nudges.remove(sid);
        st.plan_nudges.remove(sid);
        st.clean_since.remove(sid);
        st.assignments.remove(sid);
        st.reviewers.retain(|_, v| v.as_str() != sid);
    }

    /// Worker `Backlog-W*` : réassigne les items `wip`, réattribue
    /// les PRs ouvertes du registre, spawn le remplaçant, rm l'ancien.
    async fn failover_worker(
        self: &Arc<Self>,
        bin: &str,
        cfg: &cfg::OrchCfg,
        ws: &Path,
        m: &SessionMeta,
        tasks: &[Task],
        prs: &[proto::PrEntry],
        st: &mut OrchPersist,
    ) {
        let sid = m.id.as_str();
        let new_agent = cfg.agent_or(&cfg.worker_agent).to_string();
        let my_wip: Vec<Task> = tasks
            .iter()
            .filter(|t| task_agent(t) == sid && task_status(t) == "wip")
            .cloned()
            .collect();
        let my_prs: Vec<proto::PrEntry> = prs
            .iter()
            .filter(|p| p.session == sid && !matches!(p.status.as_str(), "closed" | "merged"))
            .cloned()
            .collect();
        if my_wip.is_empty() && my_prs.is_empty() {
            // Rien à reprendre : suppression simple — le prochain
            // dispatch pourvoira si du travail existe.
            self.failover_rm(sid, st).await;
            return;
        }
        let items_txt = if my_wip.is_empty() {
            "(aucun — items soldes)".to_string()
        } else {
            my_wip
                .iter()
                .map(|t| format!("- #{} {}", task_id(t), orch_plan::item_body(t)))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let prs_txt = if my_prs.is_empty() {
            "(aucune)".to_string()
        } else {
            my_prs
                .iter()
                .map(|p| {
                    format!(
                        "- {}#{} {}",
                        p.repo,
                        p.pr,
                        p.title.clone().unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        // Workspace du remplaçant : celui de l'item wip à reprendre
        // (tag repo → repo_workspaces) ; worker sans wip (PR seules)
        // → workspace par défaut comme avant.
        let fo_ws = my_wip
            .first()
            .map(|t| cfg.workspace_for(task_repo(t)).to_string())
            .unwrap_or_else(|| cfg.workspace.clone());
        let prompt = orch_plan::fill(
            WORKER_FAILOVER_PROMPT,
            &[
                ("WS", fo_ws.clone()),
                ("ITEMS", items_txt),
                ("PRS", prs_txt),
            ],
        );
        // plan_review : les workers implémentent → implement_mode ;
        // sinon le mode historique du rôle.
        let mode = if cfg.plan_review {
            cfg.implement_mode.as_str()
        } else {
            cfg.mode.as_str()
        };
        st.counter += 1;
        let Some(nsid) = self
            .plan_spawn(
                st.counter,
                &cfg.title_prefix,
                &new_agent,
                mode,
                prompt,
                &fo_ws,
                cfg,
            )
            .await
        else {
            // Spawn refusé : on NE touche pas à l'ancienne session —
            // elle reste en backoff et le prochain tick réessaiera
            // (spawn refusé = souvent quota/profile → attendre).
            warn!(
                "orchestrateur: failover worker {sid} — spawn {new_agent} refusé, session conservée"
            );
            return;
        };
        for t in &my_wip {
            let id = task_id(t).to_string();
            let (rc, _o, e) =
                run_out(&[bin, "--reassign", &id, "--agent-id", &nsid, "--force"], ws)
                    .await
                    .unwrap_or((-1, String::new(), "run_out failed".into()));
            if rc != 0 {
                warn!("orchestrateur: failover --reassign #{id} → {nsid} rc={rc} {e}");
            }
        }
        // Réattribution registre PR : `session: Some` = upsert
        // délibéré (via: manual) — l'ancienne session est morte, la
        // nouvelle porte la suite (surveillance merge incluse).
        for p in &my_prs {
            let _ = self.prs.track(crate::prs::TrackIn {
                session: Some(nsid.clone()),
                repo: p.repo.clone(),
                pr: p.pr,
                forge: p.forge.clone(),
                host: p.host.clone(),
                change_id: p.change_id.clone(),
                url: Some(p.url.clone()),
                branch: p.branch.clone(),
                title: p.title.clone(),
                status: None,
                note: None,
                sync_fails: None,
                review: None,
                ci: None,
            });
        }
        info!(
            "orchestrateur: failover worker {sid} → {nsid} ({} items, {} PRs)",
            my_wip.len(),
            my_prs.len()
        );
        self.failover_rm(sid, st).await;
    }

    /// Planner/implémenteur `Backlog-P*` : items `wip` → spawn du
    /// remplaçant (planner en phase planning, implémenteur en phase
    /// implementing) + `--reassign`. Items en review : l'ancien
    /// `agent_id` reste — les chemins « planner mort » du pipeline
    /// les résorbent (verdict → release/re-spawn).
    async fn failover_planner(
        self: &Arc<Self>,
        bin: &str,
        cfg: &cfg::OrchCfg,
        ws: &Path,
        m: &SessionMeta,
        tasks: &[Task],
        st: &mut OrchPersist,
    ) {
        let sid = m.id.as_str();
        let new_agent = cfg.agent_or(&cfg.planner_agent).to_string();
        let my_wip: Vec<Task> = tasks
            .iter()
            .filter(|t| task_agent(t) == sid && task_status(t) == "wip")
            .cloned()
            .collect();
        if my_wip.is_empty() {
            self.failover_rm(sid, st).await;
            return;
        }
        // Comme pour le worker : si un spawn est refusé (quota,
        // profile), l'ancienne session est CONSERVÉE — elle reste en
        // backoff et le prochain tick réessaiera pour les items non
        // encore remplacés. Suppression seulement quand tout est
        // réassigné (les items déjà partis ne lui appartiennent plus).
        let mut all_replaced = true;
        for t in &my_wip {
            if !self.orch.is_enabled() {
                return;
            }
            let key = task_id(t).to_string();
            let implementing = st.item_phase.get(&key).is_some_and(|p| p == "implementing");
            let (tpl, mode) = if implementing {
                (orch_plan::IMPLEMENTER_PROMPT, cfg.implement_mode.as_str())
            } else {
                (orch_plan::PLANNER_PROMPT, cfg.planner_mode.as_str())
            };
            // Workspace routé de l'item repris (tag repo →
            // repo_workspaces) — le remplaçant travaille dans le bon
            // checkout.
            let iws = cfg.workspace_for(task_repo(t)).to_string();
            let mut prompt = orch_plan::fill(
                tpl,
                &[
                    ("ID", key.clone()),
                    ("ITEM", orch_plan::item_body(t)),
                    ("WS", iws.clone()),
                    (
                        "ROUND",
                        t.get("review_round")
                            .and_then(Value::as_u64)
                            .unwrap_or(0)
                            .to_string(),
                    ),
                    (
                        "PLAN",
                        orch_plan::clip_inline(
                            orch_plan::task_plan_content(t),
                            orch_plan::PLAN_INLINE_MAX,
                        ),
                    ),
                ],
            );
            // Brouillon de plan déjà déposé par l'ancienne session :
            // le nouveau planner repart de ce fichier plutôt que de
            // zéro (marqueur PLAN_FILE retrouvé dans son journal).
            if !implementing {
                if let Ok(s) = self.get_session(sid).await {
                    if let Some((_seq, path)) = self.plan_marker(&s, m).await {
                        prompt.push_str(&format!(
                            "\n\nBrouillon de plan deja depose par la session precedente : {} — verifie-le et continue/affine au lieu de repartir de zero.",
                            path.display()
                        ));
                    }
                }
            }
            prompt.push_str(FAILOVER_NOTE);
            st.counter += 1;
            let Some(nsid) = self
                .plan_spawn(
                    st.counter,
                    orch_plan::PLANNER_PREFIX,
                    &new_agent,
                    mode,
                    prompt,
                    &iws,
                    cfg,
                )
                .await
            else {
                warn!(
                    "orchestrateur: failover planner {sid} — spawn {new_agent} refusé pour #{key}, session conservée pour retry"
                );
                all_replaced = false;
                continue;
            };
            let (rc, _o, e) =
                run_out(&[bin, "--reassign", &key, "--agent-id", &nsid, "--force"], ws)
                    .await
                    .unwrap_or((-1, String::new(), "run_out failed".into()));
            if rc != 0 {
                warn!("orchestrateur: failover --reassign #{key} → {nsid} rc={rc} {e}");
                self.plan_rm_session(&nsid, st).await;
                all_replaced = false;
                continue;
            }
            // Le journal du nouveau SID repart à seq 1 : sans reset,
            // le watermark de l'ancienne session masquerait ses
            // futurs marqueurs PLAN_FILE (plan_review) / verdict.
            st.plan_marker_seq.remove(&key);
            st.reviewer_seq.remove(&key);
            st.planner_seq.remove(&key);
            info!("orchestrateur: failover planner {sid} → {nsid} pour #{key}");
        }
        if all_replaced {
            self.failover_rm(sid, st).await;
        }
    }

    /// Reviewer `Backlog-R*` : items `reviewing` → `--unclaim-review`
    /// (le dispatch respawn un reviewer sur le nouveau profil au
    /// prochain tick) ; mappings et watermarks du SID abandonné.
    /// Pas de spawn direct — le pipeline pourvoit.
    async fn failover_reviewer(
        self: &Arc<Self>,
        bin: &str,
        ws: &Path,
        m: &SessionMeta,
        tasks: &[Task],
        st: &mut OrchPersist,
    ) {
        let sid = m.id.as_str();
        let mut keys: Vec<String> = Vec::new();
        for t in tasks.iter().filter(|t| {
            task_status(t) == "reviewing"
                && t.get("reviewer_id").and_then(Value::as_str) == Some(sid)
        }) {
            let key = task_id(t).to_string();
            let (rc, _o, e) = run_out(&[bin, "--unclaim-review", &key], ws)
                .await
                .unwrap_or((-1, String::new(), "run_out failed".into()));
            if rc != 0 {
                warn!("orchestrateur: failover --unclaim-review #{key} rc={rc} {e}");
            }
            keys.push(key);
        }
        // Items mappés à ce reviewer hors review active (plan_review
        // entre rounds, wip deny/conditional) : le mapping saute —
        // le dispatch respawne un reviewer frais — et les watermarks
        // sont purgés : le journal du nouveau SID repart à seq 1,
        // l'ancien watermark masquerait ses marqueurs de verdict.
        for (key, rsid) in st.reviewers.iter() {
            if rsid.as_str() == sid && !keys.contains(key) {
                keys.push(key.clone());
            }
        }
        for key in &keys {
            st.reviewers.remove(key);
            st.reviewer_seq.remove(key);
            st.planner_seq.remove(key);
            st.review_started.remove(key);
            st.review_retry.remove(key);
        }
        self.failover_rm(sid, st).await;
    }
}

#[cfg(test)]
mod tests {
    use super::super::orch_review::PR_REVIEW_ORIGIN;
    use super::*;
    use proto::LastError;

    fn meta(id: &str, title: &str, agent: &str) -> SessionMeta {
        SessionMeta {
            id: id.into(),
            agent: agent.into(),
            title: title.into(),
            cwd: String::new(),
            created_at: String::new(),
            native_session_id: None,
            mode: None,
            model: None,
            model_source: None,
            model_override_sid: None,
            supports_load: None,
            available_modes: vec![],
            available_commands: vec![],
            available_models: vec![],
            config_options: vec![],
            pending_config: Default::default(),
            desired_config: Default::default(),
            state: "error".into(),
            group: None,
            spawned_by: None,
            origin: Some(COMPONENT_ORIGIN.into()),
            context_used: None,
            context_size: None,
            last_error: Some(LastError {
                message: "rl".into(),
                kind: "rate_limit".into(),
                at: String::new(),
                count: 1,
                retry_at: None,
            }),
            wait: None,
        }
    }

    fn cfg() -> cfg::OrchCfg {
        cfg::OrchCfg {
            agent: "devin".into(),
            worker_agent: "cursor".into(),
            planner_agent: String::new(),
            reviewer_agent: "claude".into(),
            pr_reviewer_agent: String::new(),
            title_prefix: "Backlog-W".into(),
            ..Default::default()
        }
    }

    #[test]
    fn fail_role_rate_limit_sur_perime() {
        let c = cfg();
        // Worker sur « cursor » = worker_agent → pas de drift.
        let m = meta("s-1", "Backlog-W1 #42 · t", "cursor");
        assert!(fail_role(&m, &c).is_none());
        // Worker sur « devin » (ancien worker_agent ou fallback agent)
        // → worker, effectif cursor.
        let m = meta("s-1", "Backlog-W1 #42 · t", "devin");
        assert_eq!(fail_role(&m, &c), Some((FailRole::Worker, "cursor")));
        // Planner : planner_agent vide → fallback agent=devin ; session
        // sur cursor → planner, effectif devin.
        let m = meta("s-2", "Backlog-P3", "cursor");
        assert_eq!(fail_role(&m, &c), Some((FailRole::Planner, "devin")));
        // Reviewer : reviewer_agent=claude ; session sur devin → drift.
        let m = meta("s-3", "Backlog-R2", "devin");
        assert_eq!(fail_role(&m, &c), Some((FailRole::Reviewer, "claude")));
    }

    #[test]
    fn fail_role_ignore_hold_etranger_et_actif() {
        let c = cfg();
        // Hold model → jamais failover.
        let mut m = meta("s-1", "Backlog-W1", "devin");
        m.last_error = Some(LastError {
            message: "x".into(),
            kind: "model".into(),
            at: String::new(),
            count: 1,
            retry_at: None,
        });
        assert!(fail_role(&m, &c).is_none());
        // Session active (working) → pas de drift même si agent diffère.
        let mut m = meta("s-1", "Backlog-W1", "devin");
        m.state = "working".into();
        assert!(fail_role(&m, &c).is_none());
        // Origin manager → exclue.
        let mut m = meta("s-1", "Backlog-W1", "devin");
        m.origin = Some(MANAGER_ORIGIN.into());
        assert!(fail_role(&m, &c).is_none());
        // Origin PRReview → hors périmètre de cette passe (gérée par
        // orch_review) — le titre PRReview-* ne matcherait aucun
        // préfixe de toute façon.
        let mut m = meta("s-4", "PRReview-12", "devin");
        m.origin = Some(PR_REVIEW_ORIGIN.into());
        assert!(fail_role(&m, &c).is_none());
    }
}
