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

    /// Valeurs connues d'un config_id : options ACP (`config_options`,
    /// value + name affiché) complétées par `available_models` /
    /// `available_modes`. Vide = l'agent n'a rien annoncé → les appelants
    /// restent permissifs (pas de garde sur information absente).
    fn known_config_values(&self, config_id: &str) -> Vec<(String, String)> {
        let m = self.meta.lock().unwrap();
        let mut out: Vec<(String, String)> = m
            .config_options
            .iter()
            .find(|o| o.id == config_id || o.category == config_id)
            .map(|o| {
                o.options
                    .iter()
                    .map(|x| (x.value.clone(), x.name.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let extra: &[String] = match config_id {
            "model" => &m.available_models,
            "mode" => &m.available_modes,
            _ => &[],
        };
        for v in extra {
            if !out.iter().any(|(x, _)| x == v) {
                out.push((v.clone(), v.clone()));
            }
        }
        out
    }

    /// Options annoncées pour `config_id` (`config_options` uniquement —
    /// `id` ou `category` matchant). Vide = l'agent n'a pas déclaré ce
    /// configId : `session/set_config_option` est alors interdit par la
    /// spec (le champ ACP `models` implique `session/set_model`, pas un
    /// configId « model » inventé).
    fn option_values(&self, config_id: &str) -> Vec<(String, String)> {
        let m = self.meta.lock().unwrap();
        m.config_options
            .iter()
            .find(|o| o.id == config_id || o.category == config_id)
            .map(|o| {
                o.options
                    .iter()
                    .map(|x| (x.value.clone(), x.name.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Résout `want` contre les valeurs annoncées de `config_id` (exact,
    /// casse, préfixe univoque) puis applique `set_config_option`.
    /// `Ok(Some(v))` valeur résolue (peut différer de `want`), `Ok(None)`
    /// configId non annoncé → à router ailleurs (session/set_model…),
    /// `Err` rien n'a été émis.
    async fn resolve_then_set(
        self: &Arc<Self>,
        config_id: &str,
        want: &str,
    ) -> Result<Option<String>> {
        let known = self.option_values(config_id);
        if known.is_empty() {
            return Ok(None);
        }
        let v = match resolve_config_value(&known, want) {
            Resolve::Hit(v) => v,
            Resolve::Ambiguous(c) => {
                anyhow::bail!(
                    "{config_id} ambigu « {want} » — candidats : {}",
                    c.join(", ")
                )
            }
            Resolve::Unknown => {
                anyhow::bail!(
                    "{config_id} inconnu « {want} » — valeurs : {}",
                    join_known(&known)
                )
            }
        };
        self.set_config_option(config_id, &v).await?;
        Ok(Some(v))
    }

    /// Accusé de changement de modèle — mentionne la résolution quand la
    /// valeur appliquée diffère de la demande.
    fn emit_model_applied(&self, applied: &str, wanted: &str) {
        let text = if applied == wanted {
            format!("model → {applied}")
        } else {
            format!("model → {applied} (résolu depuis « {wanted} »)")
        };
        self.emit(EventKind::System { text });
    }

    /// Change de modèle. Route ACP selon ce que l'agent expose :
    /// config_option "model" annoncée → `session/set_config_option`
    /// (Devin) ; sinon `session/set_model` (champ ACP `models`), avec
    /// fallback sur la slash command "/model <m>".
    /// La valeur est résolue contre la liste annoncée AVANT envoi :
    /// préfixe/nom univoque accepté ("swe-2" → "swe-2-high"), ambigu →
    /// candidats listés, inconnu → erreur avec les valeurs valides —
    /// jamais de valeur hors liste ni de Invalid params ACP brut.
    /// Agent mort → mémorisé brut dans pending_config["model"], rejoué
    /// au resume (le replay repasse par ce même chemin gardé ; la
    /// mémorisation reste permissive, la liste pouvant être périmée
    /// tant que l'agent n'a pas re-annoncé ses options).
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
        // Garde-fou + routage. Une config_option "model" annoncée impose
        // set_config_option ; available_models seuls → chaîne legacy
        // (un configId non annoncé est interdit par la spec). Liste vide
        // (agent sans annonce) → passthrough, l'agent fait foi.
        let wanted = model;
        let has_opt = !self.option_values("model").is_empty();
        let model = if has_opt {
            match self.resolve_then_set("model", model).await {
                Ok(Some(v)) => {
                    self.emit_model_applied(&v, model);
                    return Ok(());
                }
                Ok(None) => unreachable!("option non vide"),
                Err(e) => {
                    self.emit(EventKind::Error {
                        message: format!("{e:#}"),
                    });
                    return Err(e);
                }
            }
        } else {
            let known = self.known_config_values("model");
            if known.is_empty() {
                model.to_string()
            } else {
                match resolve_config_value(&known, model) {
                    Resolve::Hit(v) => v,
                    Resolve::Ambiguous(c) => {
                        let msg = format!(
                            "model ambigu « {model} » — candidats : {}",
                            c.join(", ")
                        );
                        self.emit(EventKind::Error {
                            message: msg.clone(),
                        });
                        anyhow::bail!("{msg}")
                    }
                    Resolve::Unknown => {
                        let msg = format!(
                            "model inconnu « {model} » — valeurs : {}",
                            join_known(&known)
                        );
                        self.emit(EventKind::Error {
                            message: msg.clone(),
                        });
                        anyhow::bail!("{msg}")
                    }
                }
            }
        };
        let model = model.as_str();
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
                self.emit_model_applied(model, wanted);
                Ok(())
            }
            Err(_) => {
                // Fallback : beaucoup d'agents exposent /model en slash command.
                // meta.model écrit AVANT l'envoi : la valeur relue par
                // h_set_model reflète la demande même si le prompt part en file.
                self.meta.lock().unwrap().model = Some(model.into());
                self.save_meta();
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
            let cur: Option<String> = {
                let m = self.meta.lock().unwrap();
                match id {
                    "mode" => m.mode.clone(),
                    _ => m.model.clone(),
                }
            };
            let known = self.known_config_values(id);
            // Déjà la valeur courante → rien à faire.
            if cur.as_deref() == Some(want) {
                continue;
            }
            // Résolution tolérante : la valeur demandée à la création
            // ("swe-2", "SWE-2"…) est acceptée si elle désigne une seule
            // option annoncée. Liste vide (agent sans annonce) → pas de
            // tentative ni de bruit. Non résolu : warning explicite pour
            // `model` seulement — `mode` reste un skip silencieux (la
            // valeur arrive souvent du domaine env, ex. "normal" via
            // DEVIN_PERMISSION_MODE, qui n'est pas un id ACP : avertir à
            // chaque spawn serait du bruit régressif).
            let want2 = if known.is_empty() {
                continue;
            } else {
                match resolve_config_value(&known, want) {
                    Resolve::Hit(v) => v,
                    Resolve::Ambiguous(c) => {
                        if id == "model" {
                            self.emit(EventKind::System {
                                text: format!(
                                    "model={want} (demandé à la création) ambigu — candidats : {}",
                                    c.join(", ")
                                ),
                            });
                        }
                        continue;
                    }
                    Resolve::Unknown => {
                        if id == "model" {
                            self.emit(EventKind::System {
                                text: format!(
                                    "model={want} (demandé à la création) absent des options annoncées — valeurs : {}",
                                    join_known(&known)
                                ),
                            });
                        }
                        continue;
                    }
                }
            };
            let want = want2.as_str();
            // La demande résolue peut déjà être la valeur courante
            // ("swe-2" demandé alors que swe-2-high est actif).
            if cur.as_deref() == Some(want) {
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
        // Garde-fou : si le configId est annoncé dans config_options avec
        // une liste, la valeur est résolue contre elle (exact, casse,
        // préfixe univoque) — jamais de valeur absente de la liste envoyée
        // à l'agent. ConfigId non annoncé/liste vide → passthrough : la
        // spec interdit un configId inventé, la liste persistée peut être
        // périmée après une maj de l'agent — l'agent fait foi (les
        // valeurs connues enrichissent alors un éventuel refus).
        let known = self.option_values(config_id);
        let value = if known.is_empty() {
            value.to_string()
        } else {
            match resolve_config_value(&known, value) {
                Resolve::Hit(v) => v,
                Resolve::Ambiguous(c) => {
                    let msg = format!(
                        "set_config_option {config_id} ambigu « {value} » — candidats : {}",
                        c.join(", ")
                    );
                    self.emit(EventKind::Error {
                        message: msg.clone(),
                    });
                    anyhow::bail!("{msg}")
                }
                Resolve::Unknown => {
                    let msg = format!(
                        "set_config_option {config_id} inconnu « {value} » — valeurs : {}",
                        join_known(&known)
                    );
                    self.emit(EventKind::Error {
                        message: msg.clone(),
                    });
                    anyhow::bail!("{msg}")
                }
            }
        };
        let value = value.as_str();
        let known: Vec<String> = known.into_iter().map(|(v, _)| v).collect();
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

/// Résolution tolérante d'une valeur de config contre la liste annoncée
/// par l'agent (`(value, name affiché)`). Ordre : exact sur `value` →
/// égalité insensible à la casse sur `value`/`name` → préfixe insensible
/// à la casse (Hit si un seul candidat, Ambiguous sinon) → Unknown.
#[derive(Debug, PartialEq)]
enum Resolve {
    Hit(String),
    Ambiguous(Vec<String>),
    Unknown,
}

fn resolve_config_value(known: &[(String, String)], want: &str) -> Resolve {
    if let Some((v, _)) = known.iter().find(|(v, _)| v == want) {
        return Resolve::Hit(v.clone());
    }
    let w = want.to_ascii_lowercase();
    let eq: Vec<String> = known
        .iter()
        .filter(|(v, n)| v.to_ascii_lowercase() == w || n.to_ascii_lowercase() == w)
        .map(|(v, _)| v.clone())
        .collect();
    if eq.len() == 1 {
        return Resolve::Hit(eq[0].clone());
    }
    if !eq.is_empty() {
        return Resolve::Ambiguous(eq);
    }
    let pre: Vec<String> = known
        .iter()
        .filter(|(v, n)| {
            v.to_ascii_lowercase().starts_with(&w) || n.to_ascii_lowercase().starts_with(&w)
        })
        .map(|(v, _)| v.clone())
        .collect();
    match pre.len() {
        0 => Resolve::Unknown,
        1 => Resolve::Hit(pre[0].clone()),
        _ => Resolve::Ambiguous(pre),
    }
}

/// « a, b, c, … » — tronqué à 8 entrées pour les messages d'erreur.
fn join_known(known: &[(String, String)]) -> String {
    let mut vals: Vec<&str> = known.iter().map(|(v, _)| v.as_str()).collect();
    vals.dedup();
    let mut s = vals.iter().take(8).cloned().collect::<Vec<_>>().join(", ");
    if vals.len() > 8 {
        s.push_str(", …");
    }
    s
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

#[cfg(test)]
mod tests {
    use super::{join_known, resolve_config_value, Resolve};

    fn known(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(v, n)| (v.to_string(), n.to_string()))
            .collect()
    }

    #[test]
    fn resolve_exact() {
        let k = known(&[("swe-2-high", "SWE-2 High"), ("adaptive", "Adaptive")]);
        assert_eq!(
            resolve_config_value(&k, "adaptive"),
            Resolve::Hit("adaptive".into())
        );
        // L'exact prime même si c'est aussi un préfixe d'une autre valeur.
        let k2 = known(&[("swe-2", ""), ("swe-2-high", "")]);
        assert_eq!(
            resolve_config_value(&k2, "swe-2"),
            Resolve::Hit("swe-2".into())
        );
    }

    #[test]
    fn resolve_case_insensitive() {
        let k = known(&[("swe-2-high", "SWE-2 High"), ("adaptive", "")]);
        assert_eq!(
            resolve_config_value(&k, "SWE-2-High"),
            Resolve::Hit("swe-2-high".into())
        );
        // Égalité sur le nom affiché.
        assert_eq!(
            resolve_config_value(&k, "swe-2 high"),
            Resolve::Hit("swe-2-high".into())
        );
    }

    #[test]
    fn resolve_prefix() {
        // Préfixe univoque → résolu ("swe-2" → "swe-2-high" quand la liste
        // ACP n'annonce que cette variante).
        let k = known(&[("swe-2-high", ""), ("adaptive", ""), ("claude-fable-5-1-medium", "")]);
        assert_eq!(
            resolve_config_value(&k, "swe-2"),
            Resolve::Hit("swe-2-high".into())
        );
        assert_eq!(
            resolve_config_value(&k, "SWE-2"),
            Resolve::Hit("swe-2-high".into())
        );
        // Préfixe univoque sur le nom affiché.
        let k2 = known(&[("gpt-6[medium]", "GPT 6 medium"), ("gpt-6[high]", "GPT 6 high")]);
        assert_eq!(
            resolve_config_value(&k2, "gpt 6 m"),
            Resolve::Hit("gpt-6[medium]".into())
        );
    }

    #[test]
    fn resolve_ambiguous() {
        let k = known(&[("swe-2-high", ""), ("swe-2-medium", ""), ("adaptive", "")]);
        assert_eq!(
            resolve_config_value(&k, "swe-2"),
            Resolve::Ambiguous(vec!["swe-2-high".into(), "swe-2-medium".into()])
        );
        // Ambiguïté possible aussi via le préfixe sur le nom affiché.
        let k2 = known(&[("m-a", "SWE Max"), ("m-b", "SWE Medium")]);
        assert_eq!(
            resolve_config_value(&k2, "swe"),
            Resolve::Ambiguous(vec!["m-a".into(), "m-b".into()])
        );
    }

    #[test]
    fn resolve_unknown() {
        let k = known(&[("swe-2-high", ""), ("adaptive", "")]);
        assert_eq!(resolve_config_value(&k, "gpt-9"), Resolve::Unknown);
        // Liste vide = pas de résolution possible (passthrough côté appelant).
        assert_eq!(resolve_config_value(&[], "x"), Resolve::Unknown);
    }

    /// Garde héritée au replay : une session morte dont l'agent avait
    /// annoncé ses options résout la valeur mémorisée ("swe-2" →
    /// "swe-2-high") ; inconnu → erreur explicite, rien n'est rejoué
    /// tel quel vers l'agent (chemins agent-mort : la mémorisation
    /// brute reste permissive, la résolution intervient au replay).
    #[tokio::test]
    async fn dead_session_pending_replay_guard() {
        let (s, _dir) = crate::session::tests::tmp_session("resolve-pending");
        {
            let mut m = s.meta.lock().unwrap();
            m.config_options = vec![proto::ConfigOption {
                id: "model".into(),
                current_value: "adaptive".into(),
                options: vec![
                    proto::ConfigOpt {
                        value: "adaptive".into(),
                        ..Default::default()
                    },
                    proto::ConfigOpt {
                        value: "swe-2-high".into(),
                        name: "SWE-2 High".into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }];
        }
        let r = s.resolve_then_set("model", "swe-2").await.unwrap();
        assert_eq!(r.as_deref(), Some("swe-2-high"));
        let m = s.meta_snapshot();
        assert_eq!(m.model.as_deref(), Some("swe-2-high"));
        assert_eq!(
            m.pending_config.get("model").map(|x| x.as_str()),
            Some("swe-2-high")
        );
        assert!(s.resolve_then_set("model", "gpt-9").await.is_err());
        let m = s.meta_snapshot();
        assert_eq!(
            m.pending_config.get("model").map(|x| x.as_str()),
            Some("swe-2-high")
        );
    }

    #[test]
    fn join_known_truncates() {
        let k = known(&[
            ("a", ""), ("b", ""), ("c", ""), ("d", ""), ("e", ""),
            ("f", ""), ("g", ""), ("h", ""), ("i", ""),
        ]);
        assert_eq!(join_known(&k), "a, b, c, d, e, f, g, h, …");
        assert_eq!(join_known(&k[..2]), "a, b");
    }
}
