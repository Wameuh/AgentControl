//! Registre PR ↔ session : handlers `pr_*`/`forge_list` + détection auto
//! dans les sorties de tool calls (watcher broadcast + backfill du
//! journal).

use super::*;
use proto::EventKind;

use crate::forges::{Forges, PrRef};
use crate::prs::{ref_key, PrRegistry, TrackIn};

/// Intervalle minimal entre deux syncs réussis — l'onglet Dépôts du
/// client redemande un sync à l'ouverture et périodiquement.
const PR_SYNC_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(45);
/// Un run encore en vol passé ce délai est considéré coincé (gh
/// suspendu, socket morte) → un nouveau run est autorisé à repartir.
const PR_SYNC_STUCK: std::time::Duration = std::time::Duration::from_secs(180);
/// L'appelant n'attend pas indéfiniment : passé ce délai la réponse
/// part en `running` — le worker termine quand même et applique ses
/// updates en tâche de fond.
const PR_SYNC_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// Retentatives d'une vérif « forge injoignable » — délais successifs
/// (≈15 min de fenêtre totale), puis abandon : la référence n'est pas
/// enregistrée. Une URL réapparaissant plus tard en sortie reprendra
/// le cycle depuis zéro — la détection reste best-effort.
const VERIFY_BACKOFF: [std::time::Duration; 5] = [
    std::time::Duration::from_secs(10),
    std::time::Duration::from_secs(30),
    std::time::Duration::from_secs(90),
    std::time::Duration::from_secs(240),
    std::time::Duration::from_secs(600),
];
/// Pause minimale entre deux appels forge du worker — la file est
/// séquentielle, la pause borne le débit quand un backfill y vide
/// d'un coup des dizaines de placeholders.
const VERIFY_GAP: std::time::Duration = std::time::Duration::from_millis(300);
/// Une référence vérifiée « inexistante » reste refusée sans nouvel
/// appel forge pendant ce délai — borné : un placeholder cité dans une
/// doc puis réellement créé redevient enregistrable à expiration.
const VERIFY_NEG_TTL: std::time::Duration = std::time::Duration::from_secs(30 * 60);
/// Bornes mémoire de la file et du cache négatif — un flood d'URLs
/// distinctes ne doit ni gonfler le démon ni marteler la forge.
const VERIFY_PENDING_CAP: usize = 256;
const VERIFY_NEG_CAP: usize = 1024;

/// Retour de `PrVerifier::submit` — ce que le watcher doit faire de la
/// référence qu'il vient d'extraire.
pub(super) enum Submit {
    /// Entrée déjà connue — `detected` a été appliqué dans le chemin
    /// synchrone (le booléen = son retour : créée/réattribuée).
    Applied(bool),
    /// Référence nouvelle — vérification forge en file ; l'event de
    /// confirmation partira du worker quand le verdict tombera.
    Queued,
    /// Rien à faire : verdict « inexistant » encore frais, ou file
    /// pleine (flood) — l'occurrence est simplement ignorée.
    Rejected,
}

/// Référence en attente de verdict forge.
struct PendingRef {
    /// Session à laquelle attribuer l'entrée si la référence existe —
    /// remplacée par la dernière détection « forte » (création réelle).
    sid: String,
    /// Session d'émission des events de verdict — faible : une session
    /// droppée ne doit pas retenir l'entrée ni paniquer l'emit.
    session: std::sync::Weak<Session>,
    r: PrRef,
    strong: bool,
    /// Nombre de retentatives déjà programmées (forge injoignable).
    attempts: u32,
    next_try: tokio::time::Instant,
}

/// Vérification d'existence forge des références nouvelles détectées —
/// la barrière structurelle anti-placeholders : une URL lue dans une
/// sortie outil n'entre au registre qu'après preuve qu'elle désigne une
/// vraie review (repo + référence vus par la forge).
///
/// Jamais dans le chemin du watcher : `submit` est synchrone (lookup
/// registre + cache négatif + file) ; les appels forge courent dans un
/// worker dédié, UN à la fois (`spawn_blocking` + `VERIFY_GAP`).
/// Cache négatif borné+TTL → zéro appel par ré-occurrence d'une URL
/// placebo ; file dédupliquée par clé de référence → zéro appel par
/// répétition en vol. Forge injoignable → retentatives en backoff
/// borné, puis abandon (voir `VERIFY_BACKOFF`).
pub(super) struct PrVerifier {
    /// Registre — `known`/`detected` y sont appliqués.
    pub(super) prs: Arc<PrRegistry>,
    /// Existence-check — `forgesync::verify_ref` en prod, stub injecté
    /// en test (le worker s'exerce alors entièrement sans réseau).
    check: Arc<dyn Fn(&forgesync::RefTarget) -> forgesync::Verify + Send + Sync>,
    /// Références en attente de verdict — clé `ref_key` : la dédup est
    /// naturelle, une seconde détection fusionne (fort prime sur sid).
    pending: std::sync::Mutex<std::collections::BTreeMap<String, PendingRef>>,
    /// Vérifiées « inexistantes » récemment (clé → instant du verdict).
    negative: std::sync::Mutex<HashMap<String, tokio::time::Instant>>,
    /// Réveil du worker à l'arrivée d'une référence (ou d'un retry).
    notify: tokio::sync::Notify,
    /// Paliers de retentative « forge injoignable » — `VERIFY_BACKOFF`
    /// en prod, délais milliseconde injectés en test.
    backoff: &'static [std::time::Duration],
}

impl PrVerifier {
    pub(super) fn new(
        prs: Arc<PrRegistry>,
        check: impl Fn(&forgesync::RefTarget) -> forgesync::Verify + Send + Sync + 'static,
    ) -> Self {
        Self::with_timing(prs, check, &VERIFY_BACKOFF)
    }

    fn with_timing(
        prs: Arc<PrRegistry>,
        check: impl Fn(&forgesync::RefTarget) -> forgesync::Verify + Send + Sync + 'static,
        backoff: &'static [std::time::Duration],
    ) -> Self {
        Self {
            prs,
            check: Arc::new(check),
            pending: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            negative: std::sync::Mutex::new(HashMap::new()),
            notify: tokio::sync::Notify::new(),
            backoff,
        }
    }

    /// Propose une référence extraite d'une sortie outil. Synchrone et
    /// borné — appelable depuis le watcher broadcast comme le backfill.
    pub(super) fn submit(
        &self,
        s: &Arc<Session>,
        sid: &str,
        r: PrRef,
        strong: bool,
    ) -> Result<Submit> {
        // Entrée déjà tranchée → pas d'appel forge : la détection
        // reprend son rôle (réattribution « forte » éventuelle).
        if self.prs.known(&r) {
            return Ok(Submit::Applied(self.prs.detected(sid, &r, strong)?));
        }
        let key = ref_key(&r);
        if self.negative_hit(&key) {
            return Ok(Submit::Rejected);
        }
        {
            let mut g = self.pending.lock().unwrap();
            match g.get_mut(&key) {
                Some(p) => {
                    // La détection « forte » la plus récente prime pour
                    // l'attribution (gh pr create d'une autre session
                    // arrivé pendant la file).
                    if strong {
                        p.sid = sid.into();
                        p.session = Arc::downgrade(s);
                    }
                    p.strong |= strong;
                }
                None => {
                    if g.len() >= VERIFY_PENDING_CAP {
                        warn!("[{sid}] prs verify: file pleine — {} ignorée", r.display());
                        return Ok(Submit::Rejected);
                    }
                    g.insert(
                        key,
                        PendingRef {
                            sid: sid.into(),
                            session: Arc::downgrade(s),
                            r,
                            strong,
                            attempts: 0,
                            next_try: tokio::time::Instant::now(),
                        },
                    );
                }
            }
        }
        self.notify.notify_one();
        Ok(Submit::Queued)
    }

    /// Worker unique de vérification — spawné une fois au `Daemon::open`.
    pub(super) fn spawn_worker(self: &Arc<Self>) {
        let me = Arc::clone(self);
        tokio::spawn(async move { me.run().await });
    }

    async fn run(&self) {
        loop {
            let next = {
                let g = self.pending.lock().unwrap();
                g.values().map(|p| p.next_try).min()
            };
            match next {
                // File vide : sommeil jusqu'au prochain submit.
                None => self.notify.notified().await,
                // Item pas encore mûr : sommeil borné, réveillé avant
                // terme si un item immédiat entre entre-temps.
                Some(t) => {
                    let _ = tokio::time::timeout_at(t, self.notify.notified()).await;
                }
            }
            let Some((key, p)) = self.pop_due() else {
                continue;
            };
            self.check_one(key, p).await;
            tokio::time::sleep(VERIFY_GAP).await;
        }
    }

    /// Retire l'item arrivé à échéance (le plus ancien `next_try` ≤ now).
    fn pop_due(&self) -> Option<(String, PendingRef)> {
        let now = tokio::time::Instant::now();
        let mut g = self.pending.lock().unwrap();
        let k = g
            .iter()
            .find(|(_, p)| p.next_try <= now)
            .map(|(k, _)| k.clone())?;
        g.remove(&k).map(|p| (k, p))
    }

    /// Vérifie une référence et applique le verdict : enregistrement,
    /// cache négatif, ou retentative/abandon.
    async fn check_one(&self, key: String, p: PendingRef) {
        // Entrée créée entre-temps (pr add manuel, autre branche de la
        // file) → la vérif n'a plus lieu d'être, détection directe.
        if self.prs.known(&p.r) {
            self.apply(&p);
            return;
        }
        // Deuxième file pour la même clé soumise pendant le premier
        // appel : si le verdict « inexistant » est tombé entre-temps,
        // pas de second appel forge.
        if self.negative_hit(&key) {
            return;
        }
        let check = Arc::clone(&self.check);
        let t = forgesync::RefTarget {
            forge: p.r.forge.to_string(),
            host: p.r.host.clone(),
            repo: p.r.repo.clone(),
            pr: p.r.pr,
            change_id: p.r.change_id.clone(),
        };
        let verdict = tokio::task::spawn_blocking(move || check(&t)).await;
        match verdict {
            Ok(forgesync::Verify::Exists) => self.apply(&p),
            Ok(forgesync::Verify::Missing) => {
                self.negative_add(key);
                self.emit(
                    &p,
                    format!(
                        "PR {} ignorée — introuvable sur {}",
                        p.r.display(),
                        p.r.forge
                    ),
                );
            }
            // Erreur de join = panic du checker : même destin qu'un
            // injoignable (retentative bornée), jamais d'enregistrement.
            Ok(forgesync::Verify::Unreachable) | Err(_) => self.retry_or_drop(key, p),
        }
    }

    /// Verdict « existe » (ou entrée apparue entre-temps) — l'entrée
    /// registre et l'event de confirmation de la session.
    fn apply(&self, p: &PendingRef) {
        match self.prs.detected(&p.sid, &p.r, p.strong) {
            Ok(true) => self.emit(p, format!("PR détectée et enregistrée : {}", p.r.display())),
            Ok(false) => {}
            Err(e) => warn!("[{}] prs.detected: {e:#}", p.sid),
        }
    }

    /// Forge injoignable : retentative en backoff tant qu'il reste des
    /// paliers, sinon abandon — la référence n'entre jamais au registre
    /// (un nouvel extrait la fera repartir de zéro).
    fn retry_or_drop(&self, key: String, mut p: PendingRef) {
        if (p.attempts as usize) < self.backoff.len() {
            p.next_try = tokio::time::Instant::now() + self.backoff[p.attempts as usize];
            p.attempts += 1;
            self.pending.lock().unwrap().insert(key, p);
            self.notify.notify_one();
        } else {
            warn!(
                "[{}] PR {} non enregistrée — forge injoignable après {} tentatives",
                p.sid,
                p.r.display(),
                p.attempts
            );
            self.emit(
                &p,
                format!("PR {} non enregistrée — forge injoignable", p.r.display()),
            );
        }
    }

    /// Event journalisé sur la session porteuse si elle est encore
    /// vivante — la traçabilité du verdict (refus compris) est aussi
    /// précieuse que la détection elle-même.
    fn emit(&self, p: &PendingRef, text: String) {
        if let Some(s) = p.session.upgrade() {
            s.emit(EventKind::System { text });
        }
    }

    /// La clé figure-t-elle au cache négatif encore frais ?
    fn negative_hit(&self, key: &str) -> bool {
        let mut n = self.negative.lock().unwrap();
        match n.get(key) {
            Some(t) if t.elapsed() < VERIFY_NEG_TTL => true,
            Some(_) => {
                n.remove(key);
                false
            }
            None => false,
        }
    }

    /// Enregistre un verdict « inexistant » — map bornée : expirés
    /// purgés à la borne ; toujours pleine → insertion sautée (la ref
    /// sera re-vérifiée à sa prochaine occurrence — pas d'OOM).
    fn negative_add(&self, key: String) {
        let mut n = self.negative.lock().unwrap();
        if n.len() >= VERIFY_NEG_CAP {
            n.retain(|_, t| t.elapsed() < VERIFY_NEG_TTL);
            if n.len() >= VERIFY_NEG_CAP {
                return;
            }
        }
        n.insert(key, tokio::time::Instant::now());
    }
}

/// État d'un `pr_sync` — Mutex non async : les sections critiques ne
/// contiennent aucun await.
#[derive(Default)]
pub(super) struct PrSyncCtl {
    /// Départ du run en vol (None = aucun).
    running_since: Option<std::time::Instant>,
    /// Fin du dernier run abouti (throttle).
    last_done: Option<std::time::Instant>,
}

impl Daemon {
    // -----------------------------------------------------------------------
    // Registre PR : détection auto dans les sorties de tool calls
    // -----------------------------------------------------------------------

    /// Branche la détection PR sur une session : scan rétroactif du journal
    /// (backfill borne — les PRs créées avant le démarrage sont détectées),
    /// puis un watcher sur le broadcast des events. Le watcher meurt tout
    /// seul quand la session est droppée (sender fermé / Weak épuisé).
    pub(super) fn watch_prs(&self, s: &Arc<Session>) {
        let sid = s.meta_snapshot().id;
        if let Ok(evs) = s.journal.read_page(&s.dir, None, 2000) {
            let mut titles = HashMap::new();
            for ev in &evs {
                scan_pr_event(&self.pr_verify, &self.forges, s, &sid, ev, &mut titles);
            }
        }
        let verify = Arc::clone(&self.pr_verify);
        let forges = self.forges.clone();
        let weak = Arc::downgrade(s);
        let mut rx = s.events_tx.subscribe();
        tokio::spawn(async move {
            let mut titles: HashMap<String, String> = HashMap::new();
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        let Some(s) = weak.upgrade() else { break };
                        scan_pr_event(&verify, &forges, &s, &sid, &ev, &mut titles);
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }


    pub(super) async fn h_pr_list(self: &Arc<Self>, session: Option<String>) -> Result<Value> {
                let mut v = self.prs.list();
                if let Some(sid) = session {
                    v.retain(|e| e.session == sid);
                }
                Ok(serde_json::to_value(v)?)
    }
    pub(super) async fn h_pr_track(
        self: &Arc<Self>,
        session: Option<String>,
        repo: String,
        pr: u64,
        forge: Option<String>,
        host: Option<String>,
        change_id: Option<String>,
        url: Option<String>,
        branch: Option<String>,
        title: Option<String>,
        status: Option<String>,
        note: Option<String>,
        sync_fails: Option<u32>,
    ) -> Result<Value> {
                let e = self.prs.track(TrackIn {
                    session,
                    repo,
                    pr,
                    forge: forge.unwrap_or_default(),
                    host: host.unwrap_or_default(),
                    change_id,
                    url,
                    branch,
                    title,
                    status,
                    note,
                    sync_fails,
                })?;
                Ok(serde_json::to_value(e)?)
    }
    pub(super) async fn h_pr_untrack(
        self: &Arc<Self>,
        repo: String,
        pr: u64,
        forge: Option<String>,
        host: Option<String>,
        change_id: Option<String>,
    ) -> Result<Value> {
                Ok(json!({ "removed": self.prs.remove(
                    forge.as_deref().unwrap_or(""),
                    host.as_deref().unwrap_or(""),
                    &repo,
                    pr,
                    change_id.as_deref(),
                )? }))
    }
    /// Set effectif des forges de review (détection + sync) — github et
    /// gitlab.com implicites + `[[forges]]` de la config. `token_env` est
    /// un nom de variable d'environnement, jamais le token.
    pub(super) async fn h_forge_list(self: &Arc<Self>) -> Result<Value> {
                Ok(self.forges.list_json())
    }

    /// `pr_sync` : resynchronise les statuts du registre depuis les
    /// forges live — même collecte que `acctl pr sync` (crate
    /// forgesync : gh pour github, REST gitlab/gerrit), appliquée
    /// directement au registre (`session: None` = retouche métadonnées,
    /// ne sanctifie pas `via`). Throttlé à PR_SYNC_MIN_INTERVAL et
    /// non réentrant : les onglets Dépôts de plusieurs clients peuvent
    /// demander en rafale sans dupliquer les appels forge. La réponse
    /// peut prendre des dizaines de secondes (REST séquentiel à 8 s par
    /// entrée injoignable) — les clients bornent leur attente (60 s)
    /// et relisent le registre au refresh suivant : le canal résultat
    /// est best-effort, la convergence passe par le registre.
    pub(super) async fn h_pr_sync(self: &Arc<Self>) -> Result<Value> {
        {
            let mut c = self.pr_sync.lock().unwrap();
            if c.running_since.is_some_and(|t| t.elapsed() < PR_SYNC_STUCK) {
                return Ok(json!({ "synced": false, "running": true }));
            }
            // Un run « coincé » (> PR_SYNC_STUCK) n'exclut plus — ses
            // updates tardives restent idempotentes si elles atterrissent.
            if c.last_done.is_some_and(|t| t.elapsed() < PR_SYNC_MIN_INTERVAL) {
                return Ok(json!({ "synced": false, "throttled": true }));
            }
            c.running_since = Some(std::time::Instant::now());
        }
        let prs = Arc::clone(&self.prs);
        let set = self.forges.list_json();
        let ctl = Arc::clone(&self.pr_sync);
        let work = tokio::task::spawn_blocking(move || {
            let entries: Vec<Value> = prs
                .list()
                .iter()
                .filter_map(|e| serde_json::to_value(e).ok())
                .collect();
            let set = set.as_array().cloned().unwrap_or_default();
            let out = forgesync::collect_updates(&set, &entries);
            let mut applied = 0usize;
            for u in &out.updates {
                match prs.track(TrackIn {
                    session: None,
                    repo: u.repo.clone(),
                    pr: u.pr,
                    forge: u.forge.clone(),
                    host: u.host.clone(),
                    change_id: u.change_id.clone(),
                    url: u.url.clone(),
                    branch: u.branch.clone(),
                    title: u.title.clone(),
                    status: Some(u.status.clone()),
                    note: None,
                    sync_fails: None,
                }) {
                    Ok(_) => applied += 1,
                    Err(e) => warn!("pr_sync track {} {}: {e:#}", u.repo, u.pr),
                }
            }
            // Compteur d'échecs par entrée : vues live → 0, injoignables
            // → +1 (les updates déjà trackées sont des « vues » aussi).
            // Au-delà de PR_UNRESOLVED_AFTER l'entrée s'affiche
            // « non résoluble » côté client.
            for s in &out.seen {
                if let Err(e) = prs.sync_seen(
                    &s.forge,
                    &s.host,
                    &s.repo,
                    s.pr,
                    s.change_id.as_deref(),
                    true,
                ) {
                    warn!("pr_sync seen {} {}: {e:#}", s.repo, s.pr);
                }
            }
            for s in &out.failed {
                if let Err(e) = prs.sync_seen(
                    &s.forge,
                    &s.host,
                    &s.repo,
                    s.pr,
                    s.change_id.as_deref(),
                    false,
                ) {
                    warn!("pr_sync fail {} {}: {e:#}", s.repo, s.pr);
                }
            }
            // Soldé quoi qu'il arrive : les notes restent dans la
            // réponse/logs, le throttle repart de la fin du run.
            let mut c = ctl.lock().unwrap();
            c.running_since = None;
            c.last_done = Some(std::time::Instant::now());
            (out, applied)
        });
        match tokio::time::timeout(PR_SYNC_REPLY_TIMEOUT, work).await {
            Ok(Ok((out, applied))) => Ok(json!({
                "synced": true,
                "checked": out.checked,
                "updated": applied,
                "failed": out.failed.len(),
                "notes": out.notes,
            })),
            // Panic du worker : l'état reste « en vol » jusqu'à
            // PR_SYNC_STUCK — un crash ne doit pas relancer en rafale.
            Ok(Err(e)) => Err(anyhow::anyhow!("pr_sync: {e:#}")),
            // Timeout de réponse : le worker continue en tâche de fond,
            // les updates tomberont (running_since est soldé par lui).
            Err(_) => Ok(json!({ "synced": false, "running": true })),
        }
    }
}

/// Le titre d'un tool call (la commande pour un exec) évoque une
/// création de review — signal fort pour la réattribution d'une entrée
/// détectée. Un `gh pr view|checks|merge` / `glab mr view` d'une autre
/// session ne doit PAS pouvoir réattribuer l'entrée.
fn pr_cmd_title(title: &str) -> bool {
    let t = title.to_lowercase();
    t.contains("pr create")        // gh pr create
        || t.contains("mr create") // glab mr create
        || t.contains("git review") // git-review (gerrit)
        || t.contains("refs/for/") // push gerrit direct
}

/// Scanne un event journalisé à la recherche de références de review
/// dans les sorties outils et les enregistre au nom de la session.
/// `titles` mémorise call_id → commande (un ToolCallUpdate n'a que l'id).
///
/// Une référence NOUVELLE n'entre au registre qu'après vérification
/// d'existence sur la forge — `verifier.submit` la met en file (la
/// confirm émise par le worker à verdict « existe ») ; une référence
/// déjà connue est réattribuée dans le fil synchrone comme avant.
fn scan_pr_event(
    verifier: &PrVerifier,
    forges: &Forges,
    s: &Arc<Session>,
    sid: &str,
    ev: &proto::Event,
    titles: &mut HashMap<String, String>,
) {
    let (excerpt, strong) = match &ev.kind {
        EventKind::ToolCall {
            call_id,
            title,
            excerpt,
            ..
        } => {
            titles.insert(call_id.clone(), title.clone());
            (excerpt.as_deref(), pr_cmd_title(title))
        }
        EventKind::ToolCallUpdate {
            call_id, excerpt, ..
        } => (
            excerpt.as_deref(),
            titles.get(call_id).is_some_and(|t| pr_cmd_title(t)),
        ),
        _ => return,
    };
    let Some(text) = excerpt else { return };
    for r in forges.extract_refs(text) {
        let disp = r.display();
        match verifier.submit(s, sid, r, strong) {
            Ok(Submit::Applied(true)) => s.emit(EventKind::System {
                text: format!("PR détectée et enregistrée : {disp}"),
            }),
            Ok(_) => {}
            Err(e) => warn!("[{sid}] prs.detected: {e:#}"),
        }
    }
    // Fin de review vue dans une sortie (« ✓ Merged pull request o/r#N »,
    // « Merged merge request », « merged » + URL gerrit) → statut à jour :
    // le badge disparaît côté client/acctl (ils filtrent merged/closed) ;
    // l'entrée reste pour la traçabilité.
    for (r, status) in crate::prs::extract_state_refs(text, forges) {
        let disp = r.display();
        match verifier.prs.state_seen(&r, status) {
            Ok(true) => s.emit(EventKind::System {
                text: format!("PR {disp} → {status}"),
            }),
            Ok(false) => {}
            Err(e) => warn!("[{sid}] prs.state_seen: {e:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Session;
    use proto::ToolStatus;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Registre jetable sur un root temporaire.
    fn reg() -> Arc<PrRegistry> {
        let d = std::env::temp_dir().join(format!(
            "ac-prv-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        Arc::new(PrRegistry::open(&d))
    }

    /// Session minimale sur disque — même gabarit que
    /// session::tests::tmp_session (l'emit du verdict écrit au journal).
    fn sess(tag: &str) -> Arc<Session> {
        let dir = std::env::temp_dir().join(format!(
            "ac-prv-sess-{}-{}-{}",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let meta = proto::SessionMeta {
            id: format!("s-{tag}"),
            agent: "test".into(),
            title: "t".into(),
            cwd: "/tmp".into(),
            created_at: proto::now_iso(),
            native_session_id: None,
            mode: None,
            model: None,
            available_modes: vec![],
            available_commands: vec![],
            available_models: vec![],
            config_options: vec![],
            pending_config: Default::default(),
            state: "idle".into(),
            group: None,
            spawned_by: None,
            origin: None,
            context_used: None,
            context_size: None,
        };
        std::fs::write(
            dir.join("meta.json"),
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();
        Session::open(dir, &CompactSrc {
            global: crate::server::CompactCfg::default(),
            by_agent: Default::default(),
        })
        .unwrap()
    }

    fn gh(repo: &str, pr: u64) -> PrRef {
        PrRef {
            forge: "github",
            host: "github.com".into(),
            repo: repo.into(),
            pr,
            change_id: None,
            url: proto::forge_url("github", "github.com", repo, pr, None),
        }
    }

    /// Event ToolCall portant `excerpt` — l'entrée du scan du watcher.
    fn call_ev(excerpt: &str) -> proto::Event {
        proto::Event {
            seq: 1,
            ts: proto::now_iso(),
            kind: EventKind::ToolCall {
                call_id: "c1".into(),
                title: "gh pr create".into(),
                kind: "exec".into(),
                status: ToolStatus::Done,
                blob: None,
                excerpt: Some(excerpt.into()),
                size: None,
                diff: None,
                locations: None,
            },
        }
    }

    /// Poll borné — le worker est async, attendre fixe serait flaky.
    async fn until(timeout_ms: u64, mut f: impl FnMut() -> bool) -> bool {
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        while std::time::Instant::now() < deadline {
            if f() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        f()
    }

    fn verifier(
        r: &Arc<PrRegistry>,
        check: impl Fn(&forgesync::RefTarget) -> forgesync::Verify + Send + Sync + 'static,
    ) -> Arc<PrVerifier> {
        Arc::new(PrVerifier::with_timing(
            Arc::clone(r),
            check,
            &VERIFY_BACKOFF[..0],
        ))
    }

    /// Pipeline complet : URL réelle dans une sortie → file → vérif
    /// « existe » → entrée registre attribuée à la session.
    #[tokio::test]
    async fn verify_exists_registers() {
        let r = reg();
        let v = verifier(&r, |_| forgesync::Verify::Exists);
        v.spawn_worker();
        let s = sess("exists");
        scan_pr_event(
            &v,
            &Forges::resolve(&[]),
            &s,
            "s-x",
            &call_ev("created https://github.com/acme/widgets/pull/9"),
            &mut HashMap::new(),
        );
        assert!(until(5000, || !r.list().is_empty()).await);
        let e = &r.list()[0];
        assert_eq!((e.repo.as_str(), e.pr), ("acme/widgets", 9));
        assert_eq!(e.session, "s-x");
        assert_eq!(e.via, "detected");
    }

    /// Verdict « inexistant » : aucune entrée + cache négatif — la
    /// ré-occurrence ne repart plus vers la forge (compteur à 1).
    #[tokio::test]
    async fn verify_missing_rejected_and_cached() {
        let r = reg();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&calls);
        let v = verifier(&r, move |_| {
            c.fetch_add(1, Ordering::SeqCst);
            forgesync::Verify::Missing
        });
        v.spawn_worker();
        let s = sess("missing");
        assert!(matches!(
            v.submit(&s, "s-x", gh("corp/tools", 42), false).unwrap(),
            Submit::Queued
        ));
        assert!(until(5000, || calls.load(Ordering::SeqCst) >= 1).await);
        assert!(until(5000, || v.negative.lock().unwrap().len() == 1).await);
        assert!(r.list().is_empty());
        // Ré-occurrence fraîche : rejetée sans nouvel appel forge.
        assert!(matches!(
            v.submit(&s, "s-x", gh("corp/tools", 42), false).unwrap(),
            Submit::Rejected
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Entrée déjà connue → détection synchrone, zéro appel forge.
    #[tokio::test]
    async fn known_ref_skips_check() {
        let r = reg();
        r.detected("s-a", &gh("corp/tools", 42), true).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&calls);
        let v = verifier(&r, move |_| {
            c.fetch_add(1, Ordering::SeqCst);
            forgesync::Verify::Missing
        });
        let s = sess("known");
        let out = v.submit(&s, "s-b", gh("corp/tools", 42), true).unwrap();
        assert!(matches!(out, Submit::Applied(true)));
        assert_eq!(r.list()[0].session, "s-b");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    /// Dédup en file : deux détections de la même référence = un item ;
    /// la détection « forte » prime pour l'attribution.
    #[tokio::test]
    async fn dedup_strong_wins() {
        let r = reg();
        let v = verifier(&r, |_| forgesync::Verify::Exists);
        let s = sess("dedup");
        assert!(matches!(
            v.submit(&s, "s-faible", gh("corp/tools", 7), false).unwrap(),
            Submit::Queued
        ));
        assert!(matches!(
            v.submit(&s, "s-forte", gh("corp/tools", 7), true).unwrap(),
            Submit::Queued
        ));
        let g = v.pending.lock().unwrap();
        assert_eq!(g.len(), 1);
        let p = g.values().next().unwrap();
        assert_eq!(p.sid, "s-forte");
        assert!(p.strong);
    }

    /// Forge injoignable : retentatives en backoff, puis abandon — la
    /// référence n'entre jamais au registre.
    #[tokio::test]
    async fn unreachable_retries_then_drops() {
        static BACKOFF: &[std::time::Duration] = &[
            std::time::Duration::from_millis(5),
            std::time::Duration::from_millis(5),
        ];
        let r = reg();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&calls);
        let v = Arc::new(PrVerifier::with_timing(
            Arc::clone(&r),
            move |_| {
                c.fetch_add(1, Ordering::SeqCst);
                forgesync::Verify::Unreachable
            },
            BACKOFF,
        ));
        v.spawn_worker();
        let s = sess("down");
        v.submit(&s, "s-x", gh("corp/tools", 9), false).unwrap();
        // 1 essai + 2 retries = 3 appels, puis file vidée (abandon).
        assert!(until(5000, || calls.load(Ordering::SeqCst) >= 3).await);
        assert!(until(5000, || v.pending.lock().unwrap().is_empty()).await);
        assert!(r.list().is_empty());
    }

    /// Un placeholder denylisté est écarté par `extract_refs` AVANT la
    /// file : le scan ne produit ni pending ni appel forge.
    #[test]
    fn placeholder_never_queued() {
        let r = reg();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&calls);
        let v = verifier(&r, move |_| {
            c.fetch_add(1, Ordering::SeqCst);
            forgesync::Verify::Exists
        });
        let s = sess("ph");
        scan_pr_event(
            &v,
            &Forges::resolve(&[]),
            &s,
            "s-x",
            &call_ev("doc : https://github.com/owner/repo/pull/9"),
            &mut HashMap::new(),
        );
        assert!(v.pending.lock().unwrap().is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(r.list().is_empty());
    }
}
