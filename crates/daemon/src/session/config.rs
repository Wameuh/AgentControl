//! Configuration de la session : mode, modèle (avec application des
//! options de spawn en attente), groupe, titre, id natif, options de
//! config dynamiques de l'agent, reset de contexte (/clear).

use super::*;
use anyhow::{Context, Result};
use proto::EventKind;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::acp;

impl Session {
    pub async fn set_mode(&self, mode: &str) -> Result<()> {
        if !self.is_alive() {
            // Agent mort : mémoriser, rejoué au resume comme "mode".
            let mut m = self.meta.lock().unwrap();
            m.pending_config.insert("mode".into(), mode.into());
            m.mode = Some(mode.into());
            drop(m);
            self.save_meta();
            self.emit(EventKind::System {
                text: format!("mode={mode} mémorisé — appliqué à la reprise"),
            });
            return Ok(());
        }
        let sid = self
            .meta_snapshot()
            .native_session_id
            .context("no native session id")?;
        self.acp_request("session/set_mode", acp::session_set_mode_params(&sid, mode))
            .await?;
        Ok(())
    }

    /// Change de modèle : ACP session/set_model, sinon fallback sur la
    /// slash command "/model <m>" envoyée comme prompt.
    /// Agent mort → mémorisé dans pending_config["model"], rejoué au resume.
    pub async fn set_model(self: &Arc<Self>, model: &str) -> Result<()> {
        if !self.is_alive() {
            let mut m = self.meta.lock().unwrap();
            m.pending_config.insert("model".into(), model.into());
            m.model = Some(model.into());
            drop(m);
            self.save_meta();
            self.emit(EventKind::System {
                text: format!("model={model} mémorisé — appliqué à la reprise"),
            });
            return Ok(());
        }
        let sid = self
            .meta_snapshot()
            .native_session_id
            .context("no native session id")?;
        match self
            .acp_request("session/set_model", acp::session_set_model_params(&sid, model))
            .await
        {
            Ok(_) => {
                self.meta.lock().unwrap().model = Some(model.into());
                self.save_meta();
                self.emit(EventKind::System {
                    text: format!("model → {model}"),
                });
                Ok(())
            }
            Err(_) => {
                // Fallback : beaucoup d'agents exposent /model en slash command
                self.send_prompt(&format!("/model {model}"), false, false)
                    .await
                    .map(|_| ())
            }
        }
    }

    /// Applique le mode/modèle demandés à la création via ACP. Nécessaire
    /// pour les agents sans placeholder {mode}/{model} dans leur commande/env
    /// (cursor-agent acp, codex-acp n'acceptent aucun argument) — chez Devin
    /// le mode arrive via DEVIN_PERMISSION_MODE et vaut déjà la demande.
    /// La valeur n'est envoyée que si elle figure dans les options annoncées
    /// par l'agent : une valeur du domaine env (ex. "normal") qui n'est pas
    /// un id ACP est ignorée plutôt que d'échouer. À appeler uniquement au
    /// premier spawn (session/new) — au resume, pending_config fait foi.
    pub async fn apply_spawn_config(
        self: &Arc<Self>,
        mode: Option<&str>,
        model: Option<&str>,
    ) {
        for (id, want) in [("mode", mode), ("model", model)] {
            let Some(want) = want else { continue };
            if want.is_empty() {
                continue;
            }
            let (cur, known): (Option<String>, Vec<String>) = {
                let m = self.meta.lock().unwrap();
                let known = m
                    .config_options
                    .iter()
                    .find(|o| o.id == id)
                    .map(|o| o.options.iter().map(|x| x.value.clone()).collect())
                    .unwrap_or_else(|| match id {
                        "mode" => m.available_modes.clone(),
                        "model" => m.available_models.clone(),
                        _ => vec![],
                    });
                let cur = match id {
                    "mode" => m.mode.clone(),
                    _ => m.model.clone(),
                };
                (cur, known)
            };
            if !known.iter().any(|v| v == want) || cur.as_deref() == Some(want) {
                continue;
            }
            let r = match self.set_config_option(id, want).await {
                Ok(()) => Ok(()),
                Err(e) => {
                    let r2 = match id {
                        "mode" => self.set_mode(want).await,
                        _ => self.set_model(want).await.map(|_| ()),
                    };
                    r2.map_err(|e2| anyhow::anyhow!("{e:#} ; fallback: {e2:#}"))
                }
            };
            if let Err(e) = r {
                self.emit(EventKind::System {
                    text: format!("{id}={want} (demandé à la création) non appliqué: {e:#}"),
                });
            }
        }
    }

    /// Rebind l'ID natif (pour reprendre une autre session de l'agent).
    pub fn set_native_id(&self, id: &str) {
        self.meta.lock().unwrap().native_session_id = Some(id.into());
        self.save_meta();
    }

    /// Renomme : meta local + tentative session/rename chez l'agent (optionnel).
    pub async fn set_group(&self, group: Option<String>) {
        self.meta.lock().unwrap().group = group.filter(|g| !g.trim().is_empty());
        self.save_meta();
    }

    pub async fn rename(&self, title: &str) {
        self.meta.lock().unwrap().title = title.into();
        self.save_meta();
        if let Some(sid) = self.meta_snapshot().native_session_id {
            let _ = self
                .acp_request(
                    "session/rename",
                    json!({"sessionId": sid, "title": title}),
                )
                .await;
        }
    }

    /// ACP session/list — sessions natives de l'agent (reprendre ailleurs).
    pub async fn list_native_sessions(&self) -> Result<Value> {
        self.acp_request("session/list", json!({})).await
    }

    /// ACP session/set_config_option — le mécanisme de config de Devin
    /// (mode, model, …). Agent mort → la valeur est mémorisée dans
    /// `pending_config` et rejouée au prochain spawn (resume).
    /// Les clés `ac.*` sont interceptées côté démon (compact auto…) —
    /// elles ne partent JAMAIS vers l'agent.
    pub async fn set_config_option(&self, config_id: &str, value: &str) -> Result<()> {
        if let Some(key) = config_id.strip_prefix("ac.") {
            return self.set_daemon_option(key, value);
        }
        if !self.is_alive() {
            let opts = {
                let mut m = self.meta.lock().unwrap();
                m.pending_config.insert(config_id.into(), value.into());
                if let Some(opt) = m.config_options.iter_mut().find(|o| o.id == config_id) {
                    opt.current_value = value.into();
                }
                match config_id {
                    "mode" => m.mode = Some(value.into()),
                    "model" => m.model = Some(value.into()),
                    _ => {}
                }
                m.config_options.clone()
            };
            self.save_meta();
            self.emit(EventKind::System {
                text: format!("{config_id}={value} mémorisé — appliqué à la reprise"),
            });
            self.emit(EventKind::ConfigOptions { options: opts });
            return Ok(());
        }
        let sid = self
            .meta_snapshot()
            .native_session_id
            .context("no native session id")?;
        // Valeurs connues pour enrichir un éventuel refus de l'agent.
        // Pas de rejet local : la liste persistée peut être périmée après
        // une maj de l'agent (nouveaux modèles), l'agent fait foi.
        let known: Vec<String> = {
            let m = self.meta.lock().unwrap();
            m.config_options
                .iter()
                .find(|o| o.id == config_id)
                .map(|o| o.options.iter().map(|x| x.value.clone()).collect())
                .unwrap_or_default()
        };
        match self
            .acp_request(
                "session/set_config_option",
                json!({"sessionId": sid, "configId": config_id, "value": value}),
            )
            .await
        {
            Ok(_) => {
                // Sync la méta tout de suite — l'agent renverra sans doute un
                // config_option_update, mais l'UI doit rester cohérente même
                // s'il n'en envoie pas.
                let opts = {
                    let mut m = self.meta.lock().unwrap();
                    if let Some(opt) = m.config_options.iter_mut().find(|o| o.id == config_id) {
                        opt.current_value = value.into();
                    }
                    match config_id {
                        "mode" => m.mode = Some(value.into()),
                        "model" => m.model = Some(value.into()),
                        _ => {}
                    }
                    m.config_options.clone()
                };
                self.save_meta();
                self.emit(EventKind::ConfigOptions { options: opts });
                Ok(())
            }
            Err(e) => {
                let hint = if known.is_empty() {
                    String::new()
                } else {
                    format!(
                        " (valeurs connues: {}{})",
                        known.iter().take(8).cloned().collect::<Vec<_>>().join(", "),
                        if known.len() > 8 { ", …" } else { "" }
                    )
                };
                let msg = format!("set_config_option {config_id}={value}: {e:#}{hint}");
                self.emit(EventKind::Error {
                    message: msg.clone(),
                });
                Err(e).with_context(|| msg)
            }
        }
    }

    /// Option daemon-side `ac.<key>` (préfixe déjà consommé) — override
    /// de session persisté en `compact.json`, effectif immédiatement
    /// (compact_policy relit l'override à chaque usage_update).
    ///   `ac.compact_enabled`   on|off
    ///   `ac.compact_at_tokens` N|0 (règle off)|off (hérité)
    ///   `ac.compact_at_pct`    0<p≤1|0 (règle off)|off (hérité)
    /// Écriture disque AVANT la mutation mémoire : une erreur est un
    /// no-op réel (pas d'override actif non persisté). Puis reset de la
    /// baseline live : si le seuil franchi était déjà dépassé (compact
    /// réarmé, seuil durci), le prochain usage_update compte comme un
    /// franchissement — sinon `was` resterait vrai pour toujours.
    fn set_daemon_option(&self, key: &str, value: &str) -> Result<()> {
        // Le guard tient toute l'opération : deux setcfg concurrents ne
        // peuvent laisser mémoire et compact.json diverger (dernier
        // swap = dernier rename, dans le même ordre).
        let mut guard = self.compact_override.lock().unwrap();
        let mut o = guard.clone();
        match key {
            "compact_enabled" => o.enabled = Some(parse_on_off(value)?),
            "compact_at_tokens" => o.at_tokens = parse_opt_u64(value)?,
            "compact_at_pct" => o.at_pct = parse_opt_pct(value)?,
            _ => anyhow::bail!("option démon inconnue : ac.{key}"),
        }
        // .tmp + rename : pas de compact.json tronqué si le démon meurt
        // en pleine écriture (même convention que pinned.json).
        let tmp = self.dir.join("compact.json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&o)?)
            .context("compact.json illisible en écriture")?;
        std::fs::rename(&tmp, self.dir.join("compact.json"))?;
        *guard = o;
        drop(guard);
        self.usage_seen
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let eff = self.compact_policy();
        self.emit(EventKind::System {
            text: format!(
                "ac.{key}={value} — seuil compact effectif : {}",
                describe_policy(&eff)
            ),
        });
        Ok(())
    }

    /// /clear : journal + blobs vidés, méta recalée pour une session
    /// native neuve (native_id None — si le respawn échoue, un resume
    /// ultérieur ne rechargerait pas l'ancien contexte par surprise).
    /// L'agent doit déjà être arrêté (kill_for_respawn).
    pub fn clear_context(&self) -> Result<()> {
        // Fragments d'un tour interrompu et demandes en attente de
        // l'ancien contexte : ils n'ont plus de sens après le reset.
        self.cur_text.lock().unwrap().take();
        self.tool_calls.lock().unwrap().clear();
        self.pending_agent.lock().unwrap().clear();
        self.reset_text_anchor();
        // Prévient les abonnés AVANT le reset : émis dans l'ancienne
        // numérotation (dernier seq +1), il passe leur filtre et leur
        // fait jeter l'état affiché — sinon les events du nouveau
        // cycle (seqs reparties à 1) seraient droppés comme déjà vus.
        self.emit_ephemeral(EventKind::SessionCleared {});
        self.journal.reset(&self.dir)?;
        {
            let mut m = self.meta.lock().unwrap();
            m.native_session_id = None;
            m.context_used = None;
        }
        self.save_meta();
        self.emit(EventKind::System {
            text: "contexte effacé — nouvelle session agent".into(),
        });
        Ok(())
    }

    /// Charge `compact.json` (override session du seuil compact) —
    /// absent/vide/illisible → CompactPartial vide (héritage intégral),
    /// un fichier corrompu n'empêche pas l'ouverture. Les valeurs hors
    /// borne (édit manuel) sont neutralisées à la lecture via apply_to
    /// — un warn ici les signale, pareil que pour le TOML. Appelé par
    /// `Session::open`.
    pub(super) fn open_compact_override(dir: &std::path::Path) -> Mutex<CompactPartial> {
        let o: CompactPartial = std::fs::read_to_string(dir.join("compact.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        if let Some(v) = o.at_pct {
            if !(0.0..=1.0).contains(&v) {
                tracing::warn!(
                    "{}: compact.json at_pct={v} hors borne (0<p≤1) — règle % ignorée",
                    dir.display()
                );
            }
        }
        Mutex::new(o)
    }

}

/// `on|true|1|yes` → vrai ; `off|false|0|no` → faux.
fn parse_on_off(v: &str) -> Result<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "1" | "yes" => Ok(true),
        "off" | "false" | "0" | "no" => Ok(false),
        _ => anyhow::bail!("booléen attendu (on|off), reçu « {v} »"),
    }
}

/// `off|none|` → None (retour à l'hérité) ; `0` → Some(0) = règle
/// désactivée (« décocher » le seuil hérité) ; sinon entier > 0.
fn parse_opt_u64(v: &str) -> Result<Option<u64>> {
    let v = v.trim();
    if matches!(v.to_ascii_lowercase().as_str(), "off" | "none" | "") {
        return Ok(None);
    }
    let n: u64 = v
        .parse()
        .with_context(|| format!("entier attendu, reçu « {v} »"))?;
    Ok(Some(n))
}

/// `off|none|` → None (hérité) ; `0` → règle % désactivée ; sinon ]0,1].
fn parse_opt_pct(v: &str) -> Result<Option<f64>> {
    let v = v.trim();
    if matches!(v.to_ascii_lowercase().as_str(), "off" | "none" | "") {
        return Ok(None);
    }
    let p: f64 = v
        .parse()
        .with_context(|| format!("fraction attendue (0≤p≤1), reçu « {v} »"))?;
    if !(0.0..=1.0).contains(&p) {
        anyhow::bail!("pourcentage hors borne (0≤p≤1) : {p}");
    }
    Ok(Some(p))
}

/// Libellé lisible du seuil effectif pour les accusés `ac.*`.
fn describe_policy(p: &CompactPolicy) -> String {
    if !p.enabled {
        return "désactivé".into();
    }
    match (p.at_tokens, p.at_pct) {
        (None, None) => "actif mais aucun seuil posé (inerte)".into(),
        (t, pct) => [
            t.map(|n| format!("{n} tokens")),
            pct.map(|f| format!("{}%", (f * 100.0).round() as u64)),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ou "),
    }
}
