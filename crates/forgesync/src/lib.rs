//! Sync des statuts du registre PR depuis les forges live — partagé
//! entre le démon (RPC `pr_sync`, mise à jour directe du registre) et
//! acctl (`pr sync`, mise à jour via `pr_track`). Lecture seule côté
//! forge : `gh` CLI pour github, REST pour gitlab (`/api/v4`) et gerrit
//! (`/changes/<num|Change-Id>`) — dégradation propre par hôte
//! injoignable (l'entrée reste inchangée, une note la signale).
//!
//! Toutes les fonctions sont bloquantes (subprocess `gh`, ureq) : le
//! démon les appelle dans `spawn_blocking`.

use serde_json::{json, Value};

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Deadline d'un appel `gh` : un gh wedged (credential helper bloqué,
/// socket morte) ne doit pas retenir un thread `spawn_blocking` du
/// démon indéfiniment — les runs « stuck » sont autorisés à repartir
/// après 180 s côté démon, sans kill le process s'accumulerait.
const GH_TIMEOUT: Duration = Duration::from_secs(20);

/// Concurrence max des appels forge pendant un sync — bornée : `gh`
/// et les hôtes REST n'aiment pas les rafales (rate limit GitHub,
/// charge serveur), et un registre étoffé ne doit pas non plus ouvrir
/// des centaines de sockets d'un coup.
const SYNC_CONCURRENCY: usize = 6;

/// Sortie stdout d'une commande `gh` (None si gh absent, en échec ou
/// trop lent — le process est tué passé GH_TIMEOUT). Sortie vide
/// conservée : `gh_ok` distingue « succès silencieux » d'un échec.
fn gh_run(args: &[&str]) -> Option<String> {
    let mut child = Command::new("gh")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // Défense anti-wedge : pas de prompt interactif, pas de pager.
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_PAGER", "cat")
        .spawn()
        .ok()?;
    // Le pipe stdout est vidé en parallèle : une sortie > capacité du
    // pipe (~64 Kio, `gh pr list --limit 200` la dépasse sur un repo à
    // longs titres) bloquerait gh sur write() sans lecture concurrente.
    let mut stdout = child.stdout.take()?;
    let drain = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf);
        buf
    });
    let deadline = Instant::now() + GH_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            // Deadline ou erreur de wait : tuer puis récolter — pas de
            // zombie ; le drain meurt de l'EOF d'après-mort seul.
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    Some(drain.join().ok()?.trim().to_string())
}

/// Sortie stdout d'une commande `gh` — None si gh absent, en échec ou
/// stdout vide. Publique : acctl s'en sert aussi pour résoudre un repo
/// (`gh repo view`) hors sync.
pub fn gh_out(args: &[&str]) -> Option<String> {
    let s = gh_run(args)?;
    if s.is_empty() { None } else { Some(s) }
}

/// `true` si `gh` a répondu (exit 0) — probe pour les commandes qui
/// n'écrivent rien ou rapportent sur stderr (`gh auth status`).
fn gh_ok(args: &[&str]) -> bool {
    gh_run(args).is_some()
}

/// Schéma HTTP d'un hôte de forge : `http` uniquement pour le loopback
/// (Gerrit de test local) — `https` partout ailleurs. Règle unique
/// utilisée par `pr add` ET `pr sync`.
fn forge_scheme(host: &str) -> &'static str {
    if host.starts_with("localhost") || host.starts_with("127.") || host.starts_with("[::1]") {
        "http"
    } else {
        "https"
    }
}

/// Base URL web canonique d'un hôte de forge.
fn host_base(host: &str) -> String {
    format!("{}://{host}", forge_scheme(host))
}

/// Réponse HTTP d'un GET forge — `Code` conserve le statut d'erreur
/// (404 ≠ panne : la vérification d'existence en a besoin pour trancher
/// « absent » vs « injoignable ») ; `Down` = transport/timeout/JSON
/// illisible.
enum Http {
    Ok(Value),
    Code(u16),
    Down,
}

/// GET JSON avec statut conservé — même politique que `http_json`
/// (timeout borné, anti-XSSI Gerrit, redirects refusés).
fn http_status(url: &str, headers: &[(&str, String)]) -> Http {
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(std::time::Duration::from_secs(8))
        .build();
    let mut req = agent.get(url);
    for (k, v) in headers {
        req = req.set(k, v.as_str());
    }
    match req.call() {
        Ok(resp) => {
            let Ok(body) = resp.into_string() else {
                return Http::Down;
            };
            let body = body.strip_prefix(")]}'").map_or(body.as_str(), |b| {
                b.trim_start_matches(|c: char| c == '\n' || c == '\'')
            });
            serde_json::from_str(body).map(Http::Ok).unwrap_or(Http::Down)
        }
        Err(ureq::Error::Status(code, _)) => Http::Code(code),
        Err(_) => Http::Down,
    }
}

/// GET JSON avec timeout borné — sync GitLab/Gerrit. Tolérant : toute
/// erreur (réseau, non-2xx, JSON) → None, l'appelant dégrade proprement.
/// Le préfixe anti-XSSI `)]}'` de Gerrit est strippé. Redirects refusés :
/// un en-tête d'auth ne doit jamais suivre vers un autre hôte.
fn http_json(url: &str, headers: &[(&str, String)]) -> Option<Value> {
    match http_status(url, headers) {
        Http::Ok(v) => Some(v),
        _ => None,
    }
}

/// Code HTTP rapporté par `gh api` sur stderr (« gh: Not Found (HTTP
/// 404) », « gh: API rate limit exceeded … (HTTP 403) ») — None quand
/// l'échec n'est pas une réponse HTTP (DNS, timeout, gh absent).
fn gh_err_code(stderr: &str) -> Option<u16> {
    let mut rest = stderr;
    while let Some(i) = rest.find("HTTP") {
        rest = rest[i + 4..].trim_start_matches([' ', '/']);
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(code) = digits.parse::<u16>() {
            return Some(code);
        }
    }
    None
}

/// `gh api <endpoint>` — sortie classée : `Ok(stdout)` si exit 0,
/// `Http(code)` sur un échec HTTP identifiable, `Down` sinon (gh absent,
/// réseau, timeout — jamais un refus définitif).
fn gh_api(endpoint: &str) -> GhCall {
    let mut child = match Command::new("gh")
        // `--hostname` épingle github.com : sans lui, `GH_HOST` dans
        // l'env du process orienterait la requête vers une autre
        // instance (verdict rendu sur la mauvaise forge).
        .args(["api", "--hostname", "github.com", endpoint])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_PAGER", "cat")
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return GhCall::Down,
    };
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out_drain = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf);
        buf
    });
    let err_drain = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf);
        buf
    });
    let deadline = Instant::now() + GH_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return GhCall::Down;
            }
        }
    };
    if status.success() {
        let body = out_drain.join().unwrap_or_default();
        return GhCall::Ok(body);
    }
    let _ = out_drain.join();
    let err = err_drain.join().unwrap_or_default();
    match gh_err_code(&err) {
        Some(code) => GhCall::Http(code),
        None => GhCall::Down,
    }
}

/// Résultat classé d'un `gh api` — voir `gh_api`.
enum GhCall {
    /// stdout de la réponse — la vérif existence lit head/base dans
    /// le payload pulls pour reconnaître la release PR.
    Ok(String),
    Http(u16),
    Down,
}

/// Encodage path-segment (RFC 3986 unreserved) — chemins de projet dans
/// les URLs d'API GitLab (`group%2Fsub%2Fproj`).
fn urlenc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Hôtes déclarés pour un type de forge (réponse `forge_list`), avec
/// leur `token_env`.
fn forge_hosts<'a>(set: &'a [Value], kind: &str) -> Vec<(&'a str, Option<&'a str>)> {
    set.iter()
        .filter(|f| f["type"].as_str() == Some(kind))
        .flat_map(|f| {
            f["hosts"]
                .as_array()
                .map(|a| a.as_slice())
                .unwrap_or(&[])
                .iter()
                .map(|h| {
                    (
                        h["host"].as_str().unwrap_or_else(|| h.as_str().unwrap_or("")),
                        h["token_env"].as_str(),
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// En-têtes d'auth d'une forge : `token_env` est lu dans l'env de
/// l'appelant du sync — démon pour `pr_sync`, acctl pour `pr sync`
/// (GitLab `PRIVATE-TOKEN` seulement ; Gerrit sync = anonyme).
fn forge_auth(set: &[Value], forge: &str, host: &str) -> Vec<(String, String)> {
    if forge != "gitlab" {
        return Vec::new();
    }
    for (h, env) in forge_hosts(set, forge) {
        if h == host {
            if let Some(name) = env {
                if let Ok(tok) = std::env::var(name) {
                    if !tok.is_empty() {
                        return vec![("PRIVATE-TOKEN".into(), tok)];
                    }
                }
            }
        }
    }
    Vec::new()
}

/// État GitHub → label du registre.
fn gh_state(v: &Value) -> String {
    let st = v["state"].as_str().unwrap_or("").to_lowercase();
    if st == "open" && v["isDraft"].as_bool().unwrap_or(false) {
        "draft".into()
    } else {
        st
    }
}

/// État GitLab → label du registre : opened(+draft) | merged | closed.
fn glab_state(v: &Value) -> String {
    match v["state"].as_str().unwrap_or("").to_lowercase().as_str() {
        "merged" => "merged".into(),
        "closed" => "closed".into(),
        _ => {
            if v["draft"].as_bool().unwrap_or(false)
                || v["work_in_progress"].as_bool().unwrap_or(false)
            {
                "draft".into()
            } else {
                "open".into()
            }
        }
    }
}

/// État Gerrit → label du registre : NEW(+wip) | MERGED | ABANDONED.
fn gerrit_state(v: &Value) -> String {
    match v["status"].as_str().unwrap_or("") {
        "MERGED" => "merged".into(),
        "ABANDONED" => "closed".into(),
        _ => {
            if v["work_in_progress"].as_bool().unwrap_or(false) {
                "draft".into()
            } else {
                "open".into()
            }
        }
    }
}

/// Cible d'une référence de review pour `live_ref` (équivalent du
/// `PrTarget` résolu par acctl côté `pr add`).
pub struct RefTarget {
    pub forge: String,
    pub host: String,
    pub repo: String,
    pub pr: u64,
    pub change_id: Option<String>,
}

/// Snapshot d'une référence depuis la forge live (None si indisponible).
/// `{status, title, branch, url, pr}` — `pr` est le numéro résolu quand
/// la référence était un Change-Id Gerrit.
pub fn live_ref(set: &[Value], t: &RefTarget) -> Option<Value> {
    let auth = forge_auth(set, &t.forge, &t.host);
    let headers: Vec<(&str, String)> = auth
        .iter()
        .map(|(k, v)| (k.as_str(), v.clone()))
        .collect();
    match t.forge.as_str() {
        "github" => {
            let n = t.pr;
            gh_out(&[
                "pr", "view", &n.to_string(), "--repo", &t.repo, "--json",
                "title,state,isDraft,headRefName,url",
            ])
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .map(|v| {
                json!({
                    "status": gh_state(&v),
                    "title": v["title"],
                    "branch": v["headRefName"],
                    "url": v["url"],
                    "pr": n,
                })
            })
        }
        "gitlab" => {
            let base = host_base(&t.host);
            let url = format!(
                "{base}/api/v4/projects/{}/merge_requests/{}",
                urlenc(&t.repo),
                t.pr
            );
            http_json(&url, &headers).map(|v| {
                json!({
                    "status": glab_state(&v),
                    "title": v["title"],
                    "branch": v["source_branch"],
                    "url": v["web_url"],
                    "pr": t.pr,
                })
            })
        }
        "gerrit" => {
            // Change-Id (`I<sha>`) ou numéro — l'API /changes/ accepte les deux.
            let n_str = t.pr.to_string();
            let q = t.change_id.as_deref().unwrap_or(&n_str);
            let base = host_base(&t.host);
            http_json(&format!("{base}/changes/{q}"), &headers).map(|v| {
                json!({
                    "status": gerrit_state(&v),
                    "title": v["subject"],
                    "branch": v["branch"],
                    // `project` résolu — autorité sur le repo deviné.
                    "repo": v["project"],
                    "url": v["change_id"].as_str().map(|_| proto::forge_url(
                        "gerrit", &t.host,
                        v["project"].as_str().unwrap_or(&t.repo),
                        v["_number"].as_u64().unwrap_or(t.pr),
                        None,
                    )),
                    "pr": v["_number"].as_u64().unwrap_or(t.pr),
                })
            })
        }
        _ => None,
    }
}

/// Verdict d'une vérification d'existence d'une référence sur sa forge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verify {
    /// La forge répond et connaît la référence (repo + PR/MR/change).
    Exists,
    /// La forge répond et la référence (ou son repo) n'y existe pas —
    /// ou ne pourra jamais être relue par ce démon : 401/403 sur une
    /// forge REST = illisible sans droits qu'un sync ne retrouverait
    /// pas non plus ; hôte non déclaré = jamais requêté.
    Missing,
    /// Verdict impossible : panne réseau, 5xx, rate-limit, outil absent
    /// (`gh` manquant ou non authentifié côté github). À retenter —
    /// n'est jamais un refus définitif.
    Unreachable,
    /// La référence existe mais c'est la PR de release
    /// (head `next_release` → base `main`) : elle ne doit jamais entrer
    /// au registre — elle reste OPEN par construction et bloquerait la
    /// garde « toutes PR soldées » de l'orchestrateur (deadlock
    /// patchnotes vécu : #187 re-registrée à chaque `gh pr ready` du
    /// worker patchnotes).
    Release,
}

/// Existence-check d'une référence avant enregistrement auto (watcher →
/// file de vérif du démon). Mêmes canaux que le sync : `gh api
/// repos/{o}/{r}/pulls/{n}` pour github, REST borné aux hôtes déclarés
/// pour gitlab (`/api/v4/…/merge_requests/<iid>`) et gerrit
/// (`/changes/<num|Change-Id>`). Bloquant — à appeler hors du chemin
/// critique (spawn_blocking / worker dédié).
pub fn verify_ref(set: &[Value], t: &RefTarget) -> Verify {
    match t.forge.as_str() {
        "github" => verify_github(t),
        "gitlab" => verify_gitlab(set, t),
        "gerrit" => verify_gerrit(set, t),
        _ => Verify::Unreachable,
    }
}

/// GitHub n'accepte que `[A-Za-z0-9._-]` par segment d'`owner/repo`
/// — tout autre caractère rend le nom impossible à créer sur la forge.
/// Le garde est aussi une protection d'injection : injecté brut dans
/// l'endpoint `gh api`, `?` tronquerait le chemin (seul le repo serait
/// requêté → faux `Exists`), `{}` déclencherait la substitution depuis
/// le dépôt du cwd (`{owner}`/`{repo}`).
fn github_repo_ok(repo: &str) -> bool {
    repo.split('/').all(|seg| {
        !seg.is_empty()
            && seg.chars().any(|c| c != '.')
            && seg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    })
}

/// GitHub : la PR existe si `gh api` la lit. Sans `gh` fonctionnel
/// (binaire absent ou `gh auth status` KO) aucun verdict n'est possible
/// — et le sync ne pourrait de toute façon jamais relire l'entrée.
/// Hôte ≠ github.com : hors périmètre de `gh`, jamais résolvable →
/// `Missing` direct (l'extracteur ne produit d'ailleurs que github.com).
fn verify_github(t: &RefTarget) -> Verify {
    if t.host != "github.com" || !github_repo_ok(&t.repo) {
        return Verify::Missing;
    }
    if !gh_ok(&["auth", "status", "--hostname", "github.com"]) {
        return Verify::Unreachable;
    }
    match gh_api(&format!("repos/{}/pulls/{}", t.repo, t.pr)) {
        GhCall::Ok(body) => {
            // La release PR `next_release → main` (auto-release-pr.yml)
            // existe mais reste exclue du registre — cf. Verify::Release.
            let v = serde_json::from_str::<Value>(&body).unwrap_or_default();
            if v["head"]["ref"].as_str() == Some("next_release")
                && v["base"]["ref"].as_str() == Some("main")
            {
                Verify::Release
            } else {
                Verify::Exists
            }
        }
        GhCall::Http(404) | GhCall::Http(410) => Verify::Missing,
        // 403 (rate-limit, SSO non granté), 5xx, échec sans statut →
        // indécidable : à retenter, pas un refus définitif.
        _ => Verify::Unreachable,
    }
}

/// GitLab : MR lue via REST `merge_requests/<iid>` — borné aux hôtes
/// déclarés (comme la collecte : le registre ne doit pas devenir un
/// vecteur de requêtes vers un hôte arbitraire). 401/403 sur un hôte
/// déclaré = le démon ne pourra jamais la relire → `Missing`.
fn verify_gitlab(set: &[Value], t: &RefTarget) -> Verify {
    let declared: Vec<&str> = forge_hosts(set, "gitlab")
        .iter()
        .map(|(h, _)| *h)
        .collect();
    if !declared.contains(&t.host.as_str()) {
        return Verify::Missing;
    }
    let auth = forge_auth(set, "gitlab", &t.host);
    let headers: Vec<(&str, String)> = auth
        .iter()
        .map(|(k, v)| (k.as_str(), v.clone()))
        .collect();
    let base = host_base(&t.host);
    let url = format!(
        "{base}/api/v4/projects/{}/merge_requests/{}",
        urlenc(&t.repo),
        t.pr
    );
    match http_status(&url, &headers) {
        Http::Ok(_) => Verify::Exists,
        Http::Code(401) | Http::Code(403) | Http::Code(404) | Http::Code(410) => Verify::Missing,
        _ => Verify::Unreachable,
    }
}

/// Gerrit : `GET /changes/<num|Change-Id>` anonyme, hôtes déclarés
/// seulement. 404 = change inconnu ; 401/403 = non lisible sans auth
/// (la collecte est anonyme aussi → non résoluble par construction).
fn verify_gerrit(set: &[Value], t: &RefTarget) -> Verify {
    let declared: Vec<&str> = forge_hosts(set, "gerrit")
        .iter()
        .map(|(h, _)| *h)
        .collect();
    if !declared.contains(&t.host.as_str()) {
        return Verify::Missing;
    }
    let n_str = t.pr.to_string();
    let q = t.change_id.as_deref().unwrap_or(&n_str);
    let base = host_base(&t.host);
    match http_status(&format!("{base}/changes/{q}"), &[]) {
        Http::Ok(_) => Verify::Exists,
        Http::Code(401) | Http::Code(403) | Http::Code(404) => Verify::Missing,
        _ => Verify::Unreachable,
    }
}

/// Mise à jour d'une entrée du registre — champs `Option` = écrasables
/// (même sémantique que `PrTrack`/`TrackIn` : `None` conserve).
pub struct PrUpdate {
    pub repo: String,
    pub pr: u64,
    pub forge: String,
    pub host: String,
    pub change_id: Option<String>,
    pub url: Option<String>,
    pub branch: Option<String>,
    pub title: Option<String>,
    /// Nouveau statut forge — toujours renseigné (c'est l'objet du sync).
    pub status: String,
    /// Statut registre avant sync (affichage `old -> new`).
    pub old_status: String,
}

/// Référence d'une entrée évaluée par un passage de sync — adressage
/// identique à `PrUpdate`/`PrUntrack`. `seen` = la forge a répondu
/// pour elle (état lu, changé ou non) ; `failed` = injoignable ce run
/// (hôte mort/non déclaré, repo ou référence absente).
pub struct PrSeen {
    pub forge: String,
    pub host: String,
    pub repo: String,
    pub pr: u64,
    pub change_id: Option<String>,
}

/// Résultat d'un passage de sync : `updates` à appliquer au registre +
/// `notes` de diagnostics non fatals (hôte injoignable, PRs ouvertes
/// non enregistrées) + `checked` (entrées pour lesquelles un état live
/// a été lu — les hôtes morts ne comptent pas) + `seen`/`failed` pour
/// le compteur d'échecs du registre (affichage « non résoluble »).
pub struct SyncOutcome {
    pub checked: usize,
    pub updates: Vec<PrUpdate>,
    pub seen: Vec<PrSeen>,
    pub failed: Vec<PrSeen>,
    pub notes: Vec<String>,
}

/// `PrSeen` depuis une entrée du registre sérialisée — même défauts
/// de forge/host que la clé du registre.
fn seen_of(e: &Value) -> PrSeen {
    let forge = proto::forge_kind(e["forge"].as_str().unwrap_or("")).to_string();
    PrSeen {
        host: e["host"]
            .as_str()
            .unwrap_or_else(|| proto::forge_default_host(&forge))
            .to_string(),
        forge,
        repo: e["repo"].as_str().unwrap_or("").to_string(),
        pr: e["pr"].as_u64().unwrap_or(0),
        change_id: e["change_id"].as_str().map(String::from),
    }
}

/// Kind github dont l'hôte n'est pas github.com : hors périmètre —
/// `gh` n'interroge que github.com et relibeller l'entrée la
/// re-keyerait sur le mauvais hôte. Hôte absent = github.com (registre
/// historique).
fn gh_host_ok(e: &Value) -> bool {
    e["host"].as_str().unwrap_or("github.com") == "github.com"
}

/// Un appel forge unitaire de la collecte — github = un `gh pr list`
/// par repo (déjà groupé), gitlab/gerrit = une requête REST par entrée.
enum SyncJob<'a> {
    GithubRepo(&'a str),
    Gitlab(&'a Value),
    Gerrit(&'a Value),
}

/// Contribution d'un job de collecte à `SyncOutcome` — produite en
/// parallèle puis fusionnée dans l'ordre des jobs (déterministe,
/// identique à l'ordre séquentiel historique).
#[derive(Default)]
struct SyncPartial {
    checked: usize,
    updates: Vec<PrUpdate>,
    seen: Vec<PrSeen>,
    failed: Vec<PrSeen>,
    notes: Vec<String>,
}

impl SyncPartial {
    fn fail(&mut self, e: &Value) {
        self.failed.push(seen_of(e));
    }
}

impl SyncOutcome {
    fn absorb(&mut self, p: SyncPartial) {
        self.checked += p.checked;
        self.updates.extend(p.updates);
        self.seen.extend(p.seen);
        self.failed.extend(p.failed);
        self.notes.extend(p.notes);
    }
}

/// Exécute `f` sur chaque item avec au plus `SYNC_CONCURRENCY` threads —
/// la collecte forge est I/O-bound (subprocess `gh`, HTTP ureq) : le
/// parallélisme ramène le temps total à ~la lenteur max d'un appel au
/// lieu de la somme. L'ordre des résultats suit celui des items.
fn par_map<T: Sync, R: Send>(items: &[T], f: &(dyn Fn(&T) -> R + Sync)) -> Vec<R> {
    let n = items.len();
    if n <= 1 {
        return items.iter().map(f).collect();
    }
    let workers = SYNC_CONCURRENCY.min(n);
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel::<(usize, R)>();
    let slots = std::thread::scope(|s| {
        for _ in 0..workers {
            let tx = tx.clone();
            let next = &next;
            s.spawn(move || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    break;
                }
                // rx ne se ferme qu'en fin de scope : un envoi en
                // échec signifierait un bug — on sort plutôt que
                // boucler.
                if tx.send((i, f(&items[i]))).is_err() {
                    break;
                }
            });
        }
        drop(tx);
        let mut slots: Vec<Option<R>> = (0..n).map(|_| None).collect();
        for _ in 0..n {
            // Un worker panic → son tx meurt ; scope propage la panic à
            // la sortie, les slots manquants ne sont jamais lus.
            match rx.recv() {
                Ok((i, r)) => slots[i] = Some(r),
                Err(_) => break,
            }
        }
        slots
    });
    slots
        .into_iter()
        .map(|p| p.expect("par_map: slot vide"))
        .collect()
}

/// Interroge les forges live pour chaque entrée du registre et produit
/// les mises à jour à appliquer (aucune écriture ici — le démon track()
/// directement, acctl pousse `pr_track`). `entries` = tableau sérialisé
/// des PrEntry (`forge`, `host`, `repo`, `pr`, `change_id`, `status`).
/// `set` = réponse `forge_list` (types + hôtes + token_env).
///
/// Les appels forge tournent en parallèle borné (`SYNC_CONCURRENCY`) :
/// un registre étoffé ou une forge lente ne fait plus la somme des
/// latences — le temps total suit ~la lenteur max des jobs.
pub fn collect_updates(set: &[Value], entries: &[Value]) -> SyncOutcome {
    let started = Instant::now();
    let mut out = SyncOutcome {
        checked: 0,
        updates: Vec::new(),
        seen: Vec::new(),
        failed: Vec::new(),
        notes: Vec::new(),
    };
    // Kind github dont l'hôte n'est pas github.com : hors périmètre —
    // `gh` n'interroge que github.com et relibeller l'entrée la
    // re-keyerait sur le mauvais hôte. Hôte absent = github.com (registre
    // historique). Classé d'office, sans job : jamais résolvable.
    for e in entries {
        if proto::forge_kind(e["forge"].as_str().unwrap_or("")) == "github" && !gh_host_ok(e) {
            out.notes.push(format!(
                "github {} #{}: hôte {} non géré — statut inchangé",
                e["repo"].as_str().unwrap_or("?"),
                e["pr"].as_u64().unwrap_or(0),
                e["host"].as_str().unwrap_or("?"),
            ));
            out.failed.push(seen_of(e));
        }
    }
    let mut repos: Vec<String> = entries
        .iter()
        .filter(|e| proto::forge_kind(e["forge"].as_str().unwrap_or("")) == "github")
        .filter(|e| gh_host_ok(e))
        .filter_map(|e| e["repo"].as_str().map(String::from))
        .collect();
    repos.sort();
    repos.dedup();
    // Probe unique : `gh auth status` = binaire présent + token valide
    // (exit code seul — le rapport peut sortir sur stderr selon les
    // versions). Sans lui impossible de distinguer « repo absent » de
    // « gh KO ».
    let gh_up = !repos.is_empty() && gh_ok(&["auth", "status"]);
    // Jobs dans l'ordre de fusion historique : repos github triés,
    // puis entrées gitlab, puis gerrit (ordre du registre).
    let mut jobs: Vec<SyncJob> = repos
        .iter()
        .map(|r| SyncJob::GithubRepo(r.as_str()))
        .collect();
    jobs.extend(
        entries
            .iter()
            .filter(|e| proto::forge_kind(e["forge"].as_str().unwrap_or("")) == "gitlab")
            .map(SyncJob::Gitlab),
    );
    jobs.extend(
        entries
            .iter()
            .filter(|e| proto::forge_kind(e["forge"].as_str().unwrap_or("")) == "gerrit")
            .map(SyncJob::Gerrit),
    );
    let concurrency = SYNC_CONCURRENCY.min(jobs.len());
    let parts = par_map(&jobs, &|j| match j {
        SyncJob::GithubRepo(repo) => github_repo_sync(repo, gh_up, entries),
        SyncJob::Gitlab(e) => gitlab_entry_sync(set, e),
        SyncJob::Gerrit(e) => gerrit_entry_sync(set, e),
    });
    for p in parts {
        out.absorb(p);
    }
    tracing::info!(
        jobs = jobs.len(),
        concurrency,
        checked = out.checked,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "registre PR : collecte forge terminée"
    );
    out
}

/// GitHub — `gh pr list` groupé par repo connu : les repos comptent
/// comme « vus » dès que gh répond (un hôte down laisse les entrées
/// inchangées, sans note — gh absent est le cas nominal d'un démon
/// minimal). Un repo dont le `gh pr list` échoue alors que gh est
/// prouvé utilisable (`gh auth status` OK) = repo/référence injoignable
/// → compté en échec ; gh absent ou non authentifié n'est PAS un
/// échec d'entrée (aucune entrée ne doit passer « non résoluble » sur
/// une machine sans gh fonctionnel).
fn github_repo_sync(repo: &str, gh_up: bool, entries: &[Value]) -> SyncPartial {
    let mut p = SyncPartial::default();
    let live = gh_out(&[
        "pr", "list", "--repo", repo, "--state", "all",
        "--limit", "200", "--json",
        "number,title,state,isDraft,headRefName",
    ])
    .and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok());
    let Some(live) = live else {
        if gh_up {
            // gh sert mais ce repo ne répond pas → fantôme/privé.
            p.notes.push(format!(
                "github {repo}: repo absent ou inaccessible — statuts inchangés"
            ));
            for e in entries.iter().filter(|e| {
                proto::forge_kind(e["forge"].as_str().unwrap_or("")) == "github"
                    && gh_host_ok(e)
                    && e["repo"].as_str() == Some(repo)
            }) {
                p.fail(e);
            }
        } else {
            p.notes
                .push(format!("github {repo}: gh indisponible — statuts inchangés"));
        }
        return p;
    };
    let tracked: Vec<&Value> = entries
        .iter()
        .filter(|e| {
            proto::forge_kind(e["forge"].as_str().unwrap_or("")) == "github"
                && gh_host_ok(e)
                && e["repo"].as_str() == Some(repo)
        })
        .collect();
    for e in &tracked {
        let n = e["pr"].as_u64().unwrap_or(0);
        // `--limit 200` tronque la liste : sur un gros repo une
        // entrée ancienne n'y figure plus sans être absente —
        // requête directe avant de la compter injoignable.
        let pr = live
            .iter()
            .find(|v| v["number"].as_u64() == Some(n))
            .cloned()
            .or_else(|| {
                gh_out(&[
                    "pr", "view", &n.to_string(), "--repo", repo, "--json",
                    "number,title,state,isDraft,headRefName",
                ])
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            });
        let Some(pr) = pr.filter(|v| v["number"].as_u64() == Some(n)) else {
            // Le repo répond mais la référence n'y est plus
            // (supprimée, jamais existée) → non résoluble en cours.
            p.fail(e);
            continue;
        };
        p.checked += 1;
        p.seen.push(seen_of(e));
        let st = gh_state(&pr);
        let old = e["status"].as_str().unwrap_or("");
        if st != old {
            p.updates.push(PrUpdate {
                repo: repo.to_string(),
                pr: n,
                forge: "github".into(),
                host: "github.com".into(),
                change_id: None,
                url: None,
                branch: pr["headRefName"].as_str().map(String::from),
                title: pr["title"].as_str().map(String::from),
                status: st,
                old_status: old.into(),
            });
        }
    }
    let untracked: Vec<u64> = live
        .iter()
        .filter(|v| v["state"].as_str() == Some("OPEN"))
        .map(|v| v["number"].as_u64().unwrap_or(0))
        .filter(|n| !tracked.iter().any(|e| e["pr"].as_u64() == Some(*n)))
        .collect();
    if !untracked.is_empty() {
        p.notes.push(format!(
            "{repo}: PRs ouvertes non enregistrées: {}",
            untracked
                .iter()
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    p
}

/// GitLab — REST `/api/v4/…/merge_requests/<iid>` par entrée, token
/// `PRIVATE-TOKEN` si `token_env` configuré. Hôte privé sans token →
/// API 401 → entrée ignorée proprement. Hôtes bornés à ceux déclarés
/// (`forge_list` inclut gitlab.com implicitement) — le registre ne doit
/// pas devenir un vecteur de requêtes vers un hôte arbitraire.
fn gitlab_entry_sync(set: &[Value], e: &Value) -> SyncPartial {
    let mut p = SyncPartial::default();
    let host = e["host"].as_str().unwrap_or("gitlab.com");
    let repo = e["repo"].as_str().unwrap_or("");
    let n = e["pr"].as_u64().unwrap_or(0);
    let declared: Vec<&str> = forge_hosts(set, "gitlab")
        .iter()
        .map(|(h, _)| *h)
        .collect();
    if !declared.contains(&host) {
        p.notes.push(format!(
            "gitlab {host} {repo}!{n}: hôte non déclaré — statut inchangé"
        ));
        // Jamais résolvable tant que l'hôte n'est pas déclaré :
        // le compteur le dit (repo fantôme ou config incomplète).
        p.fail(e);
        return p;
    }
    let auth = forge_auth(set, "gitlab", host);
    let headers: Vec<(&str, String)> = auth
        .iter()
        .map(|(k, v)| (k.as_str(), v.clone()))
        .collect();
    let base = host_base(host);
    let url = format!(
        "{base}/api/v4/projects/{}/merge_requests/{n}",
        urlenc(repo)
    );
    let Some(v) = http_json(&url, &headers) else {
        p.notes.push(format!(
            "gitlab {host} {repo}!{n}: indisponible — statut inchangé"
        ));
        p.fail(e);
        return p;
    };
    p.checked += 1;
    p.seen.push(seen_of(e));
    let st = glab_state(&v);
    let old = e["status"].as_str().unwrap_or("");
    if st != old {
        p.updates.push(PrUpdate {
            repo: repo.to_string(),
            pr: n,
            forge: "gitlab".into(),
            host: host.to_string(),
            change_id: None,
            url: v["web_url"].as_str().map(String::from),
            branch: v["source_branch"].as_str().map(String::from),
            title: v["title"].as_str().map(String::from),
            status: st,
            old_status: old.into(),
        });
    }
    p
}

/// Gerrit — REST `GET /changes/<ref>` anonyme, hôtes déclarés seulement
/// (comme l'extraction de refs : rien ne distingue un hôte Gerrit d'un
/// autre service sans déclaration). Les entrées adressées par
/// `change_id` résolvent leur numéro au passage (rekey côté registre).
fn gerrit_entry_sync(set: &[Value], e: &Value) -> SyncPartial {
    let mut p = SyncPartial::default();
    let host = e["host"].as_str().unwrap_or("");
    if host.is_empty() {
        // Entrée sans hôte : jamais résolvable — comptée en échec
        // plutôt qu'ignorée silencieusement.
        p.fail(e);
        return p;
    }
    let repo = e["repo"].as_str().unwrap_or("");
    let cid = e["change_id"].as_str();
    let n = e["pr"].as_u64().unwrap_or(0);
    let n_str = n.to_string();
    let q = cid.unwrap_or(&n_str);
    let declared: Vec<&str> = forge_hosts(set, "gerrit")
        .iter()
        .map(|(h, _)| *h)
        .collect();
    if !declared.contains(&host) {
        p.notes.push(format!(
            "gerrit {host} {q}: hôte non déclaré — statut inchangé"
        ));
        p.fail(e);
        return p;
    }
    let base = host_base(host);
    let url = format!("{base}/changes/{q}");
    let Some(v) = http_json(&url, &[]) else {
        p.notes.push(format!(
            "gerrit {host} {q}: indisponible — statut inchangé"
        ));
        p.fail(e);
        return p;
    };
    p.checked += 1;
    p.seen.push(seen_of(e));
    let st = gerrit_state(&v);
    let resolved = v["_number"].as_u64().unwrap_or(n);
    let old = e["status"].as_str().unwrap_or("");
    if st != old || resolved != n {
        p.updates.push(PrUpdate {
            repo: v["project"].as_str().unwrap_or(repo).to_string(),
            pr: resolved,
            forge: "gerrit".into(),
            host: host.to_string(),
            change_id: cid
                .map(String::from)
                .or_else(|| v["change_id"].as_str().map(String::from)),
            url: v["change_id"].as_str().map(|_| {
                proto::forge_url(
                    "gerrit",
                    host,
                    v["project"].as_str().unwrap_or(repo),
                    resolved,
                    None,
                )
            }),
            branch: v["branch"].as_str().map(String::from),
            title: v["subject"].as_str().map(String::from),
            status: st,
            old_status: old.into(),
        });
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlenc_path_segment() {
        assert_eq!(urlenc("grp/sub proj"), "grp%2Fsub%20proj");
        assert_eq!(urlenc("a.b-c_d~e"), "a.b-c_d~e");
        assert_eq!(urlenc(""), "");
    }

    #[test]
    fn gh_state_maps() {
        assert_eq!(gh_state(&json!({"state": "OPEN"})), "open");
        assert_eq!(gh_state(&json!({"state": "OPEN", "isDraft": true})), "draft");
        assert_eq!(gh_state(&json!({"state": "MERGED"})), "merged");
        assert_eq!(gh_state(&json!({"state": "CLOSED"})), "closed");
        assert_eq!(gh_state(&json!({})), "");
    }

    #[test]
    fn glab_state_maps() {
        assert_eq!(glab_state(&json!({"state": "opened"})), "open");
        assert_eq!(glab_state(&json!({"state": "opened", "draft": true})), "draft");
        assert_eq!(
            glab_state(&json!({"state": "opened", "work_in_progress": true})),
            "draft"
        );
        assert_eq!(glab_state(&json!({"state": "merged"})), "merged");
        assert_eq!(glab_state(&json!({"state": "closed"})), "closed");
        assert_eq!(glab_state(&json!({"state": "locked"})), "open");
    }

    #[test]
    fn gerrit_state_maps() {
        assert_eq!(gerrit_state(&json!({"status": "NEW"})), "open");
        assert_eq!(
            gerrit_state(&json!({"status": "NEW", "work_in_progress": true})),
            "draft"
        );
        assert_eq!(gerrit_state(&json!({"status": "MERGED"})), "merged");
        assert_eq!(gerrit_state(&json!({"status": "ABANDONED"})), "closed");
    }

    /// Le code HTTP est retrouvé dans les formats d'erreur `gh api`
    /// (« gh: Not Found (HTTP 404) », rate-limit 403) ; une panne sans
    /// statut → None (Down, jamais un Missing).
    #[test]
    fn gh_err_code_parses() {
        assert_eq!(gh_err_code("gh: Not Found (HTTP 404)"), Some(404));
        assert_eq!(
            gh_err_code("gh: API rate limit exceeded for user ID 42. (HTTP 403)"),
            Some(403)
        );
        assert_eq!(gh_err_code("gh: HTTP 502 Bad Gateway"), Some(502));
        assert_eq!(gh_err_code("error connecting to api.github.com"), None);
        assert_eq!(gh_err_code(""), None);
        // « HTTP » sans code qui suit → poursuite du scan.
        assert_eq!(gh_err_code("HTTP then gh: Not Found (HTTP 404)"), Some(404));
    }

    #[test]
    fn github_repo_ok_charset() {
        assert!(github_repo_ok("cli/cli"));
        assert!(github_repo_ok("Wameuh/AgentControl"));
        assert!(github_repo_ok("my-org/re.po_name-x"));
        // `?`/`#` tronqueraient l'endpoint `gh api`.
        assert!(!github_repo_ok("o/r?x"));
        assert!(!github_repo_ok("o/r#f"));
        // `{}` → substitution depuis le dépôt du cwd par `gh`.
        assert!(!github_repo_ok("{owner}/{repo}"));
        assert!(!github_repo_ok("o/{repo}"));
        // `%`, espace, segments vides ou tout-points : impossibles.
        assert!(!github_repo_ok("o/r%2f"));
        assert!(!github_repo_ok("o/r x"));
        assert!(!github_repo_ok("o//r"));
        assert!(!github_repo_ok("o/.."));
        assert!(!github_repo_ok("./x"));
    }

    /// `par_map` traite chaque item exactement une fois et rend les
    /// résultats dans l'ordre des items, quelle que soit la fin des
    /// jobs (délais variables).
    #[test]
    fn par_map_preserves_order() {
        let items: Vec<usize> = (0..3 * SYNC_CONCURRENCY + 5).collect();
        let out = par_map(&items, &|i| {
            std::thread::sleep(Duration::from_millis((i % 7) as u64));
            i * 2
        });
        assert_eq!(out, items.iter().map(|i| i * 2).collect::<Vec<_>>());
        assert_eq!(par_map(&[] as &[usize], &|i| i + 1), Vec::<usize>::new());
        assert_eq!(par_map(&[7usize], &|i| i + 1), vec![8]);
    }

    /// Les hôtes absents de `forge_list` ne doivent jamais être
    /// interrogés : la collecte les note « non déclarés » sans toucher
    /// au réseau (test rapide — aucun appel sortant pour ces entrées).
    #[test]
    fn undeclared_hosts_are_skipped() {
        let set = json!([
            {"type": "gitlab", "hosts": [{"host": "gitlab.com"}]},
            {"type": "gerrit", "hosts": [{"host": "gerrit.int"}]},
        ]);
        let entries = json!([
            {"forge": "gitlab", "host": "gl.evil.example", "repo": "a/b", "pr": 7, "status": "open"},
            {"forge": "gerrit", "host": "gr.evil.example", "repo": "a", "pr": 9, "status": "open"},
            {"forge": "github", "host": "hub.evil.example", "repo": "a/b", "pr": 3, "status": "open"},
        ]);
        let out = collect_updates(
            set.as_array().unwrap(),
            entries.as_array().unwrap(),
        );
        assert_eq!(out.checked, 0);
        assert!(out.updates.is_empty());
        assert_eq!(out.notes.len(), 3);
        assert!(out.notes.iter().all(|n| n.contains("non déclaré") || n.contains("non géré")));
    }
}
