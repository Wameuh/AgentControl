//! Handlers des RPC composants managés (docs/components-integration.md
//! §2.3) : liste/infos lecture + cycle de vie `process` + tail de log
//! + édition des specs + catalogue d'intégration. Toute la mécanique
//! est dans `crate::components` — ici on ne fait que router la requête
//! et formater la réponse.

use super::*;
use proto::ComponentSpec;

impl Daemon {
    /// `ListComponents` — items {name, kind, state, pid?, autostart,
    /// health?, uptime_s?, restarts, last_exit?, description?, source}
    /// pour specs components.toml + internal déclarés en code.
    pub(super) async fn h_list_components(self: &Arc<Self>) -> Result<Value> {
        Ok(json!(self.components.list().await))
    }

    /// `ComponentInfo{name}` — spec complète + état détaillé.
    pub(super) async fn h_component_info(self: &Arc<Self>, name: String) -> Result<Value> {
        self.components.info(&name).await
    }

    /// `ComponentStart{name}` → {name, state} — `process` seulement
    /// (internal/external/tool refusés côté Components). Exception
    /// `backlog-orchestrator` : « démarrer » l'interne = basculer
    /// l'orchestrateur natif à chaud (même effet qu'`OrchestratorSet`).
    pub(super) async fn h_component_start(self: &Arc<Self>, name: String) -> Result<Value> {
        if name == crate::server::orch::COMPONENT_NAME {
            self.orch.set_enabled(true);
            info!("orchestrateur: démarré via component_start");
            return Ok(json!({ "name": name, "state": self.orch.component_state() }));
        }
        let st = self.components.start(&name).await?;
        Ok(json!({ "name": name, "state": st.state }))
    }

    /// `ComponentStop{name}` → {name, state} — SIGTERM gracieux.
    /// `backlog-orchestrator` : arrête la boucle (runtime seulement).
    pub(super) async fn h_component_stop(self: &Arc<Self>, name: String) -> Result<Value> {
        if name == crate::server::orch::COMPONENT_NAME {
            self.orch.set_enabled(false);
            info!("orchestrateur: arrêté via component_stop");
            return Ok(json!({ "name": name, "state": self.orch.component_state() }));
        }
        let st = self.components.stop(&name).await?;
        Ok(json!({ "name": name, "state": st.state }))
    }

    /// `ComponentRestart{name}` → {name, state} — stop + start.
    /// `backlog-orchestrator` : restart = (ré)activation idempotente.
    pub(super) async fn h_component_restart(self: &Arc<Self>, name: String) -> Result<Value> {
        if name == crate::server::orch::COMPONENT_NAME {
            self.orch.set_enabled(true);
            info!("orchestrateur: redémarré via component_restart");
            return Ok(json!({ "name": name, "state": self.orch.component_state() }));
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
    pub(super) async fn h_component_upsert(
        self: &Arc<Self>,
        name: String,
        spec: ComponentSpec,
    ) -> Result<Value> {
        let st = self.components.upsert(&name, spec).await?;
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

    /// `ComponentInstall{name}` — exécute la recette de l'entrée
    /// (steps auditées : run argv fixe, write_file borné HOME,
    /// require_file, upsert_spec) → {installed, name, state, notes}.
    pub(super) async fn h_component_install(self: &Arc<Self>, name: String) -> Result<Value> {
        self.components.install(&name).await
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
}
