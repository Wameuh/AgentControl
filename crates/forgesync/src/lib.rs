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

/// Sortie stdout d'une commande `gh` (None si gh absent ou en échec).
/// Publique : acctl s'en sert aussi pour résoudre un repo (`gh repo
/// view`) hors du cycle de sync.
pub fn gh_out(args: &[&str]) -> Option<String> {
    std::process::Command::new("gh")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
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

/// Résultat d'un passage de sync : `updates` à appliquer au registre +
/// `notes` de diagnostics non fatals (hôte injoignable, PRs ouvertes
/// non enregistrées) + `checked` (entrées pour lesquelles un état live
/// a été lu — les hôtes morts ne comptent pas).
pub struct SyncOutcome {
    pub checked: usize,
    pub updates: Vec<PrUpdate>,
    pub notes: Vec<String>,
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
        notes: Vec::new(),
    };
    collect_github(entries, &mut out);
    collect_gitlab(set, entries, &mut out);
    collect_gerrit(set, entries, &mut out);
    out
}

/// GitHub — `gh pr list` groupé par repo connu : les repos comptent
/// comme « vus » dès que gh répond (un hôte down laisse les entrées
/// inchangées, sans note — gh absent est le cas nominal d'un démon
/// minimal).
fn collect_github(entries: &[Value], out: &mut SyncOutcome) {
    let mut repos: Vec<String> = entries
        .iter()
        .filter(|e| proto::forge_kind(e["forge"].as_str().unwrap_or("")) == "github")
        .filter_map(|e| e["repo"].as_str().map(String::from))
        .collect();
    repos.sort();
    repos.dedup();
    for repo in repos {
        let live = gh_out(&[
            "pr", "list", "--repo", &repo, "--state", "all",
            "--limit", "200", "--json",
            "number,title,state,isDraft,headRefName",
        ])
        .and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok());
        let Some(live) = live else {
            out.notes
                .push(format!("github {repo}: gh indisponible — statuts inchangés"));
            continue;
        };
        let tracked: Vec<&Value> = entries
            .iter()
            .filter(|e| {
                proto::forge_kind(e["forge"].as_str().unwrap_or("")) == "github"
                    && e["repo"].as_str() == Some(repo.as_str())
            })
            .collect();
        for e in &tracked {
            let n = e["pr"].as_u64().unwrap_or(0);
            let Some(p) = live.iter().find(|p| p["number"].as_u64() == Some(n)) else {
                continue;
            };
            out.checked += 1;
            let st = gh_state(p);
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
/// API 401 → entrée ignorée proprement.
fn collect_gitlab(set: &[Value], entries: &[Value], out: &mut SyncOutcome) {
    for e in entries {
        if proto::forge_kind(e["forge"].as_str().unwrap_or("")) != "gitlab" {
            continue;
        }
        let host = e["host"].as_str().unwrap_or("gitlab.com");
        let repo = e["repo"].as_str().unwrap_or("");
        let n = e["pr"].as_u64().unwrap_or(0);
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
            continue;
        };
        out.checked += 1;
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

/// Gerrit — REST `GET /changes/<ref>` anonyme (port web de l'hôte
/// configuré). Les entrées adressées par `change_id` résolvent leur
/// numéro au passage (rekey côté registre).
fn collect_gerrit(_set: &[Value], entries: &[Value], out: &mut SyncOutcome) {
    for e in entries {
        if proto::forge_kind(e["forge"].as_str().unwrap_or("")) != "gerrit" {
            continue;
        }
        let host = e["host"].as_str().unwrap_or("");
        if host.is_empty() {
            continue;
        }
        let repo = e["repo"].as_str().unwrap_or("");
        let cid = e["change_id"].as_str();
        let n = e["pr"].as_u64().unwrap_or(0);
        let n_str = n.to_string();
        let q = cid.unwrap_or(&n_str);
        let base = host_base(host);
        let url = format!("{base}/changes/{q}");
        let Some(v) = http_json(&url, &[]) else {
            out.notes.push(format!(
                "gerrit {host} {q}: indisponible — statut inchangé"
            ));
            continue;
        };
        out.checked += 1;
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
