//! Handlers des RPC composants managés (docs/components-integration.md
//! §2.3) : liste/infos lecture + cycle de vie `process` + tail de log.
//! Toute la mécanique est dans `crate::components` — ici on ne fait que
//! router la requête et formater la réponse.

use super::*;

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
    /// (internal/external/tool refusés côté Components).
    pub(super) async fn h_component_start(self: &Arc<Self>, name: String) -> Result<Value> {
        let st = self.components.start(&name).await?;
        Ok(json!({ "name": name, "state": st.state }))
    }

    /// `ComponentStop{name}` → {name, state} — SIGTERM gracieux.
    pub(super) async fn h_component_stop(self: &Arc<Self>, name: String) -> Result<Value> {
        let st = self.components.stop(&name).await?;
        Ok(json!({ "name": name, "state": st.state }))
    }

    /// `ComponentRestart{name}` → {name, state} — stop + start.
    pub(super) async fn h_component_restart(self: &Arc<Self>, name: String) -> Result<Value> {
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
}
