//! Handlers RPC `review_queue_*` + `component_set_params` de la
//! file de revue — voir `rq` pour la mécanique.

use super::rq::{
    migrate_state_file, now_secs, resolve_repo, same_dir, thread_key, thread_tid, unsettled,
    ReqState, RqPair, RqRequest, RqThread,
};
use super::*;
use proto::ReviewRepoSpec;
use serde_json::{json, Value};
use std::collections::BTreeMap;

impl Daemon {
    /// `review_queue_request` — ingest d'une demande de revue.
    /// `by` = session demandeuse (None = pas de steer résultat).
    /// Validation immédiate des dépôts (chemin canonique, branche
    /// résolue) : une spec invalide est refusée AVANT d'entrer en
    /// file — le demandeur sait que rien ne sera revu.
    pub(super) async fn h_review_queue_request(
        self: &Arc<Self>,
        by: Option<String>,
        repos: Vec<ReviewRepoSpec>,
        instructions: Option<String>,
    ) -> Result<Value> {
        anyhow::ensure!(!repos.is_empty(), "review_queue_request: no repos");
        anyhow::ensure!(
            repos.len() <= 16,
            "review_queue_request: too many repos (>16)"
        );
        let mut resolved = Vec::with_capacity(repos.len());
        let mut pairs = Vec::with_capacity(repos.len());
        for spec in &repos {
            let r = resolve_repo(spec)?;
            pairs.push(RqPair {
                path: r.path.clone(),
                branch: r.branch.clone(),
            });
            resolved.push(r);
        }
        // Déduplique les paires identiques (même checkout+branche
        // demandé deux fois = une seule entrée de thread).
        pairs.sort_by(|a, b| (&a.path, &a.branch).cmp(&(&b.path, &b.branch)));
        pairs.dedup();
        let key = thread_key(&pairs);
        let by = by.unwrap_or_default();
        if !by.is_empty() {
            // Le steer résultat suppose une session vivante ou
            // reprenable — un id inconnu est refusé tout de suite
            // (l'appelant peut reprendre sans `session`).
            self.get_session(&by)
                .await
                .with_context(|| format!("unknown requester session « {by} »"))?;
        }
        let id = proto::new_id("rq");
        let run_dir = self.rq.state_dir().join("runs").join(&id);
        std::fs::create_dir_all(&run_dir)
            .with_context(|| format!("creating run dir {}", run_dir.display()))?;
        let now = now_secs();
        let title_prefix = self.rq.cfg.read().unwrap().title_prefix.clone();
        let mut st = self.rq.state.lock().unwrap();
        // Round optimiste = dernier round dispatché + 1 — le numéro
        // définitif est recalculé au dispatch (deux requêtes en file
        // sur le même thread ne doivent pas partager le même round).
        let round = st.threads.get(&key).map(|t| t.round + 1).unwrap_or(1);
        let tid = thread_tid(&key);
        let title = format!("{}{}-r{}", title_prefix, tid, round);
        st.threads.entry(key.clone()).or_insert_with(|| RqThread {
            key: key.clone(),
            tid: tid.clone(),
            pairs: pairs.clone(),
            session: None,
            title: title.clone(),
            round: 0,
            active: None,
            last_active: now,
            reap_checked_at: 0.0,
            created_at: now,
        });
        let req = RqRequest {
            id: id.clone(),
            by,
            repos: resolved,
            instructions: instructions.unwrap_or_default(),
            thread: key.clone(),
            session: None,
            round,
            state: ReqState::Queued,
            created_at: now,
            dispatched_at: None,
            compact_at: None,
            run_dir: run_dir.to_string_lossy().to_string(),
            snaps: Vec::new(),
            result: None,
            error: None,
        };
        st.requests.insert(id.clone(), req);
        drop(st);
        // Persistance remontée à l'intake : si l'écriture échoue, la
        // requête est retirée de l'état mémoire (sinon elle serait
        // dispatchée puis perdue au redémarrage — acquittée mais
        // jamais revue) et l'appelant voit l'erreur.
        if let Err(e) = self.rq.save() {
            let mut st = self.rq.state.lock().unwrap();
            st.requests.remove(&id);
            let orphan = !st.requests.values().any(|r| r.thread == key);
            if orphan {
                st.threads.remove(&key);
            }
            drop(st);
            return Err(e.context("persisting review request"));
        }
        self.rq.kick();
        Ok(json!({
            "request": id,
            "thread": key,
            "round": round,
            "state": "queued",
        }))
    }

    /// `review_queue_status` — file + threads + requêtes récentes.
    pub(super) async fn h_review_queue_status(self: &Arc<Self>) -> Result<Value> {
        let mut v = self.rq.status_json();
        v["config"] = rq_cfg::cfg_json(&self.rq.cfg.read().unwrap(), self.rq.is_enabled());
        Ok(v)
    }

    /// `review_queue_config` — get (`patch` absent) ou set validé +
    /// persisté dans `[review_queue]` de config.toml (même mécanique
    /// que `orchestrator_config`/`component_set_params`).
    pub(super) async fn h_review_queue_config(
        self: &Arc<Self>,
        patch: Option<BTreeMap<String, Value>>,
    ) -> Result<Value> {
        match patch {
            None => Ok(json!({
                "config": rq_cfg::cfg_json(&self.rq.cfg.read().unwrap(), self.rq.is_enabled()),
                "status": self.rq.status_json(),
            })),
            Some(p) => {
                let applied = self.rq_patch(p)?;
                info!("review-queue: config persisted ({applied:?})");
                Ok(json!({
                    "config": rq_cfg::cfg_json(&self.rq.cfg.read().unwrap(), self.rq.is_enabled()),
                    "applied": applied,
                    "persisted": true,
                    "status": self.rq.status_json(),
                }))
            }
        }
    }

    /// `component_set_params{name:"review-queue"}` / acctl comp set —
    /// valeurs chaînes → même `apply_patch` que la RPC (un seul
    /// chemin de validation).
    pub(super) async fn h_rq_params_set(
        self: &Arc<Self>,
        params: std::collections::HashMap<String, String>,
    ) -> Result<Value> {
        let patch: BTreeMap<String, Value> = params
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        let applied = self.rq_patch(patch)?;
        info!("review-queue: params persisted ({applied:?})");
        Ok(json!({
            "name": rq_cfg::COMPONENT_NAME,
            "state": self.rq.component_state(),
            "params": rq_cfg::params_values(&self.rq.cfg.read().unwrap()),
            "config_required": [],
            "rendered": true,
            "restarted": false,
        }))
    }

    /// Patch commun : clone → apply_patch → persist → commit cfg
    /// vivante → flip `enabled` si basculé (runtime + durable) sinon
    /// kick. Synchronise les sets concurrents via le write-lock.
    fn rq_patch(self: &Arc<Self>, patch: BTreeMap<String, Value>) -> Result<Vec<String>> {
        if patch.is_empty() {
            return Ok(Vec::new());
        }
        let (applied, flip, en) = {
            let mut guard = self.rq.cfg.write().unwrap();
            let mut next = guard.clone();
            let applied = rq_cfg::apply_patch(&mut next, &patch)?;
            // `state_dir` à chaud : refusé tant que la file a du
            // travail non soldé (leurs run_dir/intake vivent dans
            // l'ancien dossier) ; à file vide, le fichier d'état
            // est migré pour qu'un restart avant le prochain save()
            // ne perde pas l'historique.
            let old_dir = guard.state_dir(&self.root);
            let new_dir = next.state_dir(&self.root);
            if !same_dir(&old_dir, &new_dir) {
                let live = unsettled(&self.rq.state.lock().unwrap());
                anyhow::ensure!(
                    live == 0,
                    "state_dir change refused: {live} unsettled \
                     request(s) — drain the queue first"
                );
                migrate_state_file(&old_dir, &new_dir)?;
            }
            rq_cfg::persist(&self.root, &next)?;
            let flip = patch.contains_key("enabled") && next.enabled != self.rq.is_enabled();
            let en = next.enabled;
            *guard = next;
            (applied, flip, en)
        };
        if flip {
            self.rq.set_enabled(en);
        }
        self.rq.kick();
        Ok(applied)
    }

    /// `review_queue_reap` — passe manuelle du reaper (le tick
    /// périodique fait la même chose). → `{reaped, checked}`.
    pub(super) async fn h_review_queue_reap(self: &Arc<Self>) -> Result<Value> {
        let reaped = self.rq_reap_pass(true).await?;
        Ok(json!({ "reaped": reaped, "checked": true }))
    }
}
