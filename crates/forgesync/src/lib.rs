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
use std::time::{Duration, Instant};

/// Deadline d'un appel `gh` : un gh wedged (credential helper bloqué,
/// socket morte) ne doit pas retenir un thread `spawn_blocking` du
/// démon indéfiniment — les runs « stuck » sont autorisés à repartir
/// après 180 s côté démon, sans kill le process s'accumulerait.
const GH_TIMEOUT: Duration = Duration::from_secs(20);

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

/// GET JSON avec timeout borné — sync GitLab/Gerrit. Tolérant : toute
/// erreur (réseau, non-2xx, JSON) → None, l'appelant dégrade proprement.
/// Le préfixe anti-XSSI `)]}'` de Gerrit est strippé. Redirects refusés :
/// un en-tête d'auth ne doit jamais suivre vers un autre hôte.
fn http_json(url: &str, headers: &[(&str, String)]) -> Option<Value> {
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(std::time::Duration::from_secs(8))
        .build();
    let mut req = agent.get(url);
    for (k, v) in headers {
        req = req.set(k, v.as_str());
    }
    let body = req.call().ok()?.into_string().ok()?;
    let body = body.strip_prefix(")]}'").map_or(body.as_str(), |b| {
        b.trim_start_matches(|c: char| c == '\n' || c == '\'')
    });
    serde_json::from_str(body).ok()
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

/// Interroge les forges live pour chaque entrée du registre et produit
/// les mises à jour à appliquer (aucune écriture ici — le démon track()
/// directement, acctl pousse `pr_track`). `entries` = tableau sérialisé
/// des PrEntry (`forge`, `host`, `repo`, `pr`, `change_id`, `status`).
/// `set` = réponse `forge_list` (types + hôtes + token_env).
pub fn collect_updates(set: &[Value], entries: &[Value]) -> SyncOutcome {
    let mut out = SyncOutcome {
        checked: 0,
        updates: Vec::new(),
        seen: Vec::new(),
        failed: Vec::new(),
        notes: Vec::new(),
    };
    collect_github(entries, &mut out);
    collect_gitlab(set, entries, &mut out);
    collect_gerrit(set, entries, &mut out);
    out
}

/// Kind github dont l'hôte n'est pas github.com : hors périmètre —
/// `gh` n'interroge que github.com et relibeller l'entrée la
/// re-keyerait sur le mauvais hôte. Hôte absent = github.com (registre
/// historique).
fn gh_host_ok(e: &Value) -> bool {
    e["host"].as_str().unwrap_or("github.com") == "github.com"
}

/// GitHub — `gh pr list` groupé par repo connu : les repos comptent
/// comme « vus » dès que gh répond (un hôte down laisse les entrées
/// inchangées, sans note — gh absent est le cas nominal d'un démon
/// minimal). Un repo dont le `gh pr list` échoue alors que gh est
/// prouvé utilisable (`gh auth status` OK) = repo/référence injoignable
/// → compté en échec ; gh absent ou non authentifié n'est PAS un
/// échec d'entrée (aucune entrée ne doit passer « non résoluble » sur
/// une machine sans gh fonctionnel).
fn collect_github(entries: &[Value], out: &mut SyncOutcome) {
    for e in entries {
        if proto::forge_kind(e["forge"].as_str().unwrap_or("")) == "github" && !gh_host_ok(e) {
            out.notes.push(format!(
                "github {} #{}: hôte {} non géré — statut inchangé",
                e["repo"].as_str().unwrap_or("?"),
                e["pr"].as_u64().unwrap_or(0),
                e["host"].as_str().unwrap_or("?"),
            ));
            // Jamais résolvable par gh (hôte hors github.com) : le
            // compteur reflète l'impossibilité, pas une panne passagère.
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
    for repo in repos {
        let live = gh_out(&[
            "pr", "list", "--repo", &repo, "--state", "all",
            "--limit", "200", "--json",
            "number,title,state,isDraft,headRefName",
        ])
        .and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok());
        let Some(live) = live else {
            if gh_up {
                // gh sert mais ce repo ne répond pas → fantôme/privé.
                out.notes.push(format!(
                    "github {repo}: repo absent ou inaccessible — statuts inchangés"
                ));
                for e in entries.iter().filter(|e| {
                    proto::forge_kind(e["forge"].as_str().unwrap_or("")) == "github"
                        && gh_host_ok(e)
                        && e["repo"].as_str() == Some(repo.as_str())
                }) {
                    out.failed.push(seen_of(e));
                }
            } else {
                out.notes
                    .push(format!("github {repo}: gh indisponible — statuts inchangés"));
            }
            continue;
        };
        let tracked: Vec<&Value> = entries
            .iter()
            .filter(|e| {
                proto::forge_kind(e["forge"].as_str().unwrap_or("")) == "github"
                    && gh_host_ok(e)
                    && e["repo"].as_str() == Some(repo.as_str())
            })
            .collect();
        for e in &tracked {
            let n = e["pr"].as_u64().unwrap_or(0);
            // `--limit 200` tronque la liste : sur un gros repo une
            // entrée ancienne n'y figure plus sans être absente —
            // requête directe avant de la compter injoignable.
            let p = live
                .iter()
                .find(|p| p["number"].as_u64() == Some(n))
                .cloned()
                .or_else(|| {
                    gh_out(&[
                        "pr", "view", &n.to_string(), "--repo", &repo, "--json",
                        "number,title,state,isDraft,headRefName",
                    ])
                    .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                });
            let Some(p) = p.filter(|p| p["number"].as_u64() == Some(n)) else {
                // Le repo répond mais la référence n'y est plus
                // (supprimée, jamais existée) → non résoluble en cours.
                out.failed.push(seen_of(e));
                continue;
            };
            out.checked += 1;
            out.seen.push(seen_of(e));
            let st = gh_state(&p);
            let old = e["status"].as_str().unwrap_or("");
            if st != old {
                out.updates.push(PrUpdate {
                    repo: repo.clone(),
                    pr: n,
                    forge: "github".into(),
                    host: "github.com".into(),
                    change_id: None,
                    url: None,
                    branch: p["headRefName"].as_str().map(String::from),
                    title: p["title"].as_str().map(String::from),
                    status: st,
                    old_status: old.into(),
                });
            }
        }
        let untracked: Vec<u64> = live
            .iter()
            .filter(|p| p["state"].as_str() == Some("OPEN"))
            .map(|p| p["number"].as_u64().unwrap_or(0))
            .filter(|n| !tracked.iter().any(|e| e["pr"].as_u64() == Some(*n)))
            .collect();
        if !untracked.is_empty() {
            out.notes.push(format!(
                "{repo}: PRs ouvertes non enregistrées: {}",
                untracked
                    .iter()
                    .map(|n| format!("#{n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
}

/// GitLab — REST `/api/v4/…/merge_requests/<iid>` par entrée, token
/// `PRIVATE-TOKEN` si `token_env` configuré. Hôte privé sans token →
/// API 401 → entrée ignorée proprement. Hôtes bornés à ceux déclarés
/// (`forge_list` inclut gitlab.com implicitement) — le registre ne doit
/// pas devenir un vecteur de requêtes vers un hôte arbitraire.
fn collect_gitlab(set: &[Value], entries: &[Value], out: &mut SyncOutcome) {
    let declared: Vec<&str> = forge_hosts(set, "gitlab")
        .iter()
        .map(|(h, _)| *h)
        .collect();
    for e in entries {
        if proto::forge_kind(e["forge"].as_str().unwrap_or("")) != "gitlab" {
            continue;
        }
        let host = e["host"].as_str().unwrap_or("gitlab.com");
        let repo = e["repo"].as_str().unwrap_or("");
        let n = e["pr"].as_u64().unwrap_or(0);
        if !declared.contains(&host) {
            out.notes.push(format!(
                "gitlab {host} {repo}!{n}: hôte non déclaré — statut inchangé"
            ));
            // Jamais résolvable tant que l'hôte n'est pas déclaré :
            // le compteur le dit (repo fantôme ou config incomplète).
            out.failed.push(seen_of(e));
            continue;
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
            out.notes.push(format!(
                "gitlab {host} {repo}!{n}: indisponible — statut inchangé"
            ));
            out.failed.push(seen_of(e));
            continue;
        };
        out.checked += 1;
        out.seen.push(seen_of(e));
        let st = glab_state(&v);
        let old = e["status"].as_str().unwrap_or("");
        if st != old {
            out.updates.push(PrUpdate {
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
    }
}

/// Gerrit — REST `GET /changes/<ref>` anonyme, hôtes déclarés seulement
/// (comme l'extraction de refs : rien ne distingue un hôte Gerrit d'un
/// autre service sans déclaration). Les entrées adressées par
/// `change_id` résolvent leur numéro au passage (rekey côté registre).
fn collect_gerrit(set: &[Value], entries: &[Value], out: &mut SyncOutcome) {
    let declared: Vec<&str> = forge_hosts(set, "gerrit")
        .iter()
        .map(|(h, _)| *h)
        .collect();
    for e in entries {
        if proto::forge_kind(e["forge"].as_str().unwrap_or("")) != "gerrit" {
            continue;
        }
        let host = e["host"].as_str().unwrap_or("");
        if host.is_empty() {
            // Entrée sans hôte : jamais résolvable — comptée en échec
            // plutôt qu'ignorée silencieusement.
            out.failed.push(seen_of(e));
            continue;
        }
        let repo = e["repo"].as_str().unwrap_or("");
        let cid = e["change_id"].as_str();
        let n = e["pr"].as_u64().unwrap_or(0);
        let n_str = n.to_string();
        let q = cid.unwrap_or(&n_str);
        if !declared.contains(&host) {
            out.notes.push(format!(
                "gerrit {host} {q}: hôte non déclaré — statut inchangé"
            ));
            out.failed.push(seen_of(e));
            continue;
        }
        let base = host_base(host);
        let url = format!("{base}/changes/{q}");
        let Some(v) = http_json(&url, &[]) else {
            out.notes.push(format!(
                "gerrit {host} {q}: indisponible — statut inchangé"
            ));
            out.failed.push(seen_of(e));
            continue;
        };
        out.checked += 1;
        out.seen.push(seen_of(e));
        let st = gerrit_state(&v);
        let resolved = v["_number"].as_u64().unwrap_or(n);
        let old = e["status"].as_str().unwrap_or("");
        if st != old || resolved != n {
            out.updates.push(PrUpdate {
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
    }
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
