//! Configuration de la session : mode, modèle (avec application des
//! options de spawn en attente), groupe, titre, id natif, options de
//! config dynamiques de l'agent, reset de contexte (/clear).

use super::*;
use anyhow::{Context, Result};
use proto::EventKind;
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::acp;

impl Session {
    pub async fn set_mode(&self, mode: &str) -> Result<()> {
        if !self.is_alive() {
            // Agent mort : mémoriser, rejoué au resume comme "mode".
            // Demande explicite → purge la marque de refus définitif :
            // le replay la retentera et l'event de drop sera réémis
            // (le geste utilisateur ne doit pas disparaître en silence).
            self.config_rejected.lock().unwrap().remove("mode");
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
        // Sync la méta au succès comme set_config_option : un agent qui
        // applique sans renvoyer de mode_changed laisserait meta.mode
        // stale — le reconcile post-handshake re-pousserait alors
        // l'ancienne valeur à chaque réveil.
        self.meta.lock().unwrap().mode = Some(mode.into());
        self.save_meta();
        self.config_rejected.lock().unwrap().remove("mode");
        Ok(())
    }

    /// Valeurs connues d'un config_id : options ACP (`config_options`,
    /// value + name affiché) complétées par `available_models` /
    /// `available_modes`. Vide = l'agent n'a rien annoncé → les appelants
    /// restent permissifs (pas de garde sur information absente).
    pub(super) fn known_config_values(&self, config_id: &str) -> Vec<(String, String)> {
        let m = self.meta.lock().unwrap();
        crate::server::orch_cfg::meta_config_values(&m, config_id)
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
            self.config_rejected.lock().unwrap().remove("model");
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
                // Action explicite de réparation : un changement de
                // modèle lève la retenue d'erreur (kind model/auth —
                // c'est précisément le geste attendu pour débloquer).
                // Hold levé → drain : busy conservé vrai sans runner
                // figerait la file (garnie du prompt fautif re-enfilé).
                if self.clear_error() && self.has_queued() {
                    self.spawn_drain();
                }
                self.config_rejected.lock().unwrap().remove("model");
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
                    .map(|_| {
                        self.config_rejected.lock().unwrap().remove("model");
                    })
            }
        }
    }

    /// Applique le mode/modèle demandés à la création via ACP. Nécessaire
    /// pour les agents sans placeholder {mode}/{model} dans leur commande/env
    /// (cursor-agent acp, codex-acp n'acceptent aucun argument) — chez Devin
    /// le mode arrive via DEVIN_PERMISSION_MODE et vaut déjà la demande.
    /// La valeur n'est envoyée que si elle figure dans les options annoncées
    /// par l'agent : une valeur du domaine env (ex. "normal") qui n'est pas
    /// un id ACP est ignorée plutôt que d'échouer.
    /// La valeur résolue est TOUJOURS poussée, même quand elle semble déjà
    /// courante : `meta.mode`/`meta.model` sont pré-seedés avec la valeur
    /// demandée (h_new_session) et le `currentValue` rapporté peut n'être
    /// qu'un écho de l'env de spawn (DEVIN_MODEL) — ni l'un ni l'autre ne
    /// prouve que l'inférence applique le modèle (le store natif peut en
    /// montrer un autre). `set_config_option` est idempotent : le push
    /// inconditionnel est ce qui garantit l'application, et il écrit le
    /// modèle persisté côté agent pour les prochains resume.
    /// Appelée au premier spawn (session/new) ET aux respawns qui portent
    /// un spawn_cfg (resume/clear : le modèle persisté côté agent peut
    /// différer de la demande). `quiet` = ré-affirmation de respawn,
    /// best-effort et silencieuse : le push n'est tenté que si le
    /// configId est annoncé ET la valeur résoluble (une valeur hors
    /// config_options — catalogue env DEVIN_MODEL, défaut du compte —
    /// est skippée sans warning : rien à signaler, elle ne peut pas être
    /// posée), sans fallback set_mode/set_model ni events Error ; à la
    /// création (quiet=false) la demande explicite non résoluble garde
    /// son warning et le fallback reste câblé.
    pub async fn apply_spawn_config(
        self: &Arc<Self>,
        mode: Option<&str>,
        model: Option<&str>,
        quiet: bool,
    ) {
        for (id, want) in [("mode", mode), ("model", model)] {
            let Some(want) = want else { continue };
            if want.is_empty() {
                continue;
            }
            if quiet {
                // Ré-affirmation de respawn, best-effort et silencieuse :
                // seul un configId annoncé ET une valeur résoluble
                // autorisent le push (une valeur hors config_options —
                // catalogue env, défaut du compte — ne peut de toute
                // façon pas être posée). Pas de fallback set_mode/
                // set_model : un "/model <m>" en filé pendant spawning
                // partirait comme un vrai tour utilisateur au drain.
                let opts = self.option_values(id);
                let v = match resolve_config_value(&opts, want) {
                    Resolve::Hit(v) => v,
                    _ => continue,
                };
                if let Err(e) = self.push_config_option(id, &v).await {
                    self.emit(EventKind::System {
                        text: format!("{id}={v} non ré-affirmé au resume: {e:#}"),
                    });
                }
                continue;
            }
            let known = self.known_config_values(id);
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

    /// Renomme : meta local posée immédiatement + tentative
    /// session/rename chez l'agent (optionnelle). L'appel ACP est
    /// DIFFÉRÉ quand un tour vole (busy / active_prompts / compact) —
    /// un RPC pendant session/prompt peut avorter le tour en silence
    /// (#183) ; `flush_pending_rename` le pose entre deux tours ou à
    /// la fin du drain. Premier « method not found » → l'agent ne le
    /// supporte pas : plus de retentatives pour la session.
    pub async fn rename(&self, title: &str) {
        self.meta.lock().unwrap().title = title.into();
        self.save_meta();
        if self.turn_in_flight() {
            *self.pending_rename.lock().unwrap() = Some(title.into());
            return;
        }
        self.send_rename(title).await;
    }

    /// Tour ou activité agent en vol : ne pas émettre de RPC ACP
    /// annexe (rename) pendant ce créneau (#183).
    fn turn_in_flight(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
            || self.active_prompts.load(Ordering::SeqCst) > 0
            || self.compacting.lock().unwrap().is_some()
    }

    /// session/rename ACP — best-effort : l'échec est toléré (meta
    /// déjà à jour), un refus de méthode marque l'agent non supporté.
    /// Attente bornée : acp_request n'a pas de timeout — un agent qui
    /// accepte sans jamais répondre wedgerait le drain sur le flush
    /// (#183) ; 10 s puis abandon, l'échec est toléré de toute façon.
    pub(super) async fn send_rename(&self, title: &str) {
        if !self.rename_supported.load(Ordering::SeqCst) {
            return;
        }
        if let Some(sid) = self.meta_snapshot().native_session_id {
            let req = self.acp_request(
                "session/rename",
                json!({"sessionId": sid, "title": title}),
            );
            let err = match tokio::time::timeout(std::time::Duration::from_secs(10), req).await {
                Ok(Ok(_)) => None,
                Ok(Err(e)) => Some(format!("{e:#}").to_lowercase()),
                Err(_) => Some("timeout".into()),
            };
            if let Some(msg) = err {
                // « method not found »/« unknown method » (-32601) ou
                // refus explicite — pas « not found » seul : « Session
                // not found » (binding perdu) est transitoire, la
                // méthode peut remarcher après rebind/réveil.
                if msg.contains("method not found")
                    || msg.contains("unknown method")
                    || msg.contains("not supported")
                {
                    self.rename_supported.store(false, Ordering::SeqCst);
                }
            }
        }
    }

    /// Pose le rename ACP différé (fin de tour / fin de drain) —
    /// seul le dernier titre compte, les renommages en rafale
    /// s'écrasent. Agent mort → le RPC échoue en silence (toléré).
    /// Re-test de la fenêtre : un prompt steer concurrent
    /// (`active_prompts`) ou un compact en vol reste un session/prompt
    /// actif — le pending attend le prochain flush plutôt que
    /// reproduire le stall #183 sur le tour steer/compact.
    pub(super) async fn flush_pending_rename(&self) {
        if self.active_prompts.load(Ordering::SeqCst) > 0
            || self.compacting.lock().unwrap().is_some()
        {
            return;
        }
        let title = self.pending_rename.lock().unwrap().take();
        if let Some(t) = title {
            self.send_rename(&t).await;
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
            // Demande explicite → purge la marque de refus définitif
            // (le replay retentera et le drop réémettra son event).
            self.config_rejected
                .lock()
                .unwrap()
                .remove(config_id);
            let opts = {
                let mut m = self.meta.lock().unwrap();
                m.pending_config.insert(config_id.into(), value.into());
                if let Some(opt) = m.config_options.iter_mut().find(|o| o.id == config_id) {
                    opt.current_value = value.into();
                }
                match config_id {
                    "mode" => m.mode = Some(value.into()),
                    "model" => m.model = Some(value.into()),
                    _ => {
                        // Condition voulue persistante : rejouée à chaque
                        // spawn (reconcile post-handshake), pas seulement
                        // au prochain — sinon perdue au second réveil.
                        m.desired_config.insert(config_id.into(), value.into());
                    }
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
        match self.push_config_option(config_id, value).await {
            Ok(()) => Ok(()),
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

    /// Corps de `set_config_option` une fois la valeur arrêtée : envoie
    /// `session/set_config_option`, puis sync meta + event ConfigOptions
    /// au succès (et lève la retenue d'erreur pour `model`). Aucun event
    /// Error émis — l'appelant choisit sa forme de compte-rendu (Error
    /// détaillé pour l'action utilisateur, warn discret pour la
    /// ré-affirmation de respawn).
    async fn push_config_option(&self, config_id: &str, value: &str) -> Result<()> {
        let sid = self
            .meta_snapshot()
            .native_session_id
            .context("no native session id")?;
        self.acp_request(
            "session/set_config_option",
            json!({"sessionId": sid, "configId": config_id, "value": value}),
        )
        .await?;
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
                // Sticky : rejoué à chaque spawn — un session/load qui
                // restaure la valeur persistée côté agent ne doit pas
                // perdre la condition (thought_level…).
                _ => {
                    m.desired_config.insert(config_id.into(), value.into());
                }
            }
            m.config_options.clone()
        };
        self.save_meta();
        self.emit(EventKind::ConfigOptions { options: opts });
        // Symétrie avec set_model : changer de modèle via
        // config_option est la même réparation — lève la
        // retenue d'erreur et relance la file si un hold tombait.
        if config_id == "model" && self.clear_error() && self.has_queued() {
            self.spawn_drain();
        }
        self.config_rejected.lock().unwrap().remove(config_id);
        Ok(())
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

    /// Ré-applique un instantané de capacités frais (probe
    /// `refresh_agent_caps`) : les LISTES annoncées sont remplacées
    /// (available_modes/models, options de chaque configId) mais la
    /// valeur courante de la session est conservée — `current_value`
    /// et `meta.mode`/`meta.model` sont un état par session que le
    /// spawn jetable ne connaît pas. Push ConfigOptions aux abonnés
    /// quand les options ont bougé. Retour : vrai si la méta a changé.
    pub fn apply_caps(&self, caps: &proto::AgentCaps) -> bool {
        let (changed, opts) = {
            let mut m = self.meta.lock().unwrap();
            let mut changed = false;
            let modes: Vec<String> = caps.modes.iter().map(|o| o.value.clone()).collect();
            if !modes.is_empty() && m.available_modes != modes {
                m.available_modes = modes;
                changed = true;
            }
            let models: Vec<String> = caps.models.iter().map(|o| o.value.clone()).collect();
            if !models.is_empty() && m.available_models != models {
                m.available_models = models;
                changed = true;
            }
            for co in &caps.config_options {
                match m.config_options.iter_mut().find(|o| o.id == co.id) {
                    // Nouvelle annonce pour un configId inconnu : entrée
                    // complète (son current_value est le meilleur indice).
                    None => {
                        m.config_options.push(co.clone());
                        changed = true;
                    }
                    // ConfigId connu : seule la liste des options est
                    // rafraîchie — la valeur courante reste celle que la
                    // session a rapportée/choisie.
                    Some(o) => {
                        if o.options != co.options
                            || o.name != co.name
                            || o.category != co.category
                        {
                            o.name = co.name.clone();
                            o.category = co.category.clone();
                            o.options = co.options.clone();
                            changed = true;
                        }
                    }
                }
            }
            (changed, m.config_options.clone())
        };
        if !changed {
            return false;
        }
        self.save_meta();
        if !opts.is_empty() {
            self.emit(EventKind::ConfigOptions { options: opts });
        }
        true
    }
}

/// Résolution tolérante d'une valeur de config contre la liste annoncée
/// par l'agent (`(value, name affiché)`). Ordre : exact sur `value` →
/// égalité insensible à la casse sur `value`/`name` → préfixe insensible
/// à la casse (Hit si un seul candidat, Ambiguous sinon) → Unknown.
#[derive(Debug, PartialEq)]
pub(super) enum Resolve {
    Hit(String),
    Ambiguous(Vec<String>),
    Unknown,
}

pub(super) fn resolve_config_value(known: &[(String, String)], want: &str) -> Resolve {
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

/// Vrai si le message est une erreur de RÉSOLUTION du garde-fou
/// (valeur absente ou ambiguë dans la liste annoncée) — refus
/// définitif : retenter à l'identique ne changera rien tant que
/// l'agent n'annonce pas d'autres options. Un Invalid params ACP ou
/// une erreur transport n'a pas ces marqueurs → transitoire.
pub(super) fn is_resolution_error(msg: &str) -> bool {
    (msg.contains("inconnu «") || msg.contains("ambigu «"))
        && (msg.contains("— valeurs") || msg.contains("— candidats"))
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
    use super::{is_resolution_error, join_known, resolve_config_value, Resolve};

    fn known(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(v, n)| (v.to_string(), n.to_string()))
            .collect()
    }

    #[test]
    fn resolution_error_is_definitive_only() {
        // Garde-fou de résolution (valeur hors liste / ambiguë) → définitif.
        assert!(is_resolution_error(
            "set_config_option model inconnu « swe-2-flash-max » — valeurs : swe-2-medium"
        ));
        assert!(is_resolution_error(
            "model ambigu « swe » — candidats : swe-2-high, swe-2-low"
        ));
        assert!(is_resolution_error(
            "mode inconnu « yolo » — valeurs : normal, plan"
        ));
        // Transitoire : transport, agent pas prêt, Invalid params ACP.
        assert!(!is_resolution_error("no native session id"));
        assert!(!is_resolution_error(
            "session/set_model: Invalid params: unknown model"
        ));
        assert!(!is_resolution_error("agent exited"));
        // « inconnu » sans la signature complète (ex. option daemon) → pas
        // assimilé : trop fragile pour un drop définitif.
        assert!(!is_resolution_error("option démon inconnue : ac.foo"));
        assert!(!is_resolution_error("valeur inconnue"));
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
