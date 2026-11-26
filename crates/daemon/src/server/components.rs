//! Handlers des RPC composants managés (docs/components-integration.md
//! §2.3) : liste/infos lecture, cycle de vie `process`, tail de log,
//! édition des specs et catalogue d'intégration. Toute la mécanique
//! est dans `crate::components` — ici on ne fait que router la requête
//! et formater la réponse.

use super::*;
use proto::ComponentSpec;

impl Daemon {
    /// `ListComponents` — items {name, kind, state, pid?, autostart,
    /// health?, uptime_s?, restarts, last_exit?, description?, source}
    /// pour specs components.toml + internal déclarés en code.
    pub(super) async fn h_list_components(self: &Arc<Self>) -> Result<Value> {
        // Le composant interne `agents` lit la table vivante des
        // profils — hot-reload pour qu'un [agents.*] ajouté après le
        // boot y apparaisse sans restart.
        self.reload_agent_profiles().await;
        // Idem `prs-registry` : un [[forges]] édité à la main dans
        // config.toml apparaît dans le panneau sans restart.
        self.reload_forges().await;
        Ok(json!(self.components.list().await))
    }

    /// `ComponentInfo{name}` — spec complète + état détaillé.
    pub(super) async fn h_component_info(self: &Arc<Self>, name: String) -> Result<Value> {
        self.reload_agent_profiles().await;
        self.reload_forges().await;
        self.components.info(&name).await
    }

    /// `ComponentStart{name}` → {name, state} — `process` seulement
    /// (internal/external/tool refusés côté Components). Exception
    /// `backlog-orchestrator` : « démarrer » l'interne = basculer
    /// l'orchestrateur natif à chaud (même effet qu'`OrchestratorSet`).
    pub(super) async fn h_component_start(self: &Arc<Self>, name: String) -> Result<Value> {
        if name == crate::server::orch::COMPONENT_NAME {
            self.orch.set_enabled(true);
            info!("orchestrator: started via component_start");
            return Ok(json!({ "name": name, "state": self.orch.component_state() }));
        }
        if name == crate::server::rq::COMPONENT_NAME {
            self.rq.set_enabled(true);
            self.rq.kick();
            info!("review-queue: started via component_start");
            return Ok(json!({ "name": name, "state": self.rq.component_state() }));
        }
        let st = self.components.start(&name).await?;
        Ok(json!({ "name": name, "state": st.state }))
    }

    /// `ComponentStop{name}` → {name, state} — SIGTERM gracieux.
    /// `backlog-orchestrator` : arrête la boucle (runtime seulement).
    pub(super) async fn h_component_stop(self: &Arc<Self>, name: String) -> Result<Value> {
        if name == crate::server::orch::COMPONENT_NAME {
            self.orch.set_enabled(false);
            info!("orchestrator: stopped via component_stop");
            return Ok(json!({ "name": name, "state": self.orch.component_state() }));
        }
        if name == crate::server::rq::COMPONENT_NAME {
            self.rq.set_enabled(false);
            info!("review-queue: stopped via component_stop");
            return Ok(json!({ "name": name, "state": self.rq.component_state() }));
        }
        let st = self.components.stop(&name).await?;
        Ok(json!({ "name": name, "state": st.state }))
    }

    /// `ComponentRestart{name}` → {name, state} — stop + start.
    /// `backlog-orchestrator` : restart = (ré)activation idempotente.
    pub(super) async fn h_component_restart(self: &Arc<Self>, name: String) -> Result<Value> {
        if name == crate::server::orch::COMPONENT_NAME {
            self.orch.set_enabled(true);
            info!("orchestrator: restarted via component_restart");
            return Ok(json!({ "name": name, "state": self.orch.component_state() }));
        }
        if name == crate::server::rq::COMPONENT_NAME {
            self.rq.set_enabled(true);
            self.rq.kick();
            info!("review-queue: restarted via component_restart");
            return Ok(json!({ "name": name, "state": self.rq.component_state() }));
        }
        let st = self.components.restart(&name).await?;
        Ok(json!({ "name": name, "state": st.state }))
    }

    /// `ComponentLogs{name, offset?, limit?}` — tail paginé du log du
    /// composant : {data(b64), offset, size, truncated}.
    pub(super) async fn h_component_logs(
        self: &Arc<Self>,
        name: String,
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> Result<Value> {
        self.components.logs(&name, offset, limit).await
    }

    /// `ComponentUpsert{name, spec}` — insère/remplace la spec déclarée
    /// (réécriture atomique de components.toml) → {name, state}. Un
    /// `process` vivant garde sa spec courante jusqu'au prochain
    /// (re)start, explicite ou supervisé.
    /// Surface d'édition utilisateur : sur un composant adossé au
    /// catalogue, la spec interne est scellée (`upsert_edit`) — seuls
    /// autostart/description et les params déclarés sont mutables.
    pub(super) async fn h_component_upsert(
        self: &Arc<Self>,
        name: String,
        spec: ComponentSpec,
    ) -> Result<Value> {
        let st = self.components.upsert_edit(&name, spec).await?;
        Ok(json!({ "name": name, "state": st.state }))
    }

    /// `ComponentRemove{name}` — retire la spec (stop préalable d'un
    /// `process` vivant) → {name, removed}.
    pub(super) async fn h_component_remove(self: &Arc<Self>, name: String) -> Result<Value> {
        self.components.remove(&name).await?;
        Ok(json!({ "name": name, "removed": true }))
    }

    /// `ComponentCatalog{refresh?}` — entrées du catalogue embarqué :
    /// métadonnées + installed/version détectés + latest_version /
    /// latest_notes upstream (cache ~5 min, offline → champs null).
    pub(super) async fn h_component_catalog(
        self: &Arc<Self>,
        refresh: Option<bool>,
    ) -> Result<Value> {
        Ok(json!(
            self.components.catalog(refresh.unwrap_or(false)).await
        ))
    }

    /// `ComponentInstall{name, params?}` — exécute la recette de
    /// l'entrée (steps auditées : run argv fixe, write_file borné HOME,
    /// require_file, upsert_spec) avec les params collectés au
    /// dialogue client → {installed, name, state, notes, rendered,
    /// config_required}.
    pub(super) async fn h_component_install(
        self: &Arc<Self>,
        name: String,
        params: Option<std::collections::HashMap<String, String>>,
    ) -> Result<Value> {
        self.components.install(&name, params).await
    }

    /// `ComponentSetParams{name, params}` — édite les paramètres
    /// déclarés du composant (seule surface de config utilisateur) :
    /// fusion + validation + re-rendu + restart si le process tourne
    /// → {name, state, params(masqués), config_required, rendered,
    /// restarted}. Exceptions `backlog-orchestrator`/`agents`/
    /// `prs-registry` : les internes n'ont pas de spec toml — leurs
    /// params mappent sur la config orchestrateur (orch.rs — même
    /// validation que `OrchestratorConfig`), les profils `[agents.*]`
    /// (agents_cfg.rs) et les `[[forges]]` (forges_cfg.rs).
    pub(super) async fn h_component_set_params(
        self: &Arc<Self>,
        name: String,
        params: std::collections::HashMap<String, String>,
    ) -> Result<Value> {
        if name == crate::server::orch::COMPONENT_NAME {
            return self.h_orch_params_set(params).await;
        }
        if name == crate::server::agents_cfg::COMPONENT_NAME {
            return self.h_agents_params_set(params).await;
        }
        if name == crate::server::forges_cfg::COMPONENT_NAME {
            return self.h_forges_params_set(params).await;
        }
        if name == crate::server::rq::COMPONENT_NAME {
            return self.h_rq_params_set(params).await;
        }
        self.components.set_params(&name, &params).await
    }

    /// `ComponentQuery{name, query}` — exécute la requête de données
    /// déclarée `spec.queries[query]` (argv fixe, jamais de shell) →
    /// {code, data(b64), size, truncated, stderr?(b64)}.
    pub(super) async fn h_component_query(
        self: &Arc<Self>,
        name: String,
        query: String,
    ) -> Result<Value> {
        self.components.query(&name, &query).await
    }

    /// `PluginViewToken{name}` — émet un token de vue plugin éphémère
    /// (TTL ~5 min, scopé au composant) pour la route HTTP
    /// `/plugin/<name>/` et le bootstrap `ready` du bridge postMessage —
    /// jamais le `web.token` global. → {component, token, ttl_s}.
    pub(super) async fn h_plugin_view_token(self: &Arc<Self>, name: String) -> Result<Value> {
        let (token, ttl) = self.components.issue_view_token(&name).await?;
        Ok(json!({ "component": name, "token": token, "ttl_s": ttl }))
    }
}
