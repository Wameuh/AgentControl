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
    /// `spawn_cfg` : (mode, model) demandés à la création — appliqués via
    /// ACP tant que `spawning` est encore vrai, donc avant tout drain ou
    /// envoi concurrent (le premier prompt voit la config demandée).
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
        spawn_cfg: Option<(Option<String>, Option<String>)>,
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
            if let Some((mode, model)) = &spawn_cfg {
                self.apply_spawn_config(mode.as_deref(), model.as_deref())
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

        // ACP handshake
        let init = self
            .acp_request("initialize", acp::initialize_params())
            .await
            .context("initialize")?;
        let agent_caps = init.get("agentCapabilities").cloned().unwrap_or(json!({}));
        let supports_load = agent_caps
            .get("loadSession")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let resp = if let (Some(native), true) = (resume_native_id.clone(), supports_load) {
            self.loading_replay.store(skip_replay, Ordering::SeqCst);
            let r = self
                .acp_request(
                    "session/load",
                    acp::session_load_params(&native, &meta.cwd, mcp_servers.clone()),
                )
                .await;
            self.loading_replay.store(false, Ordering::SeqCst);
            r
        } else {
            self.acp_request(
                "session/new",
                acp::session_new_params(&meta.cwd, mcp_servers),
            )
            .await
        };
        let resp = resp.context("session/new|load")?;

        if let Some(sid) = resp.get("sessionId").and_then(Value::as_str) {
            let mut m = self.meta.lock().unwrap();
            m.native_session_id = Some(sid.to_string());
        }
        // Modes disponibles éventuels
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
            }
        }
        self.save_meta();
        // Un kill (sleep/clear/respawn concurrent) a invalidé la génération
        // pendant le handshake : le process vient d'être tué — ni line de
        // spawn, ni replay de config, ni "idle" (ils pollueraient le cycle
        // qui a pris le relais ; la config reste en pending_config pour le
        // prochain spawn).
        if self.spawn_gen.load(Ordering::SeqCst) != gen {
            return Ok(false);
        }
        self.emit(EventKind::System {
            text: if resume_native_id.is_some() {
                format!("agent respawned (session/load={}, resumed)", supports_load)
            } else {
                "agent started".into()
            },
        });
        // Config demandée pendant que l'agent était mort : rejouer maintenant
        // (modèle/mode choisis hors-ligne appliqués au resume).
        let pending: Vec<(String, String)> = {
            self.meta.lock().unwrap().pending_config.drain().collect()
        };
        if !pending.is_empty() {
            self.save_meta(); // persiste la map vidée même si les appels échouent
        }
        for (k, v) in pending {
            let r = match k.as_str() {
                "model" => match self.set_config_option("model", &v).await {
                    Ok(()) => Ok(()),
                    Err(_) => self.set_model(&v).await.map(|_| ()),
                },
                "mode" => match self.set_config_option("mode", &v).await {
                    Ok(()) => Ok(()),
                    Err(_) => self.set_mode(&v).await,
                },
                _ => self.set_config_option(&k, &v).await,
            };
            if let Err(e) = r {
                self.emit(EventKind::Error {
                    message: format!("config différée {k}={v}: {e:#}"),
                });
            }
        }
        self.set_state("idle");
        Ok(true)
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
                        {
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
                        }
                        self.save_meta();
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
            } => {
                if !self.loading_replay.load(Ordering::SeqCst) {
                    self.flush_text();
                    self.apply_tool_update(call_id, status, text, diff, title, kind, locations);
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
