//! Auto-refresh de l'explorateur : watch FS non récursif par dossier
//! déplié, cycle de vie borné à la connexion (Drop = tout est démonté,
//! une conn partie ne laisse aucun watch résiduel). Les events FS ne
//! sont JAMAIS journalisés : le debounce pousse `{push:"dir_changed"}`
//! directement dans l'outbox — le canal session (events_tx) resterait
//! sinon pollué de seqs éphémères masquant les vrais events.
//!
//! Anti-rafale (exigence, pas option) : un burst coalesce en UN push
//! ~600 ms après le dernier event, jamais plus d'un push/dossier/2 s —
//! et sous rafale continue on émet quand même à ~2 s (le client n'est
//! jamais affamé). Filtre de bruit : caches (.git, node_modules), swaps
//! éditeur, locks/temporaires ; les events accès/métadonnées que
//! list_dir ne montre pas (ni nom, ni type, ni taille) sont ignorés.

use super::*;

use notify::event::ModifyKind;
use notify::{EventKind, RecursiveMode, Watcher};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;

/// Fenêtre de calme déclenchant le push après un burst (~500-800 ms).
const DEBOUNCE: Duration = Duration::from_millis(600);
/// Cap dur : jamais plus d'un push par dossier sur 2 s ; sous rafale
/// continue c'est aussi l'échéance forcée (un push par période).
const CAP: Duration = Duration::from_secs(2);
/// Plafond global de dossiers surveillés, toutes conns confondues —
/// borne les watch descriptors inotify consommés par N sessions dont
/// l'arbre déplie plusieurs dossiers à la fois.
const MAX_DIR_WATCHES: usize = 16;
/// Une fenêtre de cap d'un dossier désabonné n'est conservée que ce
/// délai (re-watch dans la foulée = throttle conservé, sans fuite).
const STALE_EMIT: Duration = Duration::from_secs(30);

/// Un dossier suivi par la connexion : ce qui est renvoyé dans le push
/// `dir_changed` (la clé de la map est le chemin absolu canonique).
struct Sub {
    session: String,
    /// Chemin relatif tel que le client l'a demandé — re-clé d'unwatch.
    rel: String,
}

/// Fenêtre de debounce ouverte : `first` = premier event depuis la
/// dernière émission (ancre du forcage anti-rafale), `latest` = plus
/// récent (ancre du calme).
#[derive(Clone, Copy)]
struct Pend {
    first: Instant,
    latest: Instant,
}

/// Instant d'émission d'une fenêtre : le plus tôt respectant calme
/// (latest+debounce) ET cadence (floor+cap), ramené à first+cap si la
/// rafale ne laisse jamais 600 ms de trou — min() = l'échéance forcée.
fn due_at(p: &Pend, floor: Option<Instant>, now: Instant) -> Instant {
    let floor = floor.unwrap_or_else(|| now.checked_sub(CAP).unwrap_or(now));
    (p.latest + DEBOUNCE).max(floor + CAP).min(p.first + CAP)
}

/// Bruit : entrées que list_dir montre mais dont les changements ne
/// doivent pas réveiller l'arbre.
fn noisy_name(name: &str) -> bool {
    if name == ".git" || name == "node_modules" {
        return true;
    }
    // `*~` backups, `.#foo` locks Emacs…
    if name.ends_with('~') || name.starts_with(".#") {
        return true;
    }
    // `.foo.part-123-4` : temporaires de nos propres écritures atomiques
    // (splice_write) — le rename final produit l'event qui compte.
    if name.starts_with('.') && name.contains(".part-") {
        return true;
    }
    if name.contains('.') {
        return matches!(
            name.rsplit('.').next().unwrap_or_default(),
            "swp" | "swo" | "swx" | "tmp" | "part"
        );
    }
    false
}

/// L'event peut-il changer le rendu de list_dir ? Les accès purs et les
/// modifications de métadonnées seules (chmod, utime…) sont invisibles
/// dans le listing (nom/type/taille) → pas de push pour eux.
fn visible_kind(k: &EventKind) -> bool {
    !matches!(k, EventKind::Access(_))
        && !matches!(k, EventKind::Modify(ModifyKind::Metadata(_)))
}

/// Marque les dossiers suivis touchés par l'event : leur parent direct
/// (watch non récursif), ou eux-mêmes (suppression/déplacement du
/// dossier suivi — le client réconciliera vers la disparition).
fn mark_pending(
    ev: &notify::Event,
    subs: &std::sync::Mutex<HashMap<PathBuf, Sub>>,
    pend: &mut HashMap<PathBuf, Pend>,
    now: Instant,
) {
    if !visible_kind(&ev.kind) {
        return;
    }
    let subs = subs.lock().unwrap();
    for p in &ev.paths {
        let name = p.file_name().unwrap_or_default().to_string_lossy();
        if noisy_name(&name) {
            continue;
        }
        let dir = if subs.contains_key(p.as_path()) {
            Some(p.as_path())
        } else {
            p.parent().filter(|d| subs.contains_key(*d))
        };
        let Some(dir) = dir else { continue };
        pend.entry(dir.to_path_buf())
            .and_modify(|e| e.latest = now)
            .or_insert(Pend {
                first: now,
                latest: now,
            });
    }
}

/// Tâche de debounce d'une connexion : coalesce les events par dossier
/// puis pousse `dir_changed`. Sort quand le canal FS se ferme (watcher
/// droppé) ou quand l'outbox est mort (conn perdue).
async fn debounce_run(
    mut rx: mpsc::UnboundedReceiver<notify::Result<notify::Event>>,
    subs: Arc<std::sync::Mutex<HashMap<PathBuf, Sub>>>,
    outbox: Outbox,
) {
    let mut pend: HashMap<PathBuf, Pend> = HashMap::new();
    // Dernier emit par dossier — persiste entre fenêtres (le cap ne
    // doit pas être contourné par un écart d'activité).
    let mut last_emit: HashMap<PathBuf, Instant> = HashMap::new();
    loop {
        let now = Instant::now();
        let mut next: Option<Instant> = None;
        for (d, p) in &pend {
            let due = due_at(p, last_emit.get(d).copied(), now);
            next = Some(next.map_or(due, |n| n.min(due)));
        }
        tokio::select! {
            ev = rx.recv() => {
                match ev {
                    Some(Ok(e)) => mark_pending(&e, &subs, &mut pend, now),
                    Some(Err(e)) => debug!("watch: {e}"),
                    None => break,
                }
            }
            _ = async {
                match next {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                let now = Instant::now();
                let due: Vec<PathBuf> = pend
                    .iter()
                    .filter(|(d, p)| due_at(p, last_emit.get(*d).copied(), now) <= now)
                    .map(|(d, _)| d.clone())
                    .collect();
                for d in due {
                    pend.remove(&d);
                    let sub = subs
                        .lock()
                        .unwrap()
                        .get(&d)
                        .map(|s| (s.session.clone(), s.rel.clone()));
                    // Dossier désabonné entre-temps : la fenêtre de cap
                    // reste enregistrée mais rien n'est poussé.
                    let Some((session, rel)) = sub else { continue };
                    last_emit.insert(d, now);
                    let frame = json!({
                        "push": "dir_changed", "session": session, "path": rel,
                    });
                    if Daemon::send_line(&outbox, &frame).is_err() {
                        return; // conn morte — la tâche n'a plus lieu d'être
                    }
                }
                // GC : fenêtres de cap des dossiers plus suivis (>30 s).
                let subs_g = subs.lock().unwrap();
                last_emit
                    .retain(|d, t| subs_g.contains_key(d) || t.elapsed() < STALE_EMIT);
            }
        }
    }
}

/// Watches FS d'une connexion : UN watcher inotify pour tous les
/// dossiers dépliés (un seul fd), events bruts → tâche debounce.
/// `Drop` démonte tout : watcher, tâche, comptage global.
pub(crate) struct DirWatches {
    /// Créés paresseusement à la première demande — une conn sans
    /// explorateur ne coûte ni fd ni tâche.
    watcher: Option<notify::RecommendedWatcher>,
    task: Option<tokio::task::JoinHandle<()>>,
    subs: Arc<std::sync::Mutex<HashMap<PathBuf, Sub>>>,
    /// Compteur global démon des dossiers surveillés (borne MAX_*).
    count: Arc<std::sync::atomic::AtomicUsize>,
}

impl DirWatches {
    pub fn new(count: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        Self {
            watcher: None,
            task: None,
            subs: Arc::new(std::sync::Mutex::new(HashMap::new())),
            count,
        }
    }

    /// Abonne `abs` (dossier canonique déjà borné au workspace) pour
    /// `session`/`rel`. Idempotent : déjà suivi → re-clé seulement.
    /// `false` = plafond global atteint (le client garde le refresh
    /// manuel, silencieusement).
    pub fn watch(
        &mut self,
        abs: &std::path::Path,
        session: String,
        rel: String,
        outbox: &Outbox,
    ) -> Result<bool> {
        if let Some(s) = self.subs.lock().unwrap().get_mut(abs) {
            s.session = session;
            s.rel = rel;
            return Ok(true);
        }
        // Réservation atomique sous le plafond : deux conns ne peuvent
        // pas dépasser MAX en se contrôlant l'une l'autre.
        if self
            .count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            >= MAX_DIR_WATCHES
        {
            self.count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            return Ok(false);
        }
        let res = (|| -> Result<()> {
            if self.watcher.is_none() {
                let (tx, rx) = mpsc::unbounded_channel();
                self.watcher = Some(notify::recommended_watcher(
                    move |res: notify::Result<notify::Event>| {
                        let _ = tx.send(res);
                    },
                )?);
                self.task = Some(tokio::spawn(debounce_run(
                    rx,
                    Arc::clone(&self.subs),
                    outbox.clone(),
                )));
            }
            self.watcher
                .as_mut()
                .unwrap()
                .watch(abs, RecursiveMode::NonRecursive)?;
            Ok(())
        })();
        if let Err(e) = res {
            self.count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            return Err(e);
        }
        self.subs.lock().unwrap().insert(
            abs.to_path_buf(),
            Sub { session, rel },
        );
        Ok(true)
    }

    /// Retire les watches de `session` — `rel` absent = tous (panneau
    /// fermé, session quittée). Renvoie le nombre retiré.
    pub fn unwatch(&mut self, session: &str, rel: Option<&str>) -> usize {
        let gone: Vec<PathBuf> = self
            .subs
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, s)| {
                s.session == session && rel.is_none_or(|r| s.rel == r)
            })
            .map(|(d, _)| d.clone())
            .collect();
        let n = gone.len();
        if n == 0 {
            return 0;
        }
        let mut subs = self.subs.lock().unwrap();
        for d in gone {
            subs.remove(&d);
            if let Some(w) = self.watcher.as_mut() {
                let _ = w.unwatch(&d); // déjà parti = sans conséquence
            }
        }
        drop(subs);
        self.count
            .fetch_sub(n, std::sync::atomic::Ordering::Relaxed);
        n
    }
}

impl Drop for DirWatches {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
        // watcher droppé → watches inotify relâchés d'un coup.
        self.watcher = None;
        self.count.fetch_sub(
            self.subs.lock().unwrap().len(),
            std::sync::atomic::Ordering::Relaxed,
        );
    }
}

impl Daemon {
    /// watch_dir / unwatch_dir — couche connexion, comme le terminal :
    /// le watch est un état de CETTE conn (outbox pour les pushes, mort
    /// avec elle) — jamais dans le dispatch concurrent ni sur /rpc.
    pub(super) async fn h_dir_watch(
        self: &Arc<Self>,
        watches: &mut DirWatches,
        req: &Request,
        outbox: &Outbox,
    ) -> Option<Response> {
        match &req.req {
            ReqKind::WatchDir { session, path } => {
                let resp = match self
                    .get_session(session)
                    .await
                    .and_then(|s| files::resolve_in_cwd(&s, path))
                {
                    Ok(abs) if !abs.is_dir() => {
                        Response::err(req.id, format!("pas un dossier: {path}"))
                    }
                    Ok(abs) => match watches.watch(
                        &abs,
                        session.clone(),
                        path.clone(),
                        outbox,
                    ) {
                        Ok(true) => Response::ok(
                            req.id,
                            json!({"watching": true,
                                   "dir": abs.display().to_string()}),
                        ),
                        Ok(false) => {
                            Response::ok(req.id, json!({"watching": false}))
                        }
                        Err(e) => Response::err(req.id, format!("{e:#}")),
                    },
                    Err(e) => Response::err(req.id, format!("{e:#}")),
                };
                Some(resp)
            }
            ReqKind::UnwatchDir { session, path } => {
                let n = watches.unwatch(session, path.as_deref());
                Some(Response::ok(req.id, json!({"unwatched": n})))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Échéance du debounce hybride : calme 600 ms, jamais >1 emit/2 s,
    /// force sous rafale à 2 s depuis le premier event du burst.
    #[test]
    fn due_windows() {
        let t0 = Instant::now();
        // Event isolé, jamais émis : emit à +600 ms (debounce seul).
        let p = Pend { first: t0, latest: t0 };
        assert_eq!(due_at(&p, None, t0), t0 + DEBOUNCE);
        // Event 1 s après un emit : le cap repousse à +2 s pile.
        let floor = t0 - Duration::from_secs(1);
        assert_eq!(due_at(&p, Some(floor), t0), floor + CAP);
        // Rafale continue (latest recule toujours) : émission forcée à
        // first+2 s — la cadence est tenue même sans calme.
        let q = Pend {
            first: t0,
            latest: t0 + Duration::from_secs(3),
        };
        assert_eq!(due_at(&q, Some(t0 - CAP), t0), t0 + CAP);
    }

    #[test]
    fn noise() {
        for n in [".git", "node_modules", ".#a", "b~", ".f.swp", "x.tmp", ".a.part-1-2"] {
            assert!(noisy_name(n), "{n}");
        }
        for n in ["a.rs", ".gitignore", "Cargo.lock", "tmp", "README"] {
            assert!(!noisy_name(n), "{n}");
        }
    }

    /// Outbox factice : la voie pushes (`lo`) est lisible par le test
    /// pour constater les frames `dir_changed` émises par le debounce.
    fn test_outbox() -> (Outbox, mpsc::UnboundedReceiver<String>) {
        let (hi, _hi_rx) = mpsc::unbounded_channel();
        let (lo, lo_rx) = mpsc::unbounded_channel();
        let (dead, _dead_rx) = watch::channel(false);
        (
            Outbox {
                hi,
                lo,
                dead,
                queued: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            },
            lo_rx,
        )
    }

    /// E2E inotify réel : un burst de 100 fichiers coalesce en UNE
    /// frame `dir_changed` ; unwatch arrête les pushes et rend le
    /// compteur global ; drop démonte tout (conn partie = zéro watch).
    #[tokio::test]
    async fn watch_coalesce_unwatch() {
        let base = std::env::temp_dir().join(format!("ac-dirwatch-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let abs = base.canonicalize().unwrap();
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (ob, mut lo) = test_outbox();
        {
            let mut dw = DirWatches::new(Arc::clone(&count));
            assert!(dw.watch(&abs, "s1".into(), "".into(), &ob).unwrap());
            assert_eq!(count.load(std::sync::atomic::Ordering::Relaxed), 1);

            // Rafale : 100 fichiers — le debounce doit tout fondre.
            for i in 0..100 {
                std::fs::write(base.join(format!("f{i}")), b"x").unwrap();
            }
            let line = tokio::time::timeout(Duration::from_secs(5), lo.recv())
                .await
                .expect("aucun push dir_changed en 5 s")
                .unwrap();
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(v["push"], "dir_changed");
            assert_eq!(v["session"], "s1");
            assert_eq!(v["path"], "");
            // Coalescé : pas de seconde frame dans la foulée (un push
            // par fichier verrait la file se remplir immédiatement ; un
            // second push licite post-cap arriverait ≥2 s plus tard).
            assert!(
                tokio::time::timeout(Duration::from_millis(900), lo.recv())
                    .await
                    .is_err(),
                "le burst a produit plus d'un push"
            );

            // Désabonnement : compteur rendu, events suivants ignorés.
            assert_eq!(dw.unwatch("s1", Some("")), 1);
            assert_eq!(count.load(std::sync::atomic::Ordering::Relaxed), 0);
            std::fs::write(base.join("apres"), b"x").unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(1200), lo.recv())
                    .await
                    .is_err(),
                "un push est arrivé après unwatch"
            );
        }
        // Drop (conn morte) : aucun état résiduel côté compteur.
        assert_eq!(count.load(std::sync::atomic::Ordering::Relaxed), 0);
        std::fs::remove_dir_all(&base).ok();
    }
}
