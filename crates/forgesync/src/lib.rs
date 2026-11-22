//! Sync des statuts du registre PR depuis les forges live — partagé
//! entre le démon (RPC `pr_sync`, mise à jour directe du registre) et
//! acctl (`pr sync`, mise à jour via `pr_track`). Lecture seule côté
//! forge : tous les accès passent par le trait `client::ForgeClient`
//! (`gh`/`glab` CLI, REST `/api/v4` gitlab, REST github, REST gerrit,
//! git nu dégradé) — dégradation propre par hôte injoignable ou
//! transport absent (l'entrée reste inchangée, une note la signale).
//!
//! Toutes les fonctions sont bloquantes (subprocess CLI, ureq) : le
//! démon les appelle dans `spawn_blocking`.

pub mod client;

pub use client::{
    client_for, host_known, spec_for, ChangeComment, ChangeInfo, ForgeCaps, ForgeClient, ForgeSpec,
    ForgeTarget, ListQuery,
};

use serde_json::{json, Value};

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Deadline d'un appel CLI forge (`gh`/`glab`) : un binaire wedged
/// (credential helper bloqué, socket morte) ne doit pas retenir un
/// thread `spawn_blocking` du démon indéfiniment — les runs « stuck »
/// sont autorisés à repartir après 180 s côté démon, sans kill le
/// process s'accumulerait.
pub(crate) const CLI_TIMEOUT: Duration = Duration::from_secs(20);

/// Concurrence max des appels forge pendant un sync — bornée : `gh`
/// et les hôtes REST n'aiment pas les rafales (rate limit GitHub,
/// charge serveur), et un registre étoffé ne doit pas non plus ouvrir
/// des centaines de sockets d'un coup.
const SYNC_CONCURRENCY: usize = 6;

/// Sortie stdout d'une commande CLI forge (`gh`, `glab`, `git`) —
/// None si le binaire est absent, en échec ou trop lent (le process
/// est tué passé `timeout`). Sortie vide conservée : l'appelant
/// distingue « succès silencieux » d'un échec. `cwd` = répertoire de
/// travail (contexte repo des CLI) ; `Path::new(".")` = cwd hérité.
pub(crate) fn cli_run(bin: &str, args: &[&str], cwd: &Path, timeout: Duration) -> Option<String> {
    let mut child = Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // Défense anti-wedge : pas de prompt interactif, pas de pager.
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .spawn()
        .ok()?;
    // Le pipe stdout est vidé en parallèle : une sortie > capacité du
    // pipe (~64 Kio, `pr list --limit 200` la dépasse sur un repo à
    // longs titres) bloquerait le CLI sur write() sans lecture
    // concurrente.
    let mut stdout = child.stdout.take()?;
    let drain = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf);
        buf
    });
    let deadline = Instant::now() + timeout;
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

/// `cli_run` spécialisé `gh` — raccourci des impls github.
pub(crate) fn gh_run(args: &[&str], cwd: &Path) -> Option<String> {
    cli_run("gh", args, cwd, CLI_TIMEOUT)
}

/// Sortie stdout d'une commande `gh` — None si gh absent, en échec ou
/// stdout vide. Publique : acctl s'en sert aussi pour résoudre un repo
/// (`gh repo view`) hors sync — cwd hérité du process.
pub fn gh_out(args: &[&str]) -> Option<String> {
    let s = gh_run(args, Path::new("."))?;
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// `true` si `gh` a répondu (exit 0) — probe pour les commandes qui
/// n'écrivent rien ou rapportent sur stderr (`gh auth status`).
pub(crate) fn gh_ok(args: &[&str], cwd: &Path) -> bool {
    gh_run(args, cwd).is_some()
}

/// Schéma HTTP d'un hôte de forge : `http` uniquement pour le loopback
/// (Gerrit de test local) — `https` partout ailleurs. Règle unique
/// utilisée par `pr add` ET `pr sync`.
pub(crate) fn forge_scheme(host: &str) -> &'static str {
    if host.starts_with("localhost") || host.starts_with("127.") || host.starts_with("[::1]") {
        "http"
    } else {
        "https"
    }
}

/// Base URL web canonique d'un hôte de forge.
pub(crate) fn host_base(host: &str) -> String {
    format!("{}://{host}", forge_scheme(host))
}

/// Réponse HTTP d'un appel forge — `Code` conserve le statut d'erreur
/// (404 ≠ panne : la vérification d'existence en a besoin pour trancher
/// « absent » vs « injoignable ») ; `Down` = transport/timeout/JSON
/// illisible.
pub(crate) enum Http {
    Ok(Value),
    Code(u16),
    Down,
}

/// Appel HTTP JSON avec statut conservé — timeout borné, anti-XSSI
/// Gerrit, redirects refusés (un en-tête d'auth ne doit jamais suivre
/// vers un autre hôte). `body` Some → JSON sérialisé en corps de
/// requête (POST/PUT…) — `send_string` évite la feature `json` d'ureq.
pub(crate) fn http_status(
    method: &str,
    url: &str,
    headers: &[(&str, String)],
    body: Option<&Value>,
) -> Http {
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(std::time::Duration::from_secs(8))
        .build();
    let mut req = agent.request(method, url);
    for (k, v) in headers {
        req = req.set(k, v.as_str());
    }
    let resp = match body {
        Some(b) => req
            .set("Content-Type", "application/json")
            .send_string(&b.to_string()),
        None => req.call(),
    };
    match resp {
        Ok(resp) => {
            let Ok(body) = resp.into_string() else {
                return Http::Down;
            };
            let body = body.strip_prefix(")]}'").map_or(body.as_str(), |b| {
                b.trim_start_matches(|c: char| c == '\n' || c == '\'')
            });
            if body.trim().is_empty() {
                // Réponse vide (ex. POST Gerrit `:wip`) = succès sans payload.
                return Http::Ok(Value::Null);
            }
            serde_json::from_str(body)
                .map(Http::Ok)
                .unwrap_or(Http::Down)
        }
        Err(ureq::Error::Status(code, _)) => Http::Code(code),
        Err(_) => Http::Down,
    }
}

/// GET JSON avec statut conservé — raccourci `http_status`.
pub(crate) fn http_get(url: &str, headers: &[(&str, String)]) -> Http {
    http_status("GET", url, headers, None)
}

/// POST/PUT JSON classés — écriture forge (note MR, message review
/// Gerrit, bascule draft) : `Ok`/`Code`/`Down` partagés avec le GET.
pub(crate) fn http_send(method: &str, url: &str, headers: &[(&str, String)], body: &Value) -> Http {
    http_status(method, url, headers, Some(body))
}
/// Code HTTP rapporté par `gh api` sur stderr (« gh: Not Found (HTTP
/// 404) », « gh: API rate limit exceeded … (HTTP 403) ») — None quand
/// l'échec n'est pas une réponse HTTP (DNS, timeout, gh absent).
pub(crate) fn gh_err_code(stderr: &str) -> Option<u16> {
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

/// `gh api <endpoint>` sur `host` — sortie classée : `Ok(stdout)` si
/// exit 0, `Http(code)` sur un échec HTTP identifiable, `Down` sinon
/// (gh absent, réseau, timeout — jamais un refus définitif). `extra`
/// = args supplémentaires après l'endpoint (`--paginate`, `--jq`…).
pub(crate) fn gh_api(host: &str, endpoint: &str, extra: &[&str]) -> GhCall {
    let mut argv: Vec<&str> = vec!["api", "--hostname", host, endpoint];
    argv.extend_from_slice(extra);
    let mut child = match Command::new("gh")
        // `--hostname` épingle l'hôte cible : sans lui, `GH_HOST` dans
        // l'env du process orienterait la requête vers une autre
        // instance (verdict rendu sur la mauvaise forge).
        .args(&argv)
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
    let deadline = Instant::now() + CLI_TIMEOUT;
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
pub(crate) enum GhCall {
    /// stdout de la réponse — la vérif existence lit head/base dans
    /// le payload pulls pour reconnaître la release PR.
    Ok(String),
    Http(u16),
    Down,
}

/// Encodage path-segment (RFC 3986 unreserved) — chemins de projet dans
/// les URLs d'API GitLab (`group%2Fsub%2Fproj`).
pub(crate) fn urlenc(s: &str) -> String {
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
/// leur `token_env` et `transport` — le transport résout le client
/// (`spec_for`).
pub(crate) fn forge_hosts<'a>(
    set: &'a [Value],
    kind: &str,
) -> Vec<(&'a str, Option<&'a str>, Option<&'a str>)> {
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
                        h["host"]
                            .as_str()
                            .unwrap_or_else(|| h.as_str().unwrap_or("")),
                        h["token_env"].as_str(),
                        h["transport"].as_str(),
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// État GitHub → label du registre.
pub(crate) fn gh_state(v: &Value) -> String {
    let st = v["state"].as_str().unwrap_or("").to_lowercase();
    if st == "open" && v["isDraft"].as_bool().unwrap_or(false) {
        "draft".into()
    } else {
        st
    }
}

/// État GitLab → label du registre : opened(+draft) | merged | closed.
pub(crate) fn glab_state(v: &Value) -> String {
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
pub(crate) fn gerrit_state(v: &Value) -> String {
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

/// Review GitHub → label normalisé du registre : la `reviewDecision`
/// tranche (changes_requested > approved), une `reviewRequest` en
/// attente affine REVIEW_REQUIRED en "requested" (plus parlant), et
/// REVIEW_REQUIRED sans demande explicite reste "required". "" = rien
/// à signaler. Même vocabulaire que `PrEntry.review`.
pub(crate) fn gh_review_state(v: &Value) -> String {
    let dec = v["reviewDecision"].as_str().unwrap_or("");
    if dec.eq_ignore_ascii_case("CHANGES_REQUESTED") {
        return "changes_requested".into();
    }
    if dec.eq_ignore_ascii_case("APPROVED") {
        return "approved".into();
    }
    if v["reviewRequests"]
        .as_array()
        .is_some_and(|a| !a.is_empty())
    {
        return "requested".into();
    }
    if dec.eq_ignore_ascii_case("REVIEW_REQUIRED") {
        return "required".into();
    }
    String::new()
}

/// Checks CI GitHub (`statusCheckRollup` du `gh pr list`/`view` —
/// aucun appel supplémentaire) → pending | success | failure.
/// "" = rollup vide (pas de checks). Un check encore en cours rend
/// "pending" même si un autre a déjà échoué — le verdict final n'est
/// pas connu.
pub(crate) fn gh_ci_state(v: &Value) -> String {
    let Some(items) = v["statusCheckRollup"].as_array() else {
        return String::new();
    };
    if items.is_empty() {
        return String::new();
    }
    let mut pending = false;
    let mut failed = false;
    let mut ok = false;
    for it in items {
        // CheckRun → conclusion ; StatusContext → state (SUCCESS,
        // FAILURE, PENDING, EXPECTED, ERROR…). Les deux coexistent dans
        // le rollup ; `state` couvre les contextes sans check run.
        let val = it["conclusion"]
            .as_str()
            .or_else(|| it["state"].as_str())
            .unwrap_or("")
            .to_uppercase();
        match val.as_str() {
            "SUCCESS" => ok = true,
            // NEUTRAL/SKIPPED : ni succès ni échec (draft sans CI,
            // check sauté) — seuls, ils ne signalent rien.
            "NEUTRAL" | "SKIPPED" => {}
            "FAILURE" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED" | "STARTUP_FAILURE"
            | "STALE" | "ERROR" => failed = true,
            // En cours (status ≠ COMPLETED), EXPECTED/PENDING, ou
            // conclusion illisible → pending par prudence.
            _ => pending = true,
        }
    }
    if pending {
        "pending".into()
    } else if failed {
        "failure".into()
    } else if ok {
        "success".into()
    } else {
        String::new()
    }
}

/// Review GitLab : reviewers assignés sur la MR → "requested" (la
/// réponse MR standard n'a pas d'équivalent `reviewDecision`).
pub(crate) fn glab_review_state(v: &Value) -> String {
    if v["reviewers"].as_array().is_some_and(|a| !a.is_empty()) {
        "requested".into()
    } else {
        String::new()
    }
}

/// Pipeline de tête GitLab (`head_pipeline.status` du GET MR) →
/// pending | success | failure. "" = pas de pipeline ou statut inconnu.
pub(crate) fn glab_ci_state(v: &Value) -> String {
    match v["head_pipeline"]["status"]
        .as_str()
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "success" => "success".into(),
        "failed" | "canceled" | "skipped" => "failure".into(),
        "created" | "waiting_for_resource" | "preparing" | "pending" | "running" | "scheduled" => {
            "pending".into()
        }
        _ => String::new(),
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
/// `{status, title, branch, url, review, ci, pr, repo?}` — `pr` est le
/// numéro résolu quand la référence était un Change-Id Gerrit, `repo`
/// le projet résolu par la forge. Transport = `[[forges]]` résolu via
/// `client_for` — `gh` n'est plus requis pour github (`rest` possible).
pub fn live_ref(set: &[Value], t: &RefTarget) -> Option<Value> {
    let kind = proto::forge_kind(&t.forge);
    let id = t.change_id.clone().unwrap_or_else(|| t.pr.to_string());
    let client = client_for(set, kind, &t.host, &t.repo, Path::new("."))?;
    let c = client.get_change(&id)?;
    let mut o = json!({
        "status": c.state,
        "title": c.title,
        "branch": c.branch,
        "url": c.url,
        "review": c.review,
        "ci": c.ci,
        "pr": if c.number > 0 { c.number } else { t.pr },
    });
    if let Some(r) = c.resolved_repo {
        o["repo"] = json!(r);
    }
    // URL absente du payload (ex. gerrit) → reconstruite canonique.
    if o["url"].as_str().unwrap_or("").is_empty() {
        o["url"] = json!(proto::forge_url(
            kind,
            &t.host,
            o["repo"].as_str().unwrap_or(&t.repo),
            o["pr"].as_u64().unwrap_or(t.pr),
            None,
        ));
    }
    Some(o)
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
    /// La référence existe mais c'est la PR de release configurée
    /// (head = branche d'intégration → base = branche de release) :
    /// elle ne doit jamais entrer au registre — elle reste OPEN par
    /// construction et bloquerait la garde « toutes PR soldées » de
    /// l'orchestrateur (deadlock patchnotes vécu : #187 re-registrée à
    /// chaque `gh pr ready` du worker patchnotes).
    Release,
}

/// Existence-check d'une référence avant enregistrement auto (watcher →
/// file de vérif du démon). Le canal = le client du transport résolu
/// (`[[forges]]`) — `gh` n'est plus requis pour github (`rest`
/// possible), hôtes bornés aux déclarés pour toutes les forges.
/// Bloquant — à appeler hors du chemin critique (spawn_blocking /
/// worker dédié).
///
/// `release` = la paire `(head, base)` de la release PR configurée —
/// `Some(("intégration", "release"))` exclut la PR qui match cette
/// forme (`Verify::Release`) ; `None` = workflow « none » : aucune
/// release PR n'est exclue (une PR head=intégration entre alors au
/// registre comme les autres).
pub fn verify_ref(set: &[Value], t: &RefTarget, release: Option<(&str, &str)>) -> Verify {
    let kind = proto::forge_kind(&t.forge);
    let id = t.change_id.clone().unwrap_or_else(|| t.pr.to_string());
    let Some(client) = client_for(set, kind, &t.host, &t.repo, Path::new(".")) else {
        // Hôte déclaré mais transport sans impl → indécidable ; hôte
        // non déclaré → jamais requêté = Missing (le registre ne doit
        // pas devenir un vecteur de requêtes vers un hôte arbitraire).
        return if host_known(set, kind, &t.host) {
            Verify::Unreachable
        } else {
            Verify::Missing
        };
    };
    if !client.available() {
        return Verify::Unreachable;
    }
    match client.verify(&id) {
        Some(false) => Verify::Missing,
        None => Verify::Unreachable,
        Some(true) => {
            // La release PR (head/branche d'intégration → base/branche
            // de release, paire configurée) existe mais reste exclue du
            // registre — cf. Verify::Release. `release=None` (workflow
            // « none ») : rien à exclure.
            if let Some((rh, rb)) = release {
                if let Some(c) = client.get_change(&id) {
                    if c.branch == rh && c.base == rb {
                        return Verify::Release;
                    }
                }
            }
            Verify::Exists
        }
    }
}

/// GitHub n'accepte que `[A-Za-z0-9._-]` par segment d'`owner/repo`
/// — tout autre caractère rend le nom impossible à créer sur la forge.
/// Le garde est aussi une protection d'injection : injecté brut dans
/// l'endpoint `gh api`, `?` tronquerait le chemin (seul le repo serait
/// requêté → faux `Exists`), `{}` déclencherait la substitution depuis
/// le dépôt du cwd (`{owner}`/`{repo}`).
pub(crate) fn github_repo_ok(repo: &str) -> bool {
    repo.split('/').all(|seg| {
        !seg.is_empty()
            && seg.chars().any(|c| c != '.')
            && seg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    })
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
    /// Review forge normalisée (gh_review_state/glab_review_state) —
    /// "" = aucune info (efface la valeur stockée à l'application).
    pub review: String,
    /// CI forge normalisée (gh_ci_state/glab_ci_state) — "" idem.
    pub ci: String,
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

/// Groupe d'entrées partageant le même client : un `list_changes` par
/// (kind, host, repo) — quel que soit la forge, un appel par repo.
struct SyncGroup<'a> {
    kind: &'a str,
    host: String,
    repo: &'a str,
    entries: Vec<&'a Value>,
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
/// la collecte forge est I/O-bound (subprocess CLI, HTTP ureq) : le
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
        .map(|p| p.expect("par_map: empty slot"))
        .collect()
}

/// Interroge les forges live pour chaque entrée du registre et produit
/// les mises à jour à appliquer (aucune écriture ici — le démon track()
/// directement, acctl pousse `pr_track`). `entries` = tableau sérialisé
/// des PrEntry (`forge`, `host`, `repo`, `pr`, `change_id`, `status`).
/// `set` = réponse `forge_list` (types + hôtes + token_env/transport).
///
/// Toutes les forges passent par `client_for` : un client par
/// (kind, host, repo), un `list_changes` par groupe, un `get_change`
/// de repli par entrée hors de la liste tronquée. Les appels tournent
/// en parallèle borné (`SYNC_CONCURRENCY`).
///
/// Dégradations : transport absent (`gh`/`glab` non installé ou non
/// authentifié) → note, entrées inchangées SANS échec (une machine
/// sans CLI de forge ne doit pas passer les entrées « non
/// résolubles ») ; hôte non déclaré ou repo injoignable → échec.
pub fn collect_updates(set: &[Value], entries: &[Value]) -> SyncOutcome {
    let started = Instant::now();
    let mut out = SyncOutcome {
        checked: 0,
        updates: Vec::new(),
        seen: Vec::new(),
        failed: Vec::new(),
        notes: Vec::new(),
    };
    // Groupes (kind, host, repo) dans l'ordre de première occurrence —
    // déterministe et identique pour un registre donné.
    let mut groups: Vec<SyncGroup> = Vec::new();
    for e in entries {
        let kind = proto::forge_kind(e["forge"].as_str().unwrap_or(""));
        let host = e["host"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| proto::forge_default_host(kind).to_string());
        let repo = e["repo"].as_str().unwrap_or("");
        if let Some(g) = groups
            .iter_mut()
            .find(|g| g.kind == kind && g.host == host && g.repo == repo)
        {
            g.entries.push(e);
        } else {
            groups.push(SyncGroup {
                kind,
                host,
                repo,
                entries: vec![e],
            });
        }
    }
    let concurrency = SYNC_CONCURRENCY.min(groups.len());
    let parts = par_map(&groups, &|g| group_sync(set, g));
    for p in parts {
        out.absorb(p);
    }
    tracing::info!(
        groups = groups.len(),
        concurrency,
        checked = out.checked,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "registre PR : collecte forge terminée"
    );
    out
}

/// Sync d'un groupe (kind, host, repo) via le client du transport
/// résolu — l'algo est le même quelle que soit la forge : liste,
/// fallback `get_change`, diff statut/review/ci, untracked ouverts.
fn group_sync(set: &[Value], g: &SyncGroup) -> SyncPartial {
    let mut p = SyncPartial::default();
    let fail_all = |p: &mut SyncPartial| {
        for e in &g.entries {
            p.fail(e);
        }
    };
    let Some(client) = client_for(set, g.kind, &g.host, g.repo, Path::new(".")) else {
        p.notes.push(format!(
            "{} {} {}: undeclared host — statuses unchanged",
            g.kind, g.host, g.repo
        ));
        fail_all(&mut p);
        return p;
    };
    if !client.caps().list {
        // git nu : aucune lecture forge — le statut reste manuel.
        // Pas d'échec d'entrée (sinon `sync_fails` la ferait passer
        // « non résoluble » au bout de 3 passes alors que le statut
        // stocké fait foi).
        p.notes.push(format!(
            "{} {} {}: no automatic read (plain git) — statuses unchanged",
            g.kind, g.host, g.repo
        ));
        return p;
    }
    if !client.available() {
        // Transport CLI absent/non authentifié : jamais un échec
        // d'entrée (cas nominal d'un démon minimal sans gh/glab).
        p.notes.push(format!(
            "{} {} {}: transport unavailable — statuses unchanged",
            g.kind, g.host, g.repo
        ));
        return p;
    }
    let Some(live) = client.list_changes(&ListQuery::all(200)) else {
        // Le transport sert mais le repo ne répond pas →
        // fantôme/privé → compté en échec.
        p.notes.push(format!(
            "{} {} {}: repo missing or unreachable — statuses unchanged",
            g.kind, g.host, g.repo
        ));
        fail_all(&mut p);
        return p;
    };
    let rollup = client.caps().rollup;
    for e in &g.entries {
        let n = e["pr"].as_u64().unwrap_or(0);
        let cid = e["change_id"].as_str();
        let n_str = n.to_string();
        // `--limit` tronque la liste : une entrée ancienne n'y figure
        // plus sans être absente — requête directe avant de la
        // compter injoignable (numéro ou Change-Id Gerrit).
        let c = live
            .iter()
            .find(|c| c.number == n || (cid.is_some() && c.change_id.as_deref() == cid))
            .cloned()
            .or_else(|| client.get_change(cid.unwrap_or(&n_str)));
        let Some(c) =
            c.filter(|c| c.number == n || (cid.is_some() && c.change_id.as_deref() == cid))
        else {
            // Le repo répond mais la référence n'y est plus
            // (supprimée, jamais existée) → non résoluble en cours.
            p.fail(e);
            continue;
        };
        p.checked += 1;
        p.seen.push(seen_of(e));
        let old = e["status"].as_str().unwrap_or("");
        // Review/CI comptent comme changement : une PR déjà à jour en
        // statut peut voir ses checks ou sa review évoluer — sans ça
        // les badges resteraient figés au premier statut vu. Sans
        // rollup (REST github) les valeurs stockées sont conservées.
        let review = if rollup {
            c.review.clone()
        } else {
            e["review"].as_str().unwrap_or("").to_string()
        };
        let ci = if rollup {
            c.ci.clone()
        } else {
            e["ci"].as_str().unwrap_or("").to_string()
        };
        let resolved_repo = c
            .resolved_repo
            .clone()
            .unwrap_or_else(|| g.repo.to_string());
        if c.state != old
            || review != e["review"].as_str().unwrap_or("")
            || ci != e["ci"].as_str().unwrap_or("")
            || c.number != n
            || resolved_repo != g.repo
        {
            p.updates.push(PrUpdate {
                repo: resolved_repo,
                pr: if c.number > 0 { c.number } else { n },
                forge: g.kind.into(),
                host: g.host.clone(),
                change_id: cid.map(String::from).or(c.change_id.clone()),
                url: if c.url.is_empty() {
                    None
                } else {
                    Some(c.url.clone())
                },
                branch: if c.branch.is_empty() {
                    None
                } else {
                    Some(c.branch.clone())
                },
                title: if c.title.is_empty() {
                    None
                } else {
                    Some(c.title.clone())
                },
                status: c.state.clone(),
                old_status: old.into(),
                review,
                ci,
            });
        }
    }
    // Ouverts non enregistrés — le signal « PR ouverte non suivie »
    // existe déjà pour github ; étendu à toutes les forges à listing.
    let open: Vec<u64> = live
        .iter()
        .filter(|c| matches!(c.state.as_str(), "open" | "draft"))
        .map(|c| c.number)
        .filter(|n| *n > 0 && !g.entries.iter().any(|e| e["pr"].as_u64() == Some(*n)))
        .collect();
    if !open.is_empty() {
        p.notes.push(format!(
            "{}: changes ouverts non enregistrés: {}",
            g.repo,
            open.iter()
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
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
        assert_eq!(
            gh_state(&json!({"state": "OPEN", "isDraft": true})),
            "draft"
        );
        assert_eq!(gh_state(&json!({"state": "MERGED"})), "merged");
        assert_eq!(gh_state(&json!({"state": "CLOSED"})), "closed");
        assert_eq!(gh_state(&json!({})), "");
    }

    #[test]
    fn gh_review_state_maps() {
        // reviewDecision tranche en premier.
        assert_eq!(
            gh_review_state(&json!({"reviewDecision": "CHANGES_REQUESTED"})),
            "changes_requested"
        );
        assert_eq!(
            gh_review_state(&json!({"reviewDecision": "APPROVED"})),
            "approved"
        );
        // Requests en attente : plus parlant que REVIEW_REQUIRED.
        assert_eq!(
            gh_review_state(&json!({
                "reviewDecision": "REVIEW_REQUIRED",
                "reviewRequests": [{"login": "octocat"}],
            })),
            "requested"
        );
        assert_eq!(
            gh_review_state(&json!({"reviewDecision": "REVIEW_REQUIRED"})),
            "required"
        );
        // Requests seules (pas de décision) → requested.
        assert_eq!(
            gh_review_state(&json!({"reviewRequests": [{"login": "octocat"}]})),
            "requested"
        );
        assert_eq!(gh_review_state(&json!({})), "");
        assert_eq!(gh_review_state(&json!({"reviewRequests": []})), "");
    }

    #[test]
    fn gh_ci_state_maps() {
        // CheckRun completed + conclusion.
        assert_eq!(
            gh_ci_state(&json!({"statusCheckRollup": [
                {"__typename": "CheckRun", "conclusion": "SUCCESS"},
                {"__typename": "CheckRun", "conclusion": "NEUTRAL"},
            ]})),
            "success"
        );
        // Un échec sans pending → failure.
        assert_eq!(
            gh_ci_state(&json!({"statusCheckRollup": [
                {"conclusion": "SUCCESS"},
                {"conclusion": "FAILURE"},
            ]})),
            "failure"
        );
        // Pending l'emporte même avec un échec (verdict final inconnu).
        assert_eq!(
            gh_ci_state(&json!({"statusCheckRollup": [
                {"conclusion": "FAILURE"},
                {"status": "IN_PROGRESS", "conclusion": null},
            ]})),
            "pending"
        );
        // StatusContext : state porte le verdict.
        assert_eq!(
            gh_ci_state(&json!({"statusCheckRollup": [
                {"__typename": "StatusContext", "state": "SUCCESS"},
            ]})),
            "success"
        );
        assert_eq!(
            gh_ci_state(&json!({"statusCheckRollup": [
                {"__typename": "StatusContext", "state": "PENDING"},
            ]})),
            "pending"
        );
        // Rollup vide / absent → pas d'info.
        assert_eq!(gh_ci_state(&json!({"statusCheckRollup": []})), "");
        assert_eq!(gh_ci_state(&json!({})), "");
        // Que des checks sautés (draft sans CI) → pas de badge.
        assert_eq!(
            gh_ci_state(&json!({"statusCheckRollup": [
                {"conclusion": "SKIPPED"},
                {"conclusion": "NEUTRAL"},
            ]})),
            ""
        );
    }

    #[test]
    fn glab_review_ci_state_maps() {
        assert_eq!(
            glab_review_state(&json!({"reviewers": [{"username": "a"}]})),
            "requested"
        );
        assert_eq!(glab_review_state(&json!({"reviewers": []})), "");
        assert_eq!(glab_review_state(&json!({})), "");
        assert_eq!(
            glab_ci_state(&json!({"head_pipeline": {"status": "running"}})),
            "pending"
        );
        assert_eq!(
            glab_ci_state(&json!({"head_pipeline": {"status": "success"}})),
            "success"
        );
        assert_eq!(
            glab_ci_state(&json!({"head_pipeline": {"status": "failed"}})),
            "failure"
        );
        assert_eq!(glab_ci_state(&json!({"head_pipeline": null})), "");
        assert_eq!(glab_ci_state(&json!({})), "");
    }

    #[test]
    fn glab_state_maps() {
        assert_eq!(glab_state(&json!({"state": "opened"})), "open");
        assert_eq!(
            glab_state(&json!({"state": "opened", "draft": true})),
            "draft"
        );
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
        let out = collect_updates(set.as_array().unwrap(), entries.as_array().unwrap());
        assert_eq!(out.checked, 0);
        assert!(out.updates.is_empty());
        assert_eq!(out.notes.len(), 3);
        assert!(out
            .notes
            .iter()
            .all(|n| n.contains("undeclared") || n.contains("not managed")));
    }
}
