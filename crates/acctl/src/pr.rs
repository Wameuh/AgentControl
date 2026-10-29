//! `acctl pr` — registre review ↔ session multi-forges du démon
//! (prs.json) : ls / add / note / rm / sync sur GitHub, GitLab et
//! Gerrit. Le démon détecte les URLs dans les sorties outils ; acctl
//! sert l'upsert manuel et la synchronisation des statuts (`gh` pour
//! github, REST pour les autres forges — dégradation propre par hôte
//! injoignable).

use crate::Conn;
use anyhow::{Context, Result};
use serde_json::{json, Value};

/// Sortie stdout d'une commande `gh` (None si gh absent ou en échec).
fn gh_out(args: &[&str]) -> Option<String> {
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
        b.trim_start_matches(|c| c == '\n' || c == '\'')
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

/// Valeur d'un flag `--xxx` (None si absent).
fn flag_val<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).map(String::as_str))
}

/// Cible d'une référence de review pour `pr add|note|rm` : forge + hôte
/// + chemin de projet ("" si adressage par Change-Id seul).
struct PrTarget {
    forge: String,
    host: String,
    repo: String,
    /// `I<40 hex>` quand l'argument n'est pas numérique.
    change_id: Option<String>,
    pr: u64,
}

/// `I<40 hex>` → Change-Id Gerrit, sinon numéro de référence.
fn parse_ref_arg(s: &str) -> Result<(u64, Option<String>)> {
    if s.len() == 41
        && s.starts_with('I')
        && s[1..].chars().all(|c| c.is_ascii_hexdigit())
    {
        return Ok((0, Some(s.to_string())));
    }
    Ok((s.parse().context("numéro de référence invalide (ou I<Change-Id>)")?, None))
}

/// Forges actives du démon (`forge_list`) — [] sur un vieux démon sans
/// le RPC : tout retombe sur les défauts github.
async fn forge_set(c: &mut Conn) -> Vec<Value> {
    c.call(proto::ReqKind::ForgeList {})
        .await
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

/// Hôtes déclarés pour un type de forge, avec leur `token_env`.
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

/// Hostname d'une entrée de forge (`host[:port][/prefix]` → `host`).
fn host_part(h: &str) -> &str {
    h.split(':').next().unwrap_or(h).split('/').next().unwrap_or(h)
}

/// Remote git du cwd : (hostname sans port, path sans .git). Formes
/// `git@h:p`, `ssh://h[:port]/p`, `https://h/p`.
fn git_remote() -> Option<(String, String)> {
    let out = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())?;
    // user@ est strippé pour TOUTES les formes (ssh://git@gerrit:29418/p
    // est le remote canonique de git review).
    let userless = |h: &str| -> String {
        h.rsplit('@').next().unwrap_or(h).split(':').next().unwrap_or(h).to_lowercase()
    };
    let (host, path) = if let Some(rest) = out.strip_prefix("ssh://") {
        let (h, p) = rest.split_once('/')?;
        (userless(h), p.to_string())
    } else if let Some(rest) = out
        .strip_prefix("https://")
        .or_else(|| out.strip_prefix("http://"))
    {
        let (h, p) = rest.split_once('/')?;
        (userless(h), p.to_string())
    } else {
        // scp-like : [user@]host:path
        let (h, p) = out.rsplit_once(':').or_else(|| out.split_once(':'))?;
        (userless(h), p.to_string())
    };
    let path = path.trim_start_matches('/').trim_end_matches(".git").to_string();
    (!host.is_empty() && !path.is_empty()).then_some((host, path))
}

/// Index du premier argument positionnel après `args[from..]` — saute
/// les valeurs des flags à paramètre (`--repo X` n'est pas un positionnel).
fn positional_at(args: &[String], from: usize) -> Option<usize> {
    const VAL_FLAGS: [&str; 5] = ["--session", "--repo", "--note", "--forge", "--host"];
    let mut skip = false;
    for (i, a) in args.iter().enumerate().skip(from) {
        if skip {
            skip = false;
            continue;
        }
        if a.starts_with("--") {
            if VAL_FLAGS.contains(&a.as_str()) {
                skip = true;
            }
            continue;
        }
        return Some(i);
    }
    None
}

/// Résout la cible forge/host/repo d'un `pr add|note|rm` :
/// flags `--forge`/`--host`/`--repo` > remote git matché sur les forges
/// configurées (hostname seul comparé — ssh :29418 ≠ web :8080) >
/// gh repo view (github) — comportement historique.
async fn detect_target(args: &[String], c: &mut Conn, from: usize) -> Result<PrTarget> {
    let arg = positional_at(args, from)
        .map(|i| args[i].clone())
        .context("pr <add|note|rm> <N|I<Change-Id>>")?;
    let (pr, change_id) = parse_ref_arg(&arg)?;
    let forge_flag = flag_val(args, "--forge").map(|s| s.to_lowercase());
    let host_flag = flag_val(args, "--host").map(|s| s.to_lowercase());
    let repo_flag = flag_val(args, "--repo").map(String::from);
    let set = forge_set(c).await;

    // `--repo` en URL complète donne host+path (lien PR/MR/change collé).
    let mut repo = repo_flag.clone().unwrap_or_default();
    let mut host_from_url: Option<String> = None;
    if repo.contains("://") {
        if let Some(rest) = repo
            .strip_prefix("https://")
            .or_else(|| repo.strip_prefix("http://"))
            .map(String::from)
        {
            if let Some((h, p)) = rest.split_once('/') {
                host_from_url = Some(h.to_lowercase());
                repo = p
                    .split("/-/merge_requests/")
                    .next()
                    .unwrap_or(p)
                    .split("/pull/")
                    .next()
                    .unwrap_or(p)
                    .trim_end_matches('/')
                    .to_string();
                // Formes gerrit en dernier recours uniquement : une URL
                // GitLab peut avoir un groupe « c » ou « pull » légitime
                // (`gitlab.com/c/foo/-/merge_requests/1` → repo c/foo).
                if !p.contains("/-/merge_requests/") && !p.contains("/pull/") {
                    repo = repo
                        .split("/+/")
                        .next()
                        .unwrap_or(&repo)
                        .trim_start_matches("c/")
                        .to_string();
                }
            }
        }
    }

    // Match du hostname du remote contre les hôtes configurés →
    // (type, hôte web déclaré). Couvre ssh≠web port.
    let remote = git_remote();
    let mut matched: Option<(String, String)> = None;
    if let Some((rh, _)) = &remote {
        for kind in ["gitlab", "gerrit", "github"] {
            if let Some((fh, _)) = forge_hosts(&set, kind)
                .into_iter()
                .find(|(fh, _)| host_part(fh) == rh)
            {
                matched = Some((kind.to_string(), fh.to_string()));
                break;
            }
        }
    }
    let forge = forge_flag
        .or_else(|| matched.as_ref().map(|m| m.0.clone()))
        .unwrap_or_else(|| "github".into());
    let mut host = host_flag
        .or(host_from_url)
        .or_else(|| {
            // Un hôte matché ne s'applique que s'il est DE la forge
            // résolue — `matched` github + `--forge gitlab` ≠ hôte.
            matched
                .as_ref()
                .filter(|m| m.0 == forge)
                .map(|m| m.1.clone())
        });
    if repo.is_empty() && change_id.is_none() {
        if let Some((_, p)) = &remote {
            // Gerrit : le préfixe « a/ » d'auth http n'est pas du path.
            repo = p.trim_start_matches("a/").to_string();
        }
    }
    // Sans hôte connu → celui du remote est la meilleure estimation,
    // sauf quand il appartient visiblement à une autre forge (un
    // remote github.com n'est pas un hôte gitlab — on garde alors le
    // défaut de la forge). Port web différent → préciser --host.
    if host.is_none() {
        if let Some((rh, _)) = &remote {
            let ok = match forge.as_str() {
                "github" => rh == "github.com",
                "gerrit" => rh != "github.com" && rh != "gitlab.com",
                _ => rh != "github.com",
            };
            if ok {
                host = Some(rh.clone());
            }
        }
    }
    let host = host.unwrap_or_else(|| proto::forge_default_host(&forge).to_string());
    if repo.is_empty() && change_id.is_none() {
        repo = repo_flag.or_else(|| {
            if forge == "github" {
                gh_out(&["repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner"])
            } else {
                None
            }
        })
        .context("repo introuvable — passez --repo <path> [--forge F] [--host H] (ou lancez depuis le clone)")?;
    }
    anyhow::ensure!(
        matches!(forge.as_str(), "github" | "gitlab" | "gerrit"),
        "--forge github|gitlab|gerrit"
    );
    anyhow::ensure!(!host.is_empty(), "--host requis pour la forge {forge}");
    Ok(PrTarget {
        forge,
        host,
        repo,
        change_id,
        pr,
    })
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

/// En-têtes d'auth d'une forge : `token_env` est lu dans l'env de
/// l'appelant — GitLab `PRIVATE-TOKEN` seulement (Gerrit sync =
/// anonyme : un `token_env` sur type=gerrit est sans effet, documenté).
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

/// Badge compact `#N`/`!N`/`c#N` — partagé avec le client.
pub(crate) fn badge_of(e: &Value) -> String {
    proto::forge_badge(
        e["forge"].as_str().unwrap_or(""),
        e["pr"].as_u64().unwrap_or(0),
        e["change_id"].as_str(),
    )
}

/// Libellé repo d'une entrée : `o/r` github, `host:g/p` ailleurs.
fn repo_label(e: &Value) -> String {
    let repo = e["repo"].as_str().unwrap_or("");
    let host = e["host"].as_str().unwrap_or("");
    let forge = e["forge"].as_str().unwrap_or("");
    match proto::forge_kind(forge) {
        "github" => repo.to_string(),
        _ => {
            if repo.is_empty() {
                host.to_string()
            } else {
                format!("{host}:{repo}")
            }
        }
    }
}

/// Snapshot d'une référence depuis la forge live (None si indisponible).
/// `{status, title, branch, url, pr}` — `pr` est le numéro résolu quand
/// la référence était un Change-Id Gerrit.
fn live_ref(set: &[Value], t: &PrTarget) -> Option<Value> {
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

/// Sync GitHub — chemin `gh pr list` historique par repo connu.
async fn sync_github(c: &mut Conn, entries: &[Value]) {
    let mut repos: Vec<String> = entries
        .iter()
        .filter(|e| {
            proto::forge_kind(e["forge"].as_str().unwrap_or("")) == "github"
        })
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
        .and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok())
        .unwrap_or_default();
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
            let st = gh_state(p);
            if st != e["status"].as_str().unwrap_or("") {
                let _ = c
                    .call(proto::ReqKind::PrTrack {
                        session: None,
                        repo: repo.clone(),
                        pr: n,
                        forge: Some("github".into()),
                        host: Some("github.com".into()),
                        change_id: None,
                        url: None,
                        branch: p["headRefName"].as_str().map(String::from),
                        title: p["title"].as_str().map(String::from),
                        status: Some(st.clone()),
                        note: None,
                    })
                    .await;
                println!("#{n}: {} -> {st}", e["status"].as_str().unwrap_or(""));
            }
        }
        let untracked: Vec<u64> = live
            .iter()
            .filter(|p| p["state"].as_str() == Some("OPEN"))
            .map(|p| p["number"].as_u64().unwrap_or(0))
            .filter(|n| {
                !tracked.iter().any(|e| e["pr"].as_u64() == Some(*n))
            })
            .collect();
        if !untracked.is_empty() {
            println!(
                "{repo}: PRs ouvertes non enregistrées: {}",
                untracked
                    .iter()
                    .map(|n| format!("#{n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
}

/// Sync GitLab — REST `/api/v4/…/merge_requests/<iid>` par entrée,
/// token `PRIVATE-TOKEN` si `token_env` configuré. Hôte privé sans
/// token → API 401 → entrée ignorée proprement.
async fn sync_gitlab(c: &mut Conn, set: &[Value], entries: &[Value]) {
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
            eprintln!("gitlab {host} {repo}!{n}: indisponible — statut inchangé");
            continue;
        };
        let st = glab_state(&v);
        if st != e["status"].as_str().unwrap_or("") {
            let _ = c
                .call(proto::ReqKind::PrTrack {
                    session: None,
                    repo: repo.to_string(),
                    pr: n,
                    forge: Some("gitlab".into()),
                    host: Some(host.to_string()),
                    change_id: None,
                    url: v["web_url"].as_str().map(String::from),
                    branch: v["source_branch"].as_str().map(String::from),
                    title: v["title"].as_str().map(String::from),
                    status: Some(st.clone()),
                    note: None,
                })
                .await;
            println!("{host}:{repo}!{n}: {} -> {st}", e["status"].as_str().unwrap_or(""));
        }
    }
}

/// Sync Gerrit — REST `GET /changes/<ref>` anonyme (port web de
/// l'hôte configuré). Les entrées adressées par `change_id` résolvent
/// leur numéro au passage (PrTrack rekey côté démon).
async fn sync_gerrit(c: &mut Conn, _set: &[Value], entries: &[Value]) {
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
            eprintln!("gerrit {host} {q}: indisponible — statut inchangé");
            continue;
        };
        let st = gerrit_state(&v);
        let resolved = v["_number"].as_u64().unwrap_or(n);
        if st != e["status"].as_str().unwrap_or("") || resolved != n {
            let _ = c
                .call(proto::ReqKind::PrTrack {
                    session: None,
                    repo: v["project"].as_str().unwrap_or(repo).to_string(),
                    pr: resolved,
                    forge: Some("gerrit".into()),
                    host: Some(host.to_string()),
                    change_id: e["change_id"]
                        .as_str()
                        .map(String::from)
                        .or_else(|| v["change_id"].as_str().map(String::from)),
                    url: v["change_id"].as_str().map(|_| proto::forge_url(
                        "gerrit", host,
                        v["project"].as_str().unwrap_or(repo),
                        resolved,
                        None,
                    )),
                    branch: v["branch"].as_str().map(String::from),
                    title: v["subject"].as_str().map(String::from),
                    status: Some(st.clone()),
                    note: None,
                })
                .await;
            println!("gerrit {host} {q}: {} -> {st}", e["status"].as_str().unwrap_or(""));
        }
    }
}

/// Résout un id de session vers "Titre (agent)" via list_sessions.
fn session_label(sessions: &Value, sid: &str) -> String {
    for s in sessions.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        if s["id"].as_str() == Some(sid) {
            return format!(
                "{} ({})",
                s["title"].as_str().unwrap_or("?"),
                s["agent"].as_str().unwrap_or("?")
            );
        }
    }
    sid.to_string() // session supprimée ou label libre
}

/// `acctl pr` : [ls] [--all] | add <N|I<cid>> | note <N|I<cid>> <txt…>
/// | rm <N|I<cid>> | sync   [--forge F] [--host H] [--repo R].
pub(crate) async fn run(args: &[String], c: &mut Conn) -> Result<()> {
    let sub = args.get(1).map(String::as_str).unwrap_or("ls");
    match sub {
        "ls" | "--all" | "--json" => {
            let all = args.iter().any(|a| a == "--all");
            let js = args.iter().any(|a| a == "--json");
            let v = c
                .call(proto::ReqKind::PrList { session: None })
                .await?;
            if js {
                println!("{v}");
                return Ok(());
            }
            let sessions = c.call(proto::ReqKind::ListSessions).await.unwrap_or(json!([]));
            let me = std::env::var("AGENTCONTROL_SESSION").ok();
            for e in v.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
                let st = e["status"].as_str().unwrap_or("");
                if !all && (st == "merged" || st == "closed") {
                    continue;
                }
                let who = session_label(&sessions, e["session"].as_str().unwrap_or(""));
                let mine = me.as_deref() == e["session"].as_str();
                println!(
                    "{:<12} {:<24} [{:<6}] {:<28} {}{}{}",
                    badge_of(e),
                    repo_label(e),
                    st,
                    if mine { format!("{who} <- toi") } else { who },
                    e["title"].as_str().unwrap_or(""),
                    e["branch"]
                        .as_str()
                        .map(|b| format!("  ({b})"))
                        .unwrap_or_default(),
                    if e["via"].as_str() == Some("detected") { "  [auto]" } else { "" },
                );
                if let Some(n) = e["note"].as_str() {
                    println!("         note: {n}");
                }
            }
        }
        "add" => {
            let t = detect_target(args, c, 2).await?;
            let session = flag_val(args, "--session")
                .map(String::from)
                .or_else(|| std::env::var("AGENTCONTROL_SESSION").ok())
                .context("--session requis hors session agent")?;
            // Métadonnées live par forge (tolère l'échec).
            let set = forge_set(c).await;
            let info = live_ref(&set, &t);
            let note = flag_val(args, "--note").map(String::from);
            let v = c
                .call(proto::ReqKind::PrTrack {
                    session: Some(session),
                    repo: info
                        .as_ref()
                        .and_then(|i| i["repo"].as_str().map(String::from))
                        .unwrap_or_else(|| t.repo.clone()),
                    pr: info
                        .as_ref()
                        .and_then(|i| i["pr"].as_u64())
                        .unwrap_or(t.pr),
                    forge: Some(t.forge.clone()),
                    host: Some(t.host.clone()),
                    change_id: t.change_id.clone(),
                    url: info
                        .as_ref()
                        .and_then(|i| i["url"].as_str().map(String::from)),
                    branch: info
                        .as_ref()
                        .and_then(|i| i["branch"].as_str().map(String::from)),
                    title: info
                        .as_ref()
                        .and_then(|i| i["title"].as_str().map(String::from)),
                    status: info
                        .as_ref()
                        .and_then(|i| i["status"].as_str().map(String::from)),
                    note,
                })
                .await?;
            println!(
                "{} → {} [{}] {}",
                badge_of(&v),
                v["session"].as_str().unwrap_or(""),
                v["status"].as_str().unwrap_or(""),
                v["title"].as_str().unwrap_or("")
            );
        }
        "note" => {
            let idx = positional_at(args, 2).context("pr note <N|I<cid>> <txt…>")?;
            let t = detect_target(args, c, 2).await?;
            // Le texte suit la référence et s'arrête au premier flag.
            let words: Vec<String> = args[idx + 1..]
                .iter()
                .take_while(|a| !a.starts_with("--"))
                .cloned()
                .collect();
            anyhow::ensure!(!words.is_empty(), "pr note <N|I<cid>> <txt…> — texte vide");
            c.call(proto::ReqKind::PrTrack {
                session: None,
                repo: t.repo.clone(),
                pr: t.pr,
                forge: Some(t.forge.clone()),
                host: Some(t.host.clone()),
                change_id: t.change_id.clone(),
                url: None,
                branch: None,
                title: None,
                status: None,
                note: Some(words.join(" ")),
            })
            .await?;
            println!("{}: note maj", badge_of(&json!({"forge": t.forge, "pr": t.pr, "change_id": t.change_id})));
        }
        "rm" => {
            let t = detect_target(args, c, 2).await?;
            println!(
                "{}",
                c.call(proto::ReqKind::PrUntrack {
                    repo: t.repo.clone(),
                    pr: t.pr,
                    forge: Some(t.forge.clone()),
                    host: Some(t.host.clone()),
                    change_id: t.change_id.clone(),
                })
                .await?
            );
        }
        "sync" => {
            // Rafraîchit les statuts par forge (gh pour github,
            // REST pour gitlab/gerrit — dégradation propre par
            // hôte injoignable).
            let set = forge_set(c).await;
            let v = c.call(proto::ReqKind::PrList { session: None }).await?;
            let entries: Vec<Value> =
                v.as_array().map(|a| a.clone()).unwrap_or_default();
            sync_github(c, &entries).await;
            sync_gitlab(c, &set, &entries).await;
            sync_gerrit(c, &set, &entries).await;
            println!("sync ok");
        }
        other => anyhow::bail!("pr: sous-commande inconnue: {other} (add|ls|note|rm|sync)"),
    }
    Ok(())
}
