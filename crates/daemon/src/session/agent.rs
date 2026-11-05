//! Cycle de vie du process agent : spawn ACP, handshake, lecture des
//! lignes stdout (events), réponses aux requêtes entrantes, kill/exit.

use super::*;
use anyhow::{bail, Context, Result};
use proto::{EventKind, ToolStatus};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::acp;
use crate::journal;

/// Taille max d'une sortie inline dans un event — au-delà, le contenu
/// part en blob sur disque et l'event ne garde qu'un extrait.
const MAX_INLINE: usize = 32 * 1024;
/// Taille de l'extrait conservé dans l'event quand le contenu part en blob.
const EXCERPT: usize = 4 * 1024;

impl Session {
    // -----------------------------------------------------------------------
    // Spawn / ACP
    // -----------------------------------------------------------------------

    /// Spawn avec retry : un premier essai qui échoue (agent qui meurt
    /// pendant initialize, verrou transitoire…) retente automatiquement.
    /// `skip_replay` : le journal local contient déjà l'historique de cette
    /// session native → les updates rejouées par session/load sont ignorées.
    /// Passer false après un rebind (journal reset) pour les journaliser.
    /// `spawn_cfg` : mode/modèle à ré-affirmer via ACP tant que
    /// `spawning` est encore vrai, donc avant tout drain ou envoi
    /// concurrent (le premier prompt voit la config demandée). Porté à la
    /// création (mode+model demandés, `quiet=false`) et aux respawns qui
    /// veulent forcer une valeur (resume/clear : le modèle, `quiet=true`
    /// — le mode est déjà couvert par l'env et le restage pending_config).
    /// Retour : Ok(true) spawn livré, Ok(false) supplanté en douceur — un
    /// kill/rebind a invalidé la génération après le handshake : la session
    /// appartient à un nouveau cycle (ni config, ni livraison, ni état).
    pub async fn spawn_agent(
        self: &Arc<Self>,
        command: &[String],
        envs: &HashMap<String, String>,
        resume_native_id: Option<String>,
        skip_replay: bool,
        mcp_servers: Value,
        spawn_cfg: Option<SpawnCfg>,
    ) -> Result<bool> {
        // Un seul spawn à la fois : un sleep pendant un réveil en vol
        // libère `waking`, et un nouvel envoi peut alors lancer un
        // second respawn — sans le lock les deux spawn_agent_once
        // s'exécuteraient en parallèle (handshakes ACP écrits dans le
        // stdin l'un de l'autre, stdin_tx/child en last-writer-wins).
        let _guard = self.spawn_lock.lock().await;
        // Le spawn attendu derrière le lock a livré un agent vivant :
        // respawner par-dessus le tuerait pour rien (deuxième réveil,
        // resume pendant un spawn).
        if self.is_alive() {
            // Marquage `spawning` posé en amont (réservation de place) : une
            // session vivante n'est pas en démarrage — ne pas le laisser bloqué.
            self.spawning.store(false, Ordering::SeqCst);
            return Ok(true);
        }
        // Nouveau process = nouveau cycle de livraison : retenue compact
        // et flag busy d'un éventuel tour mort avec l'ancien process.
        // La baseline compact repart à zéro : si le contexte repris est
        // déjà chaud, le premier usage_update live déclenchera (le seuil
        // se mesure au franchissement — un cycle d'agent neuf en refait
        // un, les updates rejoués par le load n'en font jamais).
        self.take_compact_hold();
        self.usage_seen.store(false, Ordering::SeqCst);
        self.busy.store(false, Ordering::SeqCst);
        // Fenêtre "pas encore prêt" : stdin posé tôt mais session/load pas
        // terminé — drain_queue/envois directs doivent rester en file.
        self.spawning.store(true, Ordering::SeqCst);
        let r = self
            .spawn_attempts(command, envs, resume_native_id, skip_replay, mcp_servers)
            .await;
        // Config de création APPLIQUÉE sous le flag spawning : le drain
        // post-spawn (lancé juste après) et les envois extérieurs restent
        // en file tant que spawning est vrai — aucun tour ne peut partir
        // sous le mode par défaut devant la config demandée.
        if matches!(r, Ok(true)) {
            if let Some(cfg) = &spawn_cfg {
                self.apply_spawn_config(
                    cfg.mode.as_deref(),
                    cfg.model.as_deref(),
                    cfg.quiet,
                )
                .await;
            }
        }
        self.spawning.store(false, Ordering::SeqCst);
        // Drain post-spawn ici, flag retombé : lancé depuis
        // spawn_attempts (avant le store) la tâche pouvait casser sur
        // "spawn en vol" et les prompts en file n'étaient jamais livrés
        // (resume explicite, /clear — pas de second déclencheur).
        if matches!(r, Ok(true))
            && self.accepting_prompts()
            && !self.prompt_queue.lock().unwrap().is_empty()
        {
            self.spawn_drain();
        }
        r
    }

    async fn spawn_attempts(
        self: &Arc<Self>,
        command: &[String],
        envs: &HashMap<String, String>,
        resume_native_id: Option<String>,
        skip_replay: bool,
        mcp_servers: Value,
    ) -> Result<bool> {
        let mut last = anyhow::anyhow!("spawn not attempted");
        let sid = self.meta_snapshot().id;
        info!("[{sid}] spawn agent: {}", command.join(" "));
        // Génération attendue par tentative : spawn_agent_once incrémente
        // spawn_gen à son entrée. Un kill (sleep/clear/rebind) entre deux
        // essais la ferait dépasser — retenter produirait un agent sur un
        // contexte déjà supplanté.
        let mut expect = self.spawn_gen.load(Ordering::SeqCst);
        for attempt in 0..2 {
            // Une mise en sommeil pendant le spawn (ou pendant le backoff
            // d'un essai raté) arrête la boucle — pas de respawn : la
            // session appartient désormais à l'état "asleep". Ne vaut que
            // pour les reprises (session/load) : un /clear sur session
            // endormie doit repartir sur un contexte neuf.
            if resume_native_id.is_some() && self.meta_snapshot().state == "asleep" {
                return Err(anyhow::anyhow!("session endormie pendant le spawn"));
            }
            expect += 1;
            if self.spawn_gen.load(Ordering::SeqCst) >= expect {
                return Err(anyhow::anyhow!("spawn supplanté par un kill/rebind"));
            }
            match self
                .spawn_agent_once(
                    command,
                    envs,
                    resume_native_id.clone(),
                    skip_replay,
                    mcp_servers.clone(),
                )
                .await
            {
                Ok(delivered) => return Ok(delivered),
                Err(e) => {
                    warn!("[{sid}] spawn échoué (essai {}): {e:#}", attempt + 1);
                    last = e;
                    if attempt == 0 {
                        // Endormie pendant l'essai : pas de ligne "retry"
                        // trompeuse ni d'attente pour un arrêt volontaire.
                        if self.meta_snapshot().state == "asleep" {
                            return Err(anyhow::anyhow!(
                                "session endormie pendant le spawn"
                            ));
                        }
                        self.emit(EventKind::System {
                            text: format!("agent exited au init — retry ({last:#})"),
                        });
                        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                    }
                }
            }
        }
        error!("[{sid}] spawn abandonné après 2 essais: {last:#}");
        Err(last)
    }

    /// Réarme les refus définitifs (`config_rejected`) dont la valeur
    /// redevient RÉSOLVABLE sur la liste fraîchement annoncée : la
    /// marque tombe et la paire (k, v) — que meta.mode/model ne
    /// conserve plus après resync — est ré-insérée dans
    /// pending_config pour être rejouée (et appliquée si l'agent la
    /// confirme). Unknown/Ambiguous/liste vide = toujours non
    /// résolvable → la marque reste (sinon un passthrough sans garde
    /// re-bouclerait le warn).
    /// Appelée après chaque écriture fraîche de config_options /
    /// available_models / available_modes.
    fn revive_rejected_config(&self) {
        // Deux phases : évaluer les valeurs HORS du lock config_rejected
        // (known_config_values prend meta — le reconcile tient meta
        // puis config_rejected, ordre inverse = deadlock).
        let snapshot: Vec<(String, String)> = {
            let rej = self.config_rejected.lock().unwrap();
            rej.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        };
        if snapshot.is_empty() {
            return;
        }
        let mut rearm = Vec::new();
        let keep: std::collections::HashMap<String, String> = snapshot
            .into_iter()
            .filter(|(k, v)| {
                let known = self.known_config_values(k);
                if known.is_empty() {
                    return true; // rien à confirmer → marque conservée
                }
                match config::resolve_config_value(&known, v) {
                    config::Resolve::Hit(_) => {
                        rearm.push((k.clone(), v.clone()));
                        false
                    }
                    _ => true,
                }
            })
            .collect();
        if !rearm.is_empty() {
            {
                let mut m = self.meta.lock().unwrap();
                for (k, v) in &rearm {
                    m.pending_config.insert(k.clone(), v.clone());
                }
            }
            self.save_meta();
        }
        self.config_rejected
            .lock()
            .unwrap()
            // retain sur (k,v) exact : une marque posée entre le snapshot
            // et ici (autre cycle) n'est pas effacée à tort.
            .retain(|k, v| keep.get(k) == Some(v));
    }

    /// Vrai si `(config_id, valeur)` a déjà été soldé comme refus
    /// définitif — le reconcile saute alors le re-pending (le replay
    /// la re-échouerait à chaque spawn) et garde la valeur rapportée
    /// par l'agent dans meta.
    fn config_rejected_has(&self, k: &str, v: &str) -> bool {
        self.config_rejected
            .lock()
            .unwrap()
            .get(k)
            .map(String::as_str)
            == Some(v)
    }

    /// Retour : Ok(true) agent prêt (session idle posée), Ok(false) le
    /// handshake a réussi mais un kill/rebind a supplanté la génération
    /// entre-temps — le nouveau cycle possède la session (rien à poser).
    async fn spawn_agent_once(
        self: &Arc<Self>,
        command: &[String],
        envs: &HashMap<String, String>,
        resume_native_id: Option<String>,
        skip_replay: bool,
        mcp_servers: Value,
    ) -> Result<bool> {
        // Nouvelle génération : tout reader d'un process précédent devient
        // périmé — son on_agent_exit ne touchera plus l'état partagé.
        let gen = self.spawn_gen.fetch_add(1, Ordering::SeqCst) + 1;
        // Baseline des annonces config_options : une liste jugée
        // « fraîche » au replay pending_config doit avoir été écrite
        // APRÈS ce point (réponse handshake ou event de ce spawn).
        let cfg_gen0 = self.config_gen.load(Ordering::SeqCst);
        // Un load tué en vol laisserait le flag vrai → les updates de la
        // session suivante seraient ignorées comme du replay.
        self.loading_replay.store(false, Ordering::SeqCst);
        let meta = self.meta_snapshot();
        let (cmd, args) = command.split_first().context("commande agent vide")?;
        let mut child = Command::new(cmd)
            .args(args)
            .current_dir(&meta.cwd)
            .envs(envs)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawn {cmd}"))?;

        let stdin = child.stdin.take().context("stdin")?;
        let stdout = child.stdout.take().context("stdout")?;
        let stderr = child.stderr.take().context("stderr")?;

        // Writer task
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        {
            let mut stdin = stdin;
            tokio::spawn(async move {
                while let Some(line) = rx.recv().await {
                    if stdin.write_all(line.as_bytes()).await.is_err()
                        || stdin.write_all(b"\n").await.is_err()
                    {
                        break;
                    }
                    let _ = stdin.flush().await;
                }
            });
        }
        *self.stdin_tx.lock().unwrap() = Some(tx);

        // Stderr -> journal debug
        {
            let sid = meta.id.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    info!("[{sid} stderr] {l}");
                }
            });
        }

        // Reader task : dispatch responses / notifications / requests
        {
            let session = Arc::clone(self);
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => session.handle_agent_line(&line, gen).await,
                        _ => break,
                    }
                }
                session.on_agent_exit(gen).await;
            });
        }

        *self.child.lock().unwrap() = Some(child);
        // Un kill a pu atterrir pendant Command::spawn (sleep, clear,
        // rebind) — avant que le child soit posé, il n'y avait rien à
        // prendre : le process serait resté vivant sur un contexte
        // supplanté. Kill + bail ; la boucle ne retentera pas (check de
        // génération en tête).
        if self.spawn_gen.load(Ordering::SeqCst) != gen {
            self.kill_for_respawn().await;
            bail!("spawn supplanté (kill pendant le spawn)");
        }

        // ACP handshake : initialize puis session/new|load. Un échec ne
        // doit pas laisser le process vivant — sinon is_alive() ment
        // (agent initialisé mais sans session liée : les envois
        // répondent « Session not found » et resume_session répond
        // « already » sans jamais réparer) → kill silencieux, l'état
        // final est décidé par l'appelant du spawn.
        let hs = self
            .acp_handshake(&meta, resume_native_id.clone(), skip_replay, mcp_servers)
            .await;
        let (resp, supports_load, fell_back) = match hs {
            Ok(v) => v,
            Err(e) => {
                self.kill_failed_spawn(gen).await;
                return Err(e);
            }
        };

        if let Some(sid) = resp.get("sessionId").and_then(Value::as_str) {
            let mut m = self.meta.lock().unwrap();
            m.native_session_id = Some(sid.to_string());
        }
        // Modes disponibles éventuels — annonce fraîche de ce spawn,
        // comptée pour la fraîcheur du drop définitif pending_config.
        if let Some(modes) = resp.get("modes") {
            let mut m = self.meta.lock().unwrap();
            if let Some(cur) = modes.get("currentModeId").and_then(Value::as_str) {
                m.mode = Some(cur.to_string());
            }
            if let Some(av) = modes.get("availableModes").and_then(Value::as_array) {
                m.available_modes = av
                    .iter()
                    .filter_map(|x| x.get("id").and_then(Value::as_str).map(String::from))
                    .collect();
            }
            self.config_gen.fetch_add(1, Ordering::SeqCst);
        }
        // Modèles disponibles éventuels
        if let Some(models) = resp.get("models") {
            let mut m = self.meta.lock().unwrap();
            if let Some(cur) = models.get("currentModelId").and_then(Value::as_str) {
                m.model = Some(cur.to_string());
            }
            if let Some(av) = models.get("availableModels").and_then(Value::as_array) {
                m.available_models = av
                    .iter()
                    .filter_map(|x| {
                        x.get("modelId")
                            .or_else(|| x.get("id"))
                            .and_then(Value::as_str)
                            .map(String::from)
                    })
                    .collect();
            }
            self.config_gen.fetch_add(1, Ordering::SeqCst);
        }
        // Options de config directement dans la réponse session/new|load
        // (Devin les envoie normalement via config_option_update, mais après
        // un session/load la réponse peut les porter elle-même).
        if let Some(opts) = resp.get("configOptions").and_then(Value::as_array) {
            let parsed: Vec<proto::ConfigOption> = opts
                .iter()
                .filter_map(|o| serde_json::from_value::<proto::ConfigOption>(o.clone()).ok())
                .collect();
            if !parsed.is_empty() {
                let mut m = self.meta.lock().unwrap();
                for o in &parsed {
                    match o.id.as_str() {
                        "mode" => m.mode = Some(o.current_value.clone()),
                        "model" => m.model = Some(o.current_value.clone()),
                        _ => {}
                    }
                }
                m.config_options = parsed;
                // Liste fraîchement annoncée dans CETTE réponse — borne
                // la fraîcheur pour le drop définitif du replay
                // pending_config (uniquement si la liste a été
                // réellement réécrite : une annonce vide ne doit pas
                // valider la liste persistée).
                self.config_gen.fetch_add(1, Ordering::SeqCst);
            }
        }
        // Une annonce fraîche peut rendre résoluble une valeur soldée
        // comme refus définitif → la marque tombe, la clé repassera
        // par pending_config au reconcile ci-dessous.
        self.revive_rejected_config();
        self.save_meta();
        // Capacités du profil : chaque handshake enrichit le cache démon
        // (agent_caps.json — selects du dialogue nouvelle session,
        // refresh_agent_caps). No-op si l'agent n'a rien annoncé.
        self.caps
            .harvest(&meta.agent, &crate::caps::caps_from_new_resp(&resp));
        // Un kill (sleep/clear/respawn concurrent) a invalidé la génération
        // pendant le handshake : le process vient d'être tué — ni line de
        // spawn, ni replay de config, ni "idle" (ils pollueraient le cycle
        // qui a pris le relais ; la config reste en pending_config pour le
        // prochain spawn).
        if self.spawn_gen.load(Ordering::SeqCst) != gen {
            return Ok(false);
        }
        self.emit(EventKind::System {
            text: match (resume_native_id.is_some(), fell_back) {
                (_, true) => "agent respawned (session native perdue — contexte neuf)".into(),
                (true, false) => {
                    format!("agent respawned (session/load={}, resumed)", supports_load)
                }
                (false, _) => "agent started".into(),
            },
        });
        // Config demandée pendant que l'agent était mort : rejouer maintenant
        // (modèle/mode choisis hors-ligne appliqués au resume).
        // Réconciliation post-handshake : la valeur DEMANDÉE (snapshot
        // pré-spawn) prime sur ce que l'agent rapporte. Un session/load
        // peut restaurer le mode/modèle persisté côté agent en ignorant
        // l'env de spawn (DEVIN_PERMISSION_MODE…) — un agent endormi en
        // bypass repartait alors en mode par défaut, et le mode rapporté
        // écrasait meta.mode (spawn_args/env faux pour tous les réveils
        // suivants). Un mode ACP-only omis de l'env par env_mode n'a
        // jamais été posé non plus. Tout mismatch est re-poussé via
        // pending_config (set_config_option / set_mode au drain) et
        // meta.mode/model restaurés sur la valeur demandée ; le skip
        // évite l'appel redondant quand session/load l'avait restauré.
        {
            let mut m = self.meta.lock().unwrap();
            if let Some(want) = meta.mode.clone() {
                // "" n'est pas un mode poussable (apply_spawn_config le
                // filtre déjà) — sans garde, il bouclerait un set_mode
                // en échec à chaque spawn. Une valeur soldée comme refus
                // définitif n'est ni re-pendue ni restaurée dans meta :
                // le handshake a déjà posé la valeur rapportée par
                // l'agent (currentModeId), qui fait foi.
                if !want.is_empty()
                    && m.mode.as_deref() != Some(want.as_str())
                    && !self.config_rejected_has("mode", &want)
                {
                    m.pending_config.insert("mode".into(), want.clone());
                    m.mode = Some(want);
                }
            }
            if let Some(want) = meta.model.clone() {
                // model_source="catalogue" : la valeur est posée par le
                // side-channel (model_override_cmd), l'agent la reporte
                // clampée — la re-pousser par pending_config/set_config_
                // option serait un refus ACP garanti en boucle. On garde
                // meta.model tel quel (c'est la valeur demandée).
                if !want.is_empty()
                    && m.model_source.as_deref() != Some("catalogue")
                    && m.model.as_deref() != Some(want.as_str())
                    && !self.config_rejected_has("model", &want)
                {
                    m.pending_config.insert("model".into(), want.clone());
                    m.model = Some(want);
                }
            }
            // Conditions de session posées en live (thought_level…) :
            // ni l'env ni le handshake ne les portent — re-poussées à
            // chaque spawn tant que la valeur rapportée diffère de la
            // voulue (desired_config, sticky par nature).
            let desired: Vec<(String, String)> = m
                .desired_config
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            for (k, v) in desired {
                let reported = m
                    .config_options
                    .iter()
                    .find(|o| o.id == k)
                    .map(|o| o.current_value.as_str());
                if reported != Some(v.as_str()) && !self.config_rejected_has(&k, &v) {
                    // Une valeur déjà soldée comme refus définitif n'est
                    // pas re-pendée : le replay la re-échouerait à chaque
                    // spawn (boucle de warn). revive_rejected_config la
                    // ré-arme si une annonce fraîche la rend résolvable.
                    m.pending_config.insert(k, v);
                }
            }
        }
        self.save_meta();
        let pending: Vec<(String, String)> =
            { self.meta.lock().unwrap().pending_config.drain().collect() };
        if !pending.is_empty() {
            self.save_meta(); // persiste la map vidée même si les appels échouent
        }
        for (k, v) in pending {
            let r = match k.as_str() {
                // model/mode : d'abord la config_option annoncée, puis le
                // setter dédié en repli. Le repli n'a pas toujours de
                // garde-fou (set_mode envoie la valeur brute) — un refus
                // de résolution au 1er essai sur liste fraîche reste
                // définitif même si le repli échoue pour une autre raison.
                id @ ("model" | "mode") => {
                    match self.set_config_option(id, &v).await {
                        Ok(()) => Ok(()),
                        Err(e1) => {
                            let e1_res =
                                config::is_resolution_error(&format!("{e1:#}"));
                            let fb = if id == "model" {
                                self.set_model(&v).await.map(|_| ())
                            } else {
                                self.set_mode(&v).await
                            };
                            match fb {
                                Ok(()) => Ok(()),
                                Err(e2) => {
                                    let fresh =
                                        self.config_gen.load(Ordering::SeqCst) != cfg_gen0;
                                    if fresh && e1_res {
                                        Err(e1)
                                    } else {
                                        Err(e2)
                                    }
                                }
                            }
                        }
                    }
                }
                _ => self.set_config_option(&k, &v).await,
            };
            if let Err(e) = r {
                // Deux classes d'échec : un refus DÉFINITIF (la valeur
                // est absente de la liste que l'agent vient d'annoncer
                // pendant ce spawn — erreur de résolution du garde-fou,
                // pas un Invalid params ACP) est soldé : re-pendre
                // bouclerait le warn à chaque spawn à vie. Le transitoire
                // (agent pas prêt, transport, rate-limit, liste périmée
                // non rafraîchie ce spawn) garde le re-pending.
                let fresh = self.config_gen.load(Ordering::SeqCst) != cfg_gen0;
                let definitive =
                    fresh && config::is_resolution_error(&format!("{e:#}"));
                if definitive {
                    // meta.mode/model reflètent la valeur DEMANDÉE (le
                    // reconcile la restaure) — resynchroniser sur le
                    // current_value annoncé pour que la session soit
                    // affichée/relancée sur ce qui tourne vraiment.
                    {
                        let mut m = self.meta.lock().unwrap();
                        if k == "model" || k == "mode" {
                            let cur = m
                                .config_options
                                .iter()
                                .find(|o| o.id == k || o.category == k)
                                .map(|o| o.current_value.clone());
                            if let Some(v) = cur {
                                if k == "model" {
                                    m.model = Some(v);
                                } else {
                                    m.mode = Some(v);
                                }
                            }
                        }
                    }
                    self.save_meta();
                    let mut rej = self.config_rejected.lock().unwrap();
                    if rej.get(&k).map(String::as_str) != Some(v.as_str()) {
                        rej.insert(k.clone(), v.clone());
                        drop(rej);
                        self.emit(EventKind::System {
                            text: format!(
                                "config {k}={v} abandonnée — refus définitif de l'agent \
                                 (valeur hors liste annoncée) : {e:#}"
                            ),
                        });
                    }
                } else {
                    self.emit(EventKind::Error {
                        message: format!("config différée {k}={v}: {e:#}"),
                    });
                    // Re-mise en pending : un échec transitoire ne doit pas
                    // perdre la condition voulue — retentée au prochain spawn.
                    self.meta.lock().unwrap().pending_config.insert(k, v);
                }
            } else {
                self.config_rejected.lock().unwrap().remove(&k);
            }
        }
        if !self.meta.lock().unwrap().pending_config.is_empty() {
            self.save_meta();
        }
        // Reset de cycle : un spawn réussi solde la trace d'erreur
        // d'un éventuel cycle précédent (respawn, /clear, réveil) et
        // lève la retenue — le drain post-spawn livrera la file.
        self.take_error_hold();
        self.clear_last_error();
        self.set_state("idle");
        Ok(true)
    }

    /// Handshake ACP d'un process frais : `initialize`, puis
    /// `session/load` quand une session native est à reprendre et que
    /// l'agent l'annonce, sinon `session/new`.
    /// Un session/load dont la session n'est plus résolvable côté agent
    /// (store vidé par un restart, id jamais persisté, erreur interne…)
    /// dégrade en `session/new` sur le MÊME process : la session démon
    /// garde son journal, l'agent repart sur un contexte neuf — la ligne
    /// système émise ici le rend visible au lieu d'une erreur fatale qui
    /// laissait la session inutilisable à vie. Seules les erreurs qui
    /// signalent une session encore VERROUILLÉE ailleurs (« already
    /// open », TUI, autre démon) restent fatales : un repli silencieux
    /// masquerait un vrai conflit — et la boucle de retry du fork
    /// compte dessus.
    /// Retour : (réponse session/new|load, supports_load, fell_back).
    async fn acp_handshake(
        &self,
        meta: &SessionMeta,
        resume_native_id: Option<String>,
        skip_replay: bool,
        mcp_servers: Value,
    ) -> Result<(Value, bool, bool)> {
        let init = self
            .acp_request("initialize", acp::initialize_params())
            .await
            .context("initialize")?;
        let agent_caps = init.get("agentCapabilities").cloned().unwrap_or(json!({}));
        let supports_load = agent_caps
            .get("loadSession")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        if let (Some(native), true) = (resume_native_id, supports_load) {
            self.loading_replay.store(skip_replay, Ordering::SeqCst);
            let r = self
                .acp_request(
                    "session/load",
                    acp::session_load_params(&native, &meta.cwd, mcp_servers.clone()),
                )
                .await;
            self.loading_replay.store(false, Ordering::SeqCst);
            match r {
                Ok(v) => Ok((v, supports_load, false)),
                Err(e) => {
                    if load_err_keeps_native(&format!("{e:#}")) {
                        return Err(e).context("session/load");
                    }
                    warn!(
                        "[{}] session/load impossible ({e:#}) — repli session/new",
                        meta.id
                    );
                    self.emit(EventKind::System {
                        text: format!(
                            "session native perdue ({e}) — agent relancé sur un contexte neuf"
                        ),
                    });
                    let v = self
                        .acp_request(
                            "session/new",
                            acp::session_new_params(&meta.cwd, mcp_servers),
                        )
                        .await
                        .context("session/new (repli après session/load)")?;
                    Ok((v, supports_load, true))
                }
            }
        } else {
            let v = self
                .acp_request(
                    "session/new",
                    acp::session_new_params(&meta.cwd, mcp_servers),
                )
                .await
                .context("session/new")?;
            Ok((v, supports_load, false))
        }
    }

    /// Tue le process laissé par un handshake raté (initialize ou
    /// session/new|load en échec). Sans lui, `is_alive()` resterait vrai
    /// sur un agent sans session liée : les envois tomberaient sur
    /// « Session not found » en boucle et `resume_session` répondrait
    /// « already » — la session serait bloquée à vie. L'exit du reader
    /// est marqué par génération (`suppress_exit_gen`) : ménage fait,
    /// mais ni « agent process exited » ni dead — l'état final est posé
    /// par l'appelant du spawn (mark_dead, retour à asleep,
    /// interrupted…). Pas de bump de génération : ce kill n'est pas un
    /// supplant, les tentatives de spawn_attempts doivent continuer.
    /// Garde de génération : un kill_for_respawn externe a pu supplanter
    /// pendant le handshake — stdin_tx/acp_pending/child appartiennent
    /// alors au cycle nouveau (notre vieux process a déjà été pris) ;
    /// y toucher casserait le spawn frais en vol.
    async fn kill_failed_spawn(&self, gen: u64) {
        if self.spawn_gen.load(Ordering::SeqCst) != gen {
            return;
        }
        self.suppress_exit_gen.store(gen, Ordering::SeqCst);
        *self.stdin_tx.lock().unwrap() = None;
        {
            let mut p = self.acp_pending.lock().unwrap();
            for (_, tx) in p.drain() {
                let _ = tx.send(Err("spawn failed".into()));
            }
        }
        self.kill().await;
    }

    /// Tue le process dont la session native a disparu côté agent
    /// (session/prompt « Session not found » en pleine vie — pas un
    /// handshake raté). L'exit normal du reader s'applique : dead +
    /// « agent process exited » — honnête, le process ne peut
    /// effectivement plus rien pour cette session, et le chemin de
    /// recours (resume → session/load → repli session/new) redevient
    /// praticable.
    /// Garde de génération : la réponse classée vient du process de la
    /// génération `gen` — un supplant entre-temps (kill_for_respawn,
    /// rebind) a déjà pris le vieux child ; stdin_tx/child visibles
    /// appartiendraient au process neuf en plein handshake → ne pas y
    /// toucher.
    pub(super) async fn drop_dead_binding(&self, gen: u64) {
        if self.spawn_gen.load(Ordering::SeqCst) != gen {
            return;
        }
        *self.stdin_tx.lock().unwrap() = None;
        let c = { self.child.lock().unwrap().take() };
        if let Some(mut c) = c {
            let _ = c.kill().await;
        }
    }

    async fn on_agent_exit(&self, gen: u64) {
        // Un spawn plus récent a pris le relais (resume/rebind/clear) :
        // la mort du vieux process est attendue — ne pas marquer dead ni
        // lâcher le child (il appartient au process de la gen courante).
        if self.spawn_gen.load(Ordering::SeqCst) != gen {
            return;
        }
        *self.stdin_tx.lock().unwrap() = None;
        self.child.lock().unwrap().take();
        self.flush_text();
        // Rejeter tout ce qui attend une réponse de l'agent
        let mut p = self.acp_pending.lock().unwrap();
        for (_, tx) in p.drain() {
            let _ = tx.send(Err("agent exited".into()));
        }
        // Tué par kill_failed_spawn : le ménage ci-dessus suffit — la
        // ligne « agent process exited » et l'état dead appartiennent à
        // une mort subite, pas à un handshake raté (l'appelant du spawn
        // pose l'état final). CAS : un marqueur posé pour une AUTRE
        // génération n'éteint pas cet exit.
        if self
            .suppress_exit_gen
            .compare_exchange(gen, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            info!(
                "[{}] process de handshake raté tué (gen {gen})",
                self.meta_snapshot().id
            );
            return;
        }
        // Abort du timer de retry d'un éventuel hold : une session
        // morte ne doit pas se relancer toute seule.
        self.take_error_hold();
        self.set_state("dead");
        info!("[{}] agent process exited", self.meta_snapshot().id);
        self.emit(EventKind::System {
            text: "agent process exited".into(),
        });
    }

    pub(super) fn write_line(&self, v: &Value) -> Result<()> {
        let guard = self.stdin_tx.lock().unwrap();
        match guard.as_ref() {
            Some(tx) => {
                tx.send(serde_json::to_string(v)?)?;
                Ok(())
            }
            None => bail!("agent not running"),
        }
    }

    /// Envoie une requête JSON-RPC à l'agent et attend la réponse.
    pub async fn acp_request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.acp_next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.acp_pending.lock().unwrap().insert(id, tx);
        let msg = acp::request(id, method, params);
        if let Err(e) = self.write_line(&msg) {
            self.acp_pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        match rx.await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => bail!("acp {method}: {e}"),
            Err(_) => bail!("acp {method}: channel closed"),
        }
    }

    // -----------------------------------------------------------------------
    // Dispatch des lignes sortantes de l'agent
    // -----------------------------------------------------------------------

    async fn handle_agent_line(self: &Arc<Self>, line: &str, gen: u64) {
        // Ligne d'un process remplacé (clear/rebind) : ses updates et
        // requêtes n'ont plus de sens — et ses réponses n'ont plus
        // d'attendant (acp_pending soldé au kill).
        if self.spawn_gen.load(Ordering::SeqCst) != gen {
            return;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return,
        };
        // Réponse à une de nos requêtes ?
        if v.get("method").is_none() && v.get("id").is_some() {
            let id = v["id"].as_u64().unwrap_or(0);
            if let Some(tx) = self.acp_pending.lock().unwrap().remove(&id) {
                let r = if let Some(err) = v.get("error") {
                    Err(err.get("message").and_then(Value::as_str).unwrap_or("error").to_string())
                } else {
                    Ok(v.get("result").cloned().unwrap_or(Value::Null))
                };
                let _ = tx.send(r);
            }
            return;
        }
        let method = v.get("method").and_then(Value::as_str).unwrap_or("");
        let has_id = v.get("id").is_some();
        if !has_id {
            // Notification agent -> client
            if method == "session/update" {
                self.handle_session_update(&v["params"]).await;
            }
            return;
        }
        // Requête agent -> client : permission, elicitation, fs, terminal…
        let acp_id = v["id"].clone();
        let params = v.get("params").cloned().unwrap_or(json!({}));
        match method {
            "session/request_permission" => {
                let req = acp::parse_permission(&params);
                let request_id = proto::new_id("pr");
                self.pending_agent
                    .lock()
                    .unwrap()
                    .insert(request_id.clone(), PendingAgent::Permission { acp_id });
                self.set_state("waiting_input");
                self.emit(EventKind::PermissionRequest {
                    request_id,
                    title: req.title,
                    kind: req.kind,
                    call_id: req.call_id,
                    detail: req.detail,
                    options: req.options,
                });
            }
            m if m.contains("elicit") => {
                // elicitation/create : {message, requestedSchema}
                let request_id = proto::new_id("qr");
                self.pending_agent
                    .lock()
                    .unwrap()
                    .insert(request_id.clone(), PendingAgent::Question { acp_id });
                self.set_state("waiting_input");
                self.emit(EventKind::QuestionRequest {
                    request_id,
                    message: params
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    schema: params.get("requestedSchema").cloned(),
                });
            }
            _ => {
                // fs/*, terminal/* non supportés : refus propre
                let resp = acp::error_response(&acp_id, -32601, "method not supported by host");
                let _ = self.write_line(&resp);
            }
        }
    }

    pub(super) async fn handle_session_update(&self, params: &Value) {
        let update = match params.get("update") {
            Some(u) => u,
            None => return,
        };
        match acp::map_update(update) {
            acp::Mapped::Chunk(kind, text) => {
                if !self.loading_replay.load(Ordering::SeqCst) {
                    self.push_chunk(kind, &text);
                }
            }
            acp::Mapped::Event(kind) => {
                // Pendant le replay de session/load : les events sont
                // traités pour leurs effets de bord méta (mode, config,
                // contexte) mais PAS re-journalisés — le journal local
                // contient déjà l'historique (sinon chaque resume
                // empilait commands_available/config_options en double,
                // et un `plan` rejoué dupliquerait un message entier).
                let replaying = self.loading_replay.load(Ordering::SeqCst);
                let mut compact_done = false;
                let mut auto_compact: Option<(u64, u64)> = None;
                match &kind {
                    EventKind::ModeChanged { mode } => {
                        let mut m = self.meta.lock().unwrap();
                        if m.mode.as_deref() == Some(mode.as_str()) {
                            return; // déduplique les mode_changed à l'identique
                        }
                        m.mode = Some(mode.clone());
                        drop(m);
                        self.save_meta();
                    }
                    EventKind::CommandsAvailable { commands } => {
                        {
                            self.meta.lock().unwrap().available_commands = commands.clone();
                        }
                        self.save_meta();
                    }
                    EventKind::ConfigOptions { options } => {
                        let agent = {
                            let mut m = self.meta.lock().unwrap();
                            m.config_options = options.clone();
                            // sync mode/model courants pour compat UI
                            for o in options {
                                match o.id.as_str() {
                                    "mode" => m.mode = Some(o.current_value.clone()),
                                    "model" => m.model = Some(o.current_value.clone()),
                                    _ => {}
                                }
                            }
                            m.agent.clone()
                        };
                        self.config_gen.fetch_add(1, Ordering::SeqCst);
                        self.revive_rejected_config();
                        self.save_meta();
                        // Le cache profil suit le flux live — addition,
                        // pas remplacement (le probe reste la voie refresh).
                        self.caps.update_options(&agent, options);
                    }
                    EventKind::UsageUpdate { used, size, .. } => {
                        let mut m = self.meta.lock().unwrap();
                        if m.context_used == Some(*used) && m.context_size == Some(*size) {
                            return; // déduplique (replay, heartbeat à l'identique)
                        }
                        let (prev_used, prev_size) = (m.context_used, m.context_size);
                        m.context_used = Some(*used);
                        m.context_size = Some(*size);
                        drop(m);
                        self.save_meta();
                        // Fin de compact : l'occupation contexte est
                        // passée sous la valeur relevée au lancement.
                        if self
                            .compacting
                            .lock()
                            .unwrap()
                            .as_ref()
                            .and_then(|h| h.used_at_start)
                            .is_some_and(|u0| *used < u0)
                        {
                            compact_done = true;
                        }
                        // Compact auto au seuil : déclenché au
                        // FRANCHISSEMENT (prev sous seuil → now au-dessus)
                        // — un usage qui reste haut après un compact
                        // inefficace ne reboucle pas, un re-dépassement
                        // ultérieur (croissance) redéclenche à juste titre.
                        // Pas de baseline live encore (spawn récent) : le
                        // premier update compte comme franchissement s'il
                        // est déjà au-dessus — cas de la session reprise
                        // « chaude » (le replay ne nourrit pas `was`).
                        if !replaying {
                            let pol = self.compact_policy();
                            let over = pol.exceeds(*used, *size);
                            let seen = self.usage_seen.swap(true, Ordering::SeqCst);
                            let was = seen
                                && prev_used.is_some_and(|u| {
                                    pol.exceeds(u, prev_size.unwrap_or(*size))
                                });
                            if over && !was {
                                auto_compact = Some((*used, *size));
                            }
                        }
                    }
                    _ => {}
                }
                if !replaying {
                    self.emit(kind);
                }
                if compact_done {
                    self.end_compact("contexte réduit", false);
                }
                if let Some((u, s)) = auto_compact {
                    self.request_auto_compact(u, s);
                }
            }
            acp::Mapped::ToolUpdate {
                call_id,
                status,
                text,
                diff,
                title,
                kind,
                locations,
                input,
            } => {
                if !self.loading_replay.load(Ordering::SeqCst) {
                    self.flush_text();
                    self.apply_tool_update(
                        call_id, status, text, diff, title, kind, locations, input,
                    );
                }
            }
            acp::Mapped::Ignore => {}
        }
    }

    /// Journalise un tool_call / tool_call_update, avec externalisation blob.
    fn apply_tool_update(
        &self,
        call_id: String,
        status: ToolStatus,
        text: String,
        diff: Option<String>,
        title: Option<String>,
        kind: Option<String>,
        locations: Option<Vec<String>>,
        input: Option<String>,
    ) {
        let is_new = {
            let mut tc = self.tool_calls.lock().unwrap();
            let known = tc.contains_key(&call_id);
            if let (Some(t), Some(k)) = (title.clone(), kind.clone()) {
                tc.insert(call_id.clone(), (t, k));
            }
            !known
        };

        // Externalise le texte volumineux en blob
        let (blob, excerpt, size) = if text.len() > MAX_INLINE {
            let name = format!("{}.out", call_id);
            match journal::write_blob(&self.dir, &name, text.as_bytes()) {
                Ok(rel) => (
                    Some(rel),
                    Some(text.chars().take(EXCERPT).collect()),
                    Some(text.len() as u64),
                ),
                Err(_) => (None, Some(text.chars().take(EXCERPT).collect()), Some(text.len() as u64)),
            }
        } else {
            (None, if text.is_empty() { None } else { Some(text) }, None)
        };

        if is_new {
            let (t, k) = self
                .tool_calls
                .lock()
                .unwrap()
                .get(&call_id)
                .cloned()
                .unwrap_or_else(|| (title.unwrap_or_else(|| "tool call".into()), kind.unwrap_or_else(|| "other".into())));
            self.emit(EventKind::ToolCall {
                call_id,
                title: t,
                kind: k,
                status,
                blob,
                excerpt,
                size,
                diff,
                locations,
                input,
            });
        } else {
            self.emit(EventKind::ToolCallUpdate {
                call_id,
                status,
                excerpt,
                blob,
                size,
                diff,
            });
        }
    }

    pub async fn kill(&self) {
        let c = { self.child.lock().unwrap().take() };
        if let Some(mut c) = c {
            let _ = c.kill().await;
        }
    }

    /// Tue l'agent en vue d'un respawn immédiat (clear) : la génération
    /// est invalidée AVANT le kill → le on_agent_exit du vieux reader se
    /// tait (ni "agent process exited" ni dead dans le nouveau cycle).
    /// Les requêtes ACP en vol sont soldées en erreur, sinon leurs
    /// appelants resteraient suspendus indéfiniment.
    pub async fn kill_for_respawn(&self) {
        debug!("[{}] kill pour respawn", self.meta_snapshot().id);
        // supplant_mark = génération invalidée : un spawn concurrent la
        // compare à sa génération de départ — > départ = kill/rebind
        // intervenu PENDANT son spawn (signal typé, indépendant des
        // libellés d'erreur remontés par les pending soldés).
        let g = self.spawn_gen.fetch_add(1, Ordering::SeqCst) + 1;
        self.supplant_mark.store(g, Ordering::SeqCst);
        // Abort du timer de retry erreur : le nouveau cycle décidera —
        // laisser le timer survivre relancerait un drain sur l'agent
        // frais sans rapport avec l'échec soldé.
        self.take_error_hold();
        *self.stdin_tx.lock().unwrap() = None;
        {
            let mut p = self.acp_pending.lock().unwrap();
            for (_, tx) in p.drain() {
                let _ = tx.send(Err("agent replaced".into()));
            }
        }
        self.kill().await;
    }

    /// Marque la session morte — chemin d'erreur d'un respawn post-clear
    /// (set_state est privé, le handler serveur en a besoin ici).
    pub fn mark_dead(&self) {
        // Abort du timer de retry : une session morte ne doit pas se
        // relancer toute seule — le hold tombe avec l'agent.
        self.take_error_hold();
        self.set_state("dead");
    }

    /// Signale un spawn imminent sans attendre spawn_agent : la création
    /// async répond dès l'insert de la session et lance le spawn en tâche
    /// de fond — sans ce flag posé ici, un envoi tombé dans la fenêtre
    /// entre la réponse et le démarrage effectif bailerait "agent not
    /// running" au lieu de s'enfiler. spawn_agent reprend le flag
    /// (store true→false autour de ses tentatives réelles).
    pub fn mark_spawning(&self) {
        self.spawning.store(true, Ordering::SeqCst);
    }

    /// Remet la session en sommeil — retour d'état quand un réveil
    /// (respawn) a échoué : le prochain envoi retentera.
    pub fn mark_asleep(&self) {
        self.set_state("asleep");
    }

    /// Mise en sommeil : état persisté "asleep" posé AVANT le kill —
    /// accepting_prompts() rend faux dès ici, donc un envoi tombé dans
    /// la fenêtre s'enfile pour le réveil au lieu d'être livré à un
    /// agent en train de mourir ou de bailer "agent not running". Kill
    /// silencieux (génération invalidée — ni "agent exited" ni dead) ;
    /// la session native, le journal et la file de prompts sont
    /// conservés — le prochain envoi (user ou peer) la réveillera via
    /// session/load. La retenue compact est levée : sinon son timer
    /// relancerait un drain sur un agent mort ("prompt failed" en
    /// rafale). Un réveil en vol s'arrête au check "asleep" de la
    /// boucle de spawn — `waking` n'est PAS relâché ici : le flag
    /// appartient à la tâche de réveil, qui le libère via wake_done
    /// (sinon un nouvel envoi ré-acquerrait le flag pendant que
    /// l'ancienne tâche le libère aussi — double relâchement croisé).
    pub async fn sleep(&self) {
        self.set_state("asleep");
        self.take_compact_hold();
        self.kill_for_respawn().await;
        self.flush_text();
        self.emit(EventKind::System {
            text: "session endormie — l'agent sera relancé au prochain message".into(),
        });
    }

    /// Sort de l'état "asleep" avant un spawn légitime (réveil auto ou
    /// resume explicite) — sans ça le garde "asleep" de spawn_agent
    /// arrêterait le réveil. Retourne vrai si la session était endormie
    /// (à restaurer via set_state("asleep") si le spawn échoue).
    pub fn leave_asleep(&self) -> bool {
        if self.meta_snapshot().state == "asleep" {
            self.set_state("starting");
            return true;
        }
        false
    }

    pub fn is_alive(&self) -> bool {
        self.stdin_tx.lock().unwrap().is_some()
    }

    /// Génération courante des spawns — comparée avant/après une
    /// opération pour détecter un kill/rebind intervenu entre-temps.
    pub fn generation(&self) -> u64 {
        self.spawn_gen.load(Ordering::SeqCst)
    }

    /// Dernière génération invalidée par un kill_for_respawn — > `gen0`
    /// (génération au départ d'un spawn) = supplanté pendant le spawn.
    pub fn supplant_mark(&self) -> u64 {
        self.supplant_mark.load(Ordering::SeqCst)
    }

    /// Vrai si l'état persisté implique un tour de travail en vol au
    /// chargement — à lire AVANT reconcile_state() (qui repasse tout en
    /// "interrupted"). Sert à la reprise auto après un arrêt brutal du
    /// démon : "idle" est volontairement exclu (agent vivant mais sans
    /// travail en cours → pas de prompt "continue please").
    pub fn was_active(&self) -> bool {
        matches!(
            self.meta_snapshot().state.as_str(),
            "running" | "working" | "waiting_input" | "starting" | "compacting"
        )
    }

    /// À l'ouverture du démon : un état persisté qui implique un agent
    /// vivant repasse en interrupted — et on le journalise pour que les
    /// replays d'historique côté client ne ressuscitent pas "working".
    pub fn reconcile_state(&self) {
        let needs = {
            let mut m = self.meta.lock().unwrap();
            match m.state.as_str() {
                "running" | "working" | "waiting_input" | "idle" | "starting" | "compacting" => {
                    m.state = "interrupted".into();
                    true
                }
                "error" => {
                    // L'erreur concernait un process mort avec le démon
                    // et aucun hold ne survit à l'open (en mémoire) —
                    // garder « error » figerait la session (les gardes
                    // orch/acctl bloqueraient toute relance) alors que
                    // rien n'est réellement retenu.
                    m.state = "interrupted".into();
                    m.last_error = None;
                    true
                }
                _ => false,
            }
        };
        if needs {
            self.save_meta();
            self.emit(EventKind::StatusChanged {
                state: "interrupted".into(),
            });
        }
    }
}

/// session/load en échec : vrai quand l'erreur signale une session
/// native encore VERROUILLÉE ou un empêchement transitoire — le repli
/// session/new serait alors une perte de contexte silencieuse (la
/// session existe toujours, tenue par un TUI ou un autre démon), et le
/// retry de spawn_attempts/fork peut la libérer. Tout le reste est
/// traité comme définitif (« Session not found », store vidé, id
/// inconnu, erreur interne de l'agent) → repli sur session/new :
/// une session recréée vaut mieux qu'une session coincée à vie.
fn load_err_keeps_native(msg: &str) -> bool {
    let m = msg.to_lowercase();
    [
        "already open",
        "in use",
        "locked",
        "busy",
        "temporar",
        "timeout",
        "timed out",
        "unavailable",
    ]
    .iter()
    .any(|k| m.contains(k))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::Journal;
    use crate::server::CompactSrc;
    use proto::SessionMeta;
    use serde_json::json;
    use std::path::PathBuf;

    fn test_compact_src() -> CompactSrc {
        CompactSrc {
            global: crate::server::CompactCfg::default(),
            by_agent: Default::default(),
        }
    }

    fn tmp_session(tag: &str, native_id: &str) -> (Arc<Session>, PathBuf) {
        tmp_session_cfg(tag, native_id, None, None)
    }

    fn tmp_session_cfg(
        tag: &str,
        native_id: &str,
        mode: Option<&str>,
        model: Option<&str>,
    ) -> (Arc<Session>, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("ac-spawn-test-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = SessionMeta {
            id: "s-test".into(),
            agent: "test".into(),
            title: "t".into(),
            cwd: "/tmp".into(),
            created_at: proto::now_iso(),
            native_session_id: Some(native_id.to_string()),
            mode: mode.map(String::from),
            model: model.map(String::from),
            model_source: None,
            available_modes: vec![],
            available_commands: vec![],
            available_models: vec![],
            config_options: vec![],
            pending_config: Default::default(),
            desired_config: Default::default(),
            state: "interrupted".into(),
            group: None,
            spawned_by: None,
            origin: None,
            context_used: None,
            context_size: None,
            last_error: None,
            wait: None,
        };
        std::fs::write(
            dir.join("meta.json"),
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();
        (
            Session::open(dir.clone(), &test_compact_src(), CapsRegistry::in_memory()).unwrap(),
            dir,
        )
    }

    /// Faux agent ACP en python : initialize → loadSession annoncé ;
    /// session/load sur « gone-native » → « Session not found », sinon
    /// écho de l'id ; session/new → « new-native-1 » (ou l'échec du
    /// mode `fail_new`)；session/prompt → end_turn, sauf « live-native »
    /// dont la session a disparu en pleine vie (« Session not found »).
    /// `fail_new` couvre le cas où le repli lui-même échoue (agent cassé).
    fn fake_agent(dir: &PathBuf, fail_new: bool) -> PathBuf {
        let script = dir.join("fake-agent.py");
        let new_branch = if fail_new {
            r#"err(mid, "session/new refused")"#
        } else {
            r#"ok(mid, {"sessionId": "new-native-1"})"#
        };
        std::fs::write(
            &script,
            format!(
                r#"import sys, json

def ok(mid, result):
    print(json.dumps({{"jsonrpc": "2.0", "id": mid, "result": result}}), flush=True)

def err(mid, message):
    print(json.dumps({{"jsonrpc": "2.0", "id": mid,
        "error": {{"code": -32000, "message": message}}}}), flush=True)

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    params = msg.get("params") or {{}}
    if mid is None:
        continue
    if method == "initialize":
        ok(mid, {{"protocolVersion": 1,
                 "agentCapabilities": {{"loadSession": True}}}})
    elif method == "session/load":
        if params.get("sessionId") == "gone-native":
            err(mid, "Session not found")
        else:
            ok(mid, {{"sessionId": params.get("sessionId")}})
    elif method == "session/new":
        {new_branch}
    elif method == "session/prompt":
        if params.get("sessionId") == "live-native":
            err(mid, "Session not found")
        else:
            ok(mid, {{"stopReason": "end_turn"}})
"#
            ),
        )
        .unwrap();
        script
    }

    /// Variante : chaque requête est loguée dans `req.log` (method +
    /// params, une ligne JSON) et session/load rapporte le mode persisté
    /// côté agent (« default ») — pas celui demandé — via configOptions,
    /// comme le fait Devin au resume.
    fn fake_agent_mode_report(dir: &PathBuf) -> PathBuf {
        let script = dir.join("fake-agent-mode.py");
        let log = dir.join("req.log");
        std::fs::write(
            &script,
            format!(
                r#"import sys, json

LOG = {log:?}

def ok(mid, result):
    print(json.dumps({{"jsonrpc": "2.0", "id": mid, "result": result}}), flush=True)

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    params = msg.get("params") or {{}}
    if mid is None:
        continue
    with open(LOG, "a") as f:
        f.write(json.dumps({{"method": method, "params": params}}) + "\n")
    if method == "initialize":
        ok(mid, {{"protocolVersion": 1,
                 "agentCapabilities": {{"loadSession": True}}}})
    elif method == "session/load":
        ok(mid, {{"sessionId": params.get("sessionId"),
                 "configOptions": [
                     {{"id": "mode", "currentValue": "default",
                       "options": [{{"value": "default"}},
                                   {{"value": "bypass"}}]}}]}})
    elif method == "session/new":
        ok(mid, {{"sessionId": "new-native-1"}})
    elif method in ("session/set_config_option", "session/set_mode",
                    "session/set_model"):
        ok(mid, {{}})
    elif method == "session/prompt":
        ok(mid, {{"stopReason": "end_turn"}})
"#,
                log = log.display().to_string()
            ),
        )
        .unwrap();
        script
    }

    fn script_texts(dir: &PathBuf) -> Vec<String> {
        let j = Journal::open(dir).unwrap();
        j.read_page(dir, None, 200)
            .unwrap()
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::System { text } | EventKind::Error { message: text } => {
                    Some(text.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// Un python disponible ? Sans lui le faux agent ne peut pas
    /// tourner — le test s'auto-skip plutôt que d'échouer sur un env.
    fn python() -> Option<String> {
        for c in ["python3", "python"] {
            if std::process::Command::new(c)
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
            {
                return Some(c.to_string());
            }
        }
        None
    }

    #[test]
    fn load_err_classification() {
        // Session verrouillée/transitoire → pas de repli.
        for m in [
            "acp session/load: already open in another process",
            "resource is busy",
            "timed out waiting for session lock",
            "Session LOCKED by tui",
        ] {
            assert!(load_err_keeps_native(m), "{m}");
        }
        // Session définitivement perdue → repli session/new.
        for m in [
            "acp session/load: Session not found",
            "No such session",
            "unknown session id",
            "internal error",
            "EOF",
        ] {
            assert!(!load_err_keeps_native(m), "{m}");
        }
    }

    /// Bug remonté : restart démon → session/load « Session not found »
    /// → session coincée à vie. Le spawn doit dégrader en session/new
    /// sur le même process : session utilisable, journal conservé,
    /// ligne système explicative, native_session_id remplacé.
    #[tokio::test]
    async fn spawn_load_falls_back_to_new() {
        let Some(py) = python() else {
            eprintln!("python absent — test ignoré");
            return;
        };
        let (s, dir) = tmp_session("fallback", "gone-native");
        let agent = fake_agent(&dir, false);
        let cmd = vec![py, agent.display().to_string()];
        let ok = s
            .spawn_agent(
                &cmd,
                &Default::default(),
                Some("gone-native".into()),
                true,
                json!([]),
                None,
            )
            .await
            .unwrap();
        assert!(ok);
        assert!(s.is_alive());
        // L'id natif a été remplacé par celui de la session/new.
        assert_eq!(
            s.meta_snapshot().native_session_id.as_deref(),
            Some("new-native-1")
        );
        let texts = script_texts(&dir);
        assert!(
            texts.iter().any(|t| t.contains("session native perdue")),
            "sysline de repli absente: {texts:?}"
        );
        // La session reste utilisable : un prompt part, aucun
        // « prompt failed » ne retombe (le faux agent répond end_turn).
        s.send_prompt("ping", false, false).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let texts = script_texts(&dir);
        assert!(
            !texts.iter().any(|t| t.contains("prompt failed")),
            "prompt en échec après repli: {texts:?}"
        );
        s.kill().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Bug remonté : sleep d'une session bypass → session/load rapporte
    /// le mode persisté côté agent (« default ») qui écrasait meta.mode
    /// sans rien re-pousser → l'agent restait en mode par défaut. Le
    /// reconcile post-handshake doit envoyer session/set_config_option
    /// « mode »=bypass et garder meta.mode sur la valeur demandée.
    #[tokio::test]
    async fn load_mode_mismatch_is_repushed() {
        let Some(py) = python() else {
            eprintln!("python absent — test ignoré");
            return;
        };
        let (s, dir) = tmp_session_cfg("modefix", "native-mode", Some("bypass"), None);
        let agent = fake_agent_mode_report(&dir);
        let cmd = vec![py, agent.display().to_string()];
        let ok = s
            .spawn_agent(
                &cmd,
                &Default::default(),
                Some("native-mode".into()),
                true,
                json!([]),
                None,
            )
            .await
            .unwrap();
        assert!(ok && s.is_alive());
        // meta.mode reste sur la valeur demandée — pas le « default »
        // rapporté (sinon spawn_args/env repartent faux au prochain
        // réveil).
        assert_eq!(s.meta_snapshot().mode.as_deref(), Some("bypass"));
        // Le mode a été re-poussé vers l'agent : set_config_option ou
        // set_mode avec « bypass » doit apparaître dans req.log.
        let log = std::fs::read_to_string(dir.join("req.log")).unwrap();
        assert!(
            log.contains("\"session/set_config_option\"") && log.contains("\"bypass\"")
                || log.contains("\"session/set_mode\"") && log.contains("\"bypass\""),
            "mode non re-poussé: {log}"
        );
        s.kill().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// session/load échoue ET session/new aussi → spawn en échec, mais
    /// le process ne doit pas rester zombie : is_alive faux (sinon
    /// « already » au resume et « working » au send, tous deux en
    /// échec « Session not found » — l'état mensonger remonté).
    #[tokio::test]
    async fn failed_handshake_leaves_no_zombie() {
        let Some(py) = python() else {
            eprintln!("python absent — test ignoré");
            return;
        };
        let (s, dir) = tmp_session("zombie", "gone-native");
        let agent = fake_agent(&dir, true);
        let cmd = vec![py, agent.display().to_string()];
        let r = s
            .spawn_agent(
                &cmd,
                &Default::default(),
                Some("gone-native".into()),
                true,
                json!([]),
                None,
            )
            .await;
        assert!(r.is_err());
        // Pas de process survivant → resume/send pourront retenter.
        assert!(!s.is_alive());
        // Laisser le reader digérer l'exit : l'exit du process tué par
        // le handshake ne doit produire ni « agent process exited » ni
        // dead — l'état appartient à l'appelant (ici : interrupted).
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert_eq!(s.meta_snapshot().state, "interrupted");
        let texts = script_texts(&dir);
        assert!(
            !texts.iter().any(|t| t.contains("agent process exited")),
            "exit non supprimé: {texts:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Liaison morte en pleine vie : l'agent était bound (« live-native »
    /// chargé au spawn) puis session/prompt répond « Session not found »
    /// — le process ne peut plus rien servir pour cette session. Le
    /// tuer rend is_alive faux → resume/« already » débloqués ; l'exit
    /// normal (dead + « agent process exited ») s'applique : honnête.
    #[tokio::test]
    async fn lost_binding_kills_process() {
        let Some(py) = python() else {
            eprintln!("python absent — test ignoré");
            return;
        };
        let (s, dir) = tmp_session("binding", "live-native");
        let agent = fake_agent(&dir, false);
        let cmd = vec![py, agent.display().to_string()];
        let ok = s
            .spawn_agent(
                &cmd,
                &Default::default(),
                Some("live-native".into()),
                true,
                json!([]),
                None,
            )
            .await
            .unwrap();
        assert!(ok && s.is_alive());
        s.send_prompt("ping", false, false).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(!s.is_alive());
        assert_eq!(s.meta_snapshot().state, "dead");
        let texts = script_texts(&dir);
        assert!(
            texts.iter().any(|t| t.contains("prompt failed")),
            "échec du prompt non journalisé: {texts:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn binding_lost_classification() {
        use crate::session::prompt::session_binding_lost;
        for m in [
            "acp session/prompt: Session not found",
            "acp session/load: unknown session",
            "no such session abc",
            "la session est introuvable",
            "acp session/prompt: session does not exist",
            "Session not found",
        ] {
            assert!(session_binding_lost(m), "{m}");
        }
        // Erreurs non-liaison — y compris wrappées sous le préfixe
        // « acp session/prompt: » (qui contient « session » : il ne doit
        // pas activer la classification à lui seul).
        for m in [
            "file not found",
            "acp session/prompt: file not found",
            "acp session/prompt: unknown tool",
            "acp session/prompt: model does not exist",
            "acp session/prompt: tool call failed",
            "network error",
        ] {
            assert!(!session_binding_lost(m), "{m}");
        }
    }
}
