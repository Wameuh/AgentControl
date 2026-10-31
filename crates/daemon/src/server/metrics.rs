//! Journal de métriques backlog — série temporelle + agrégats.
//!
//! L'échantillonneur est adossé au tick de l'orchestrateur natif
//! (server/orch.rs) : en fin de cycle réussi, un sample est ajouté au
//! journal `~/.agentbacklog/metrics.jsonl`. Orchestrateur désactivé,
//! pausé ou bloqué → pas de samples : la série vit avec l'orch (pas de
//! collecteur dédié — simplicité d'abord ; flag `[backlog_orchestrator]
//! metrics = false` pour couper l'échantillonnage sans couper la RPC).
//!
//! Journal : JSONL append-only, une ligne par sample —
//!   {"ts":<epoch_s>, "pending":n, "poppable":n, "wip":n,
//!    "workers_alive":n, "by_repo":{"<repo>":{"pending":n,"wip":n}}}
//! Rétention bornée : 30 jours ET ~100k samples (le premier seuil
//! atteint tronque, en gardant les plus récents) ; la compaction
//! réécrit le fichier (atomique `.part` + rename), déclenchée
//! périodiquement (tous les `COMPACT_EVERY` appends — borne d'âge) ou
//! sous pression taille (> `COMPACT_BYTES`).
//!
//! `backlog_metrics{window}` : lit la série bornée à la fenêtre
//! (lignes corrompues ignorées + comptées, fichier absent → série
//! vide — jamais d'erreur sur le journal) et calcule les agrégats à la
//! volée sur les timestamps des items AgentBacklog
//! (`--list --all` : created/started/done → lead time, attente file,
//! items terminés/jour) + le pic de workers de la série. Un
//! agentbacklog indisponible ne prive pas la réponse de la série
//! (`items_available:false`, champs items → null).

use super::orch::{
    now_secs, state_dir, task_poppable, task_status, OrchProbe, Task, COMPONENT_ORIGIN,
};
use super::*;
use proto::SessionMeta;
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::atomic::Ordering;

/// Âge max d'un sample conservé (30 j).
const MAX_AGE_S: f64 = 30.0 * 86_400.0;
/// Cardinalité max du journal — la troncation garde la queue récente.
const MAX_SAMPLES: usize = 100_000;
/// Compaction déclenchée dès que le fichier dépasse cette taille.
const COMPACT_BYTES: u64 = 4 << 20;
/// …ou périodiquement tous les N appends (~1 j à la cadence 60 s) —
/// la borne d'âge s'applique même sur un fichier resté petit.
const COMPACT_EVERY: u64 = 1440;
/// Plancher de la fenêtre effective pour « terminés/jour » — évite
/// d'extrapoler une rafale instantanée sur la fenêtre « all ».
const MIN_SPAN_DAYS: f64 = 1.0 / 24.0;

/// `~/.agentbacklog/metrics.jsonl` — à côté de l'état orchestrateur
/// (même périmètre « file backlog » : DB, lock, pause, state.json).
pub(crate) fn metrics_path() -> PathBuf {
    state_dir().join("metrics.jsonl")
}

/// Sample d'un tick : compteurs instantanés de la file.
/// `pending` = tous les items en attente (dep-bloqués inclus) ;
/// `poppable` = le sous-ensemble lançable (dep soldée) ;
/// `workers_alive` = sessions `origin` orchestrateur non terminales
/// (asleep/idle compris — nudgeable n'est pas mort).
fn build_sample(tasks: &[Task], metas: &[SessionMeta]) -> Value {
    let pending = tasks.iter().filter(|t| task_status(t) == "pending").count();
    let poppable = tasks
        .iter()
        .filter(|t| task_status(t) == "pending" && task_poppable(t))
        .count();
    let wip = tasks.iter().filter(|t| task_status(t) == "wip").count();
    let workers_alive = metas
        .iter()
        .filter(|m| {
            m.origin.as_deref() == Some(COMPONENT_ORIGIN)
                && m.state != "dead"
                && m.state != "interrupted"
        })
        .count();
    // Répartition par workspace (tag `repo` des items) — optionnelle,
    // les items non taggés restent dans les totaux seuls.
    let mut by_repo: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for t in tasks {
        let repo = t.get("repo").and_then(Value::as_str).unwrap_or("");
        if repo.is_empty() {
            continue;
        }
        let e = by_repo.entry(repo.to_string()).or_default();
        match task_status(t) {
            "pending" => e.0 += 1,
            "wip" => e.1 += 1,
            _ => {}
        }
    }
    json!({
        "ts": now_secs(),
        "pending": pending,
        "poppable": poppable,
        "wip": wip,
        "workers_alive": workers_alive,
        "by_repo": by_repo
            .into_iter()
            .map(|(k, (p, w))| (k, json!({ "pending": p, "wip": w })))
            .collect::<serde_json::Map<String, Value>>(),
    })
}

impl OrchProbe {
    /// Append du sample du tick + compaction opportuniste — appelé en
    /// fin de cycle réussi (la série reflète des cycles aboutis : un
    /// tick coupé à mi-course ne produit pas de point). Best-effort
    /// total : aucune erreur n'est propagée — le journal métriques ne
    /// doit jamais pouvoir casser la boucle d'orchestration.
    ///
    /// Le snapshot est celui capturé en DÉBUT de cycle (items + métas
    /// figés au lancement du tick) : un worker spawné ou un item poppé
    /// en cours de tick n'apparaît qu'au point suivant — voulu, la
    /// série décrit ce sur quoi l'orch a statué.
    ///
    /// Concurrence : appelé depuis la tâche `orch_loop` seule →
    /// l'append est séquentiel et la compaction (read→rewrite→rename)
    /// ne peut pas perdre un append concurrent.
    pub(crate) fn metrics_sample(&self, tasks: &[Task], metas: &[SessionMeta]) {
        let path = metrics_path();
        let line = match serde_json::to_string(&build_sample(tasks, metas)) {
            Ok(l) => l,
            Err(e) => {
                warn!("métriques: sérialisation sample: {e}");
                return;
            }
        };
        let dir = path.parent().map(PathBuf::from).unwrap_or_default();
        let appended = std::fs::create_dir_all(&dir).and_then(|_| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .and_then(|mut f| writeln!(f, "{line}"))
        });
        if let Err(e) = appended {
            warn!("métriques: append {}: {e}", path.display());
            return;
        }
        // Compaction : périodique (borne d'âge) ou pression taille.
        let n = self.metrics_appends.fetch_add(1, Ordering::Relaxed) + 1;
        let oversized = std::fs::metadata(&path)
            .map(|m| m.len() > COMPACT_BYTES)
            .unwrap_or(false);
        if n < COMPACT_EVERY && !oversized {
            return;
        }
        self.metrics_appends.store(0, Ordering::Relaxed);
        match compact(&path) {
            Ok((kept, dropped)) if dropped > 0 => {
                info!("métriques: compaction — {kept} gardés, {dropped} tronqués/corrompus");
            }
            Ok(_) => {}
            Err(e) => warn!("métriques: compaction {}: {e}", path.display()),
        }
    }
}

/// Réécriture du journal : borne d'âge (> MAX_AGE_S) puis cardinalité
/// (> MAX_SAMPLES — garde la queue récente). Les lignes invalides ou
/// sans `ts` comptent « dropped ». `.part` + rename — même pattern
/// qu'`orchestrator-state.json` (pas de troncation visible).
/// → (kept, dropped) ; fichier absent/illisible → Ok((0, 0)).
fn compact(path: &std::path::Path) -> std::io::Result<(usize, usize)> {
    let Ok(txt) = std::fs::read_to_string(path) else {
        return Ok((0, 0));
    };
    let cutoff = now_secs() - MAX_AGE_S;
    let mut kept: Vec<&str> = Vec::with_capacity(txt.lines().count());
    let mut dropped = 0usize;
    for line in txt.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let fresh = serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|v| v.get("ts").and_then(Value::as_f64))
            .is_some_and(|ts| ts >= cutoff);
        if fresh {
            kept.push(line);
        } else {
            dropped += 1;
        }
    }
    if kept.len() > MAX_SAMPLES {
        dropped += kept.len() - MAX_SAMPLES;
        kept.drain(..kept.len() - MAX_SAMPLES);
    }
    let tmp = path.with_extension("jsonl.part");
    let mut out = String::with_capacity(txt.len());
    for l in &kept {
        out.push_str(l);
        out.push('\n');
    }
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, path)?;
    Ok((kept.len(), dropped))
}

/// `window` RPC → (durée de fenêtre en s, label normalisé).
/// `None` en durée = fenêtre « all » non bornée ; absent/vide → "7d".
fn window_secs(w: Option<&str>) -> Result<(Option<f64>, String)> {
    let w = w.unwrap_or("7d").trim().to_ascii_lowercase();
    let w = if w.is_empty() { "7d".to_string() } else { w };
    if w == "all" {
        return Ok((None, w));
    }
    // Découpe par char, pas par octet : `split_at(len - 1)` paniquerait
    // sur une unité multi-octets (« café » → frontière au milieu de « é »)
    // — l'input vient du client, un parseur ne doit jamais paniquer.
    let unit = w.chars().last().unwrap_or(' ');
    let num = &w[..w.len().saturating_sub(unit.len_utf8())];
    let n: f64 = num
        .parse()
        .with_context(|| format!("fenêtre « {w} » invalide (ex. 24h, 7d, 30d, all)"))?;
    let secs = match unit {
        'h' => n * 3_600.0,
        'd' => n * 86_400.0,
        _ => anyhow::bail!("fenêtre « {w} » invalide (ex. 24h, 7d, 30d, all)"),
    };
    anyhow::ensure!(
        secs > 0.0,
        "fenêtre « {w} » invalide (ex. 24h, 7d, 30d, all)"
    );
    Ok((Some(secs), w))
}

/// Série du journal bornée à `since` (None = toute la série) —
/// (samples, lignes corrompues ignorées). Fichier absent → vide.
fn read_samples(since: Option<f64>) -> (Vec<Value>, u64) {
    let Ok(txt) = std::fs::read_to_string(metrics_path()) else {
        return (Vec::new(), 0);
    };
    let mut out = Vec::new();
    let mut corrupt = 0u64;
    for line in txt.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(v) => match v.get("ts").and_then(Value::as_f64) {
                // Pas de `ts` exploitable → inutilisable : comptée
                // corrompue, symétrique de `compact` qui la droppe.
                None => corrupt += 1,
                Some(ts) if since.is_none_or(|s| ts >= s) => out.push(v),
                Some(_) => {}
            },
            Err(_) => corrupt += 1,
        }
    }
    (out, corrupt)
}

/// ISO-8601 (AgentBacklog : "…T…Z" en millis) → epoch secondes.
fn iso_epoch(s: &str) -> Option<f64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp_millis() as f64 / 1_000.0)
}

fn mean(v: &[f64]) -> Option<f64> {
    (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
}

fn median(v: &mut [f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    })
}

/// Agrégats calculés à la volée :
/// - lead time / attente file : timestamps des items (l'événement
///   — done / started — borne l'appartenance à la fenêtre, pas
///   l'item) ; les durées s'expriment en secondes ;
/// - items terminés/jour : fenêtre bornée → count/fenêtre ; « all » →
///   étendue réelle [premier done, maintenant], plancher 1 h pour ne
///   pas extrapoler une rafale instantanée ;
/// - pic de workers : max `workers_alive` de la série.
fn aggregates(
    items: Option<&[Task]>,
    samples: &[Value],
    since: Option<f64>,
    span: Option<f64>,
    now: f64,
) -> Value {
    let mut lead: Vec<f64> = Vec::new();
    let mut wait: Vec<f64> = Vec::new();
    let mut done_ts: Vec<f64> = Vec::new();
    let mut started_n = 0usize;
    if let Some(items) = items {
        for t in items {
            let created = t
                .get("created_at")
                .and_then(Value::as_str)
                .and_then(iso_epoch);
            let started = t
                .get("started_at")
                .and_then(Value::as_str)
                .and_then(iso_epoch);
            let done = t.get("done_at").and_then(Value::as_str).and_then(iso_epoch);
            // L'événement (done/started) compte sur son propre timestamp
            // dans la fenêtre ; `created_at` ne conditionne que la durée
            // — un item sans created_at compte toujours au throughput.
            if let Some(d) = done {
                if since.is_none_or(|s| d >= s) {
                    done_ts.push(d);
                    if let Some(c) = created {
                        lead.push(d - c);
                    }
                }
            }
            if let Some(s0) = started {
                if since.is_none_or(|s| s0 >= s) {
                    started_n += 1;
                    if let Some(c) = created {
                        wait.push(s0 - c);
                    }
                }
            }
        }
    }
    let span_days = match span {
        Some(s) => (s / 86_400.0).max(MIN_SPAN_DAYS),
        None => done_ts
            .iter()
            .copied()
            .reduce(f64::min)
            .map(|first| ((now - first) / 86_400.0).max(MIN_SPAN_DAYS))
            .unwrap_or(0.0),
    };
    let done_per_day = if span_days > 0.0 {
        done_ts.len() as f64 / span_days
    } else {
        0.0
    };
    let workers_peak = samples
        .iter()
        .filter_map(|s| s.get("workers_alive").and_then(Value::as_u64))
        .max();
    json!({
        "items_available": items.is_some(),
        "items_total": items.map_or(0, |i| i.len()),
        "items_done": done_ts.len(),
        "items_started": started_n,
        "done_per_day": done_per_day,
        "lead_time_avg_s": mean(&lead),
        "lead_time_median_s": median(&mut lead),
        "queue_wait_avg_s": mean(&wait),
        "queue_wait_median_s": median(&mut wait),
        "workers_peak": workers_peak,
    })
}

impl Daemon {
    /// `BacklogMetrics{window}` — série du journal + agrégats à la
    /// volée. Le journal absent/corrompu renvoie une série vide
    /// proprement ; un agentbacklog indisponible ne prive pas la
    /// réponse de la série (agrégats items → null).
    pub(super) async fn h_backlog_metrics(
        self: &Arc<Self>,
        window: Option<String>,
    ) -> Result<Value> {
        let (span, label) = window_secs(window.as_deref())?;
        let now = now_secs();
        let since = span.map(|s| now - s);
        let (samples, corrupt) = read_samples(since);
        // Items = source des agrégats lead-time/attente — best-effort :
        // binaire non résolvable ou --list en échec → agrégats null.
        let items = match self.orch_agentbacklog_bin().await {
            Some(bin) => {
                let ws = PathBuf::from(&self.orch.cfg.read().unwrap().workspace);
                self.orch_tasks(&bin, &ws).await
            }
            None => None,
        };
        Ok(json!({
            "window": label,
            "since": since,
            "generated_at": proto::now_iso(),
            "journal": metrics_path().display().to_string(),
            "sample_count": samples.len(),
            "corrupt_lines": corrupt,
            "samples": samples,
            "aggregates": aggregates(items.as_deref(), &samples, since, span, now),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(origin: Option<&str>, state: &str) -> SessionMeta {
        SessionMeta {
            id: "s-x".into(),
            agent: "devin".into(),
            title: "Backlog-W1".into(),
            cwd: "/workspace".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            native_session_id: None,
            mode: None,
            model: None,
            available_modes: Vec::new(),
            available_commands: Vec::new(),
            available_models: Vec::new(),
            config_options: Vec::new(),
            pending_config: Default::default(),
            state: state.into(),
            group: None,
            spawned_by: None,
            origin: origin.map(str::to_string),
            context_used: None,
            context_size: None,
        }
    }

    fn task(status: &str, repo: Option<&str>, dep_status: Option<&str>) -> Value {
        json!({
            "id": 1, "status": status, "repo": repo,
            "depends_on": dep_status.map(|_| 1),
            "dep_status": dep_status,
        })
    }

    #[test]
    fn window_parse() {
        assert_eq!(window_secs(None).unwrap().0, Some(7.0 * 86_400.0));
        assert_eq!(window_secs(Some("")).unwrap().0, Some(7.0 * 86_400.0));
        assert_eq!(window_secs(Some("24h")).unwrap().0, Some(86_400.0));
        assert_eq!(window_secs(Some("30d")).unwrap().0, Some(30.0 * 86_400.0));
        assert_eq!(window_secs(Some("all")).unwrap().0, None);
        assert_eq!(window_secs(Some(" ALL ")).unwrap().0, None);
        assert!(window_secs(Some("7w")).is_err());
        assert!(window_secs(Some("xd")).is_err());
        assert!(window_secs(Some("h")).is_err());
        assert!(window_secs(Some("0d")).is_err());
        assert!(window_secs(Some("-5d")).is_err());
        // Multi-octets : `split_at(len - 1)` paniquait sur la
        // frontière interne de « é » — la découpe par char dégrade
        // proprement en erreur, jamais en panic.
        assert!(window_secs(Some("café")).is_err());
        assert!(window_secs(Some("7é")).is_err());
        assert!(window_secs(Some("€")).is_err());
    }

    #[test]
    fn sample_counts() {
        let tasks = vec![
            task("pending", Some("AgentControl"), None),
            task("pending", Some("AgentControl"), Some("pending")), // dep-bloqué
            task("wip", Some("AgentControl"), None),
            task("done", Some("AgentControl"), None),
            task("pending", None, None), // non taggé : totaux seuls
        ];
        let metas = vec![
            meta(Some(COMPONENT_ORIGIN), "working"),
            meta(Some(COMPONENT_ORIGIN), "asleep"),
            meta(Some(COMPONENT_ORIGIN), "dead"),
            meta(Some("feature:backlog-manager"), "idle"),
            meta(None, "working"),
        ];
        let s = build_sample(&tasks, &metas);
        assert_eq!(s["pending"], 3);
        assert_eq!(s["poppable"], 2); // le dep-bloqué ne compte pas
        assert_eq!(s["wip"], 1);
        assert_eq!(s["workers_alive"], 2); // dead exclu, non-orch exclus
        assert_eq!(s["by_repo"]["AgentControl"]["pending"], 2);
        assert_eq!(s["by_repo"]["AgentControl"]["wip"], 1);
        assert!(s["ts"].as_f64().unwrap() > 0.0);
    }

    #[test]
    fn aggregate_math() {
        let now = iso_epoch("2026-10-31T00:00:00Z").unwrap();
        // Item 1 : créé il y a 2 j, attente 1 h, lead 1 j → done hier.
        // Item 2 : créé il y a 10 j, lead 10 j → done hier ; démarré le
        // 10-22 (hors fenêtre 7d qui commence le 10-24 → attente ignorée).
        // Item 3 : wip démarré hier (attente 3 h), pas de done.
        // Item 4 : done il y a 60 j — hors fenêtre 7d, dans « all ».
        let items = vec![
            json!({"status":"done","created_at":"2026-10-29T00:00:00Z",
                   "started_at":"2026-10-29T01:00:00Z","done_at":"2026-10-30T00:00:00Z"}),
            json!({"status":"done","created_at":"2026-10-20T00:00:00Z",
                   "started_at":"2026-10-22T00:00:00Z","done_at":"2026-10-30T00:00:00Z"}),
            json!({"status":"wip","created_at":"2026-10-30T18:00:00Z",
                   "started_at":"2026-10-30T21:00:00Z","done_at":null}),
            json!({"status":"done","created_at":"2026-08-01T00:00:00Z",
                   "started_at":"2026-08-02T00:00:00Z","done_at":"2026-09-01T00:00:00Z"}),
        ];
        let samples = vec![
            json!({"ts": now - 100.0, "workers_alive": 1}),
            json!({"ts": now - 50.0, "workers_alive": 4}),
        ];
        let since_7d = now - 7.0 * 86_400.0;
        let v = aggregates(
            Some(&items),
            &samples,
            Some(since_7d),
            Some(7.0 * 86_400.0),
            now,
        );
        assert_eq!(v["items_done"], 2);
        // Item 2 a démarré le 10-22 — avant la fenêtre (10-24) : hors
        // comptage attente. Items 1 et 3 → attentes 1 h et 3 h.
        assert_eq!(v["items_started"], 2);
        // lead moyen = (1 j + 10 j) / 2 = 5.5 j
        assert_eq!(v["lead_time_avg_s"], 5.5 * 86_400.0);
        assert_eq!(v["lead_time_median_s"], 5.5 * 86_400.0);
        // attente : (1 h + 3 h) / 2 = 2 h
        assert_eq!(v["queue_wait_avg_s"], 2.0 * 3_600.0);
        assert_eq!(v["queue_wait_median_s"], 2.0 * 3_600.0);
        assert_eq!(v["done_per_day"], 2.0 / 7.0);
        assert_eq!(v["workers_peak"], 4);
        // Fenêtre « all » : l'item de septembre rentre (done 60 j) —
        // span réel ~60 j → done_per_day = 3 / 60.
        let v_all = aggregates(Some(&items), &samples, None, None, now);
        assert_eq!(v_all["items_done"], 3);
        let dpd = v_all["done_per_day"].as_f64().unwrap();
        assert!((dpd - 3.0 / 60.0).abs() < 0.01);
        // Sans items : agrégats null mais la réponse reste formée.
        let v_none = aggregates(None, &samples, Some(since_7d), Some(7.0 * 86_400.0), now);
        assert_eq!(v_none["items_available"], false);
        assert!(v_none["lead_time_avg_s"].is_null());
        assert_eq!(v_none["workers_peak"], 4);
    }

    #[test]
    fn compact_bounds() {
        let dir = std::env::temp_dir().join(format!("ac-metrics-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metrics.jsonl");
        let fresh = now_secs() - 100.0;
        let stale = now_secs() - 40.0 * 86_400.0; // > MAX_AGE_S
        let content = format!(
            "{}\n{}\ncorrompu{{{{\n{}\n{}\n",
            json!({"ts": fresh, "pending": 1}),
            json!({"ts": stale, "pending": 9}),
            json!({"ts": fresh, "pending": 2}),
            json!({"foo": 1}), // sans ts → dropped
        );
        std::fs::write(&path, content).unwrap();
        let (kept, dropped) = compact(&path).unwrap();
        // dropped : ligne stale + ligne corrompue + ligne sans ts.
        assert_eq!((kept, dropped), (2, 3));
        let out = std::fs::read_to_string(&path).unwrap();
        assert_eq!(out.lines().count(), 2);
        assert!(out.contains("\"pending\":1") && out.contains("\"pending\":2"));
        // Le .part ne traîne pas après un rename réussi.
        assert!(!path.with_extension("jsonl.part").exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
