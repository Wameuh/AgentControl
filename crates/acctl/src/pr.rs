//! `acctl pr` — registre review ↔ session multi-forges du démon
//! (prs.json) : ls / add / note / rm / sync sur GitHub, GitLab et
//! Gerrit. Le démon détecte les URLs dans les sorties outils ; acctl
//! sert l'upsert manuel et la synchronisation des statuts (`gh` pour
//! github, REST pour les autres forges — dégradation propre par hôte
//! injoignable).

use crate::Conn;
use anyhow::{Context, Result};
use serde_json::{json, Value};

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

/// Découpe une valeur `--repo` : URL complète → (host, repo) ; forme
/// `host:path` (le `host:` d'affichage, copiable tel quel — cf.
/// `repo_label`) → idem ; chemin nu → repo seul. Retourne
/// (repo, host optionnel).
fn split_repo_arg(arg: &str) -> (String, Option<String>) {
    if let Some(rest) = arg
        .strip_prefix("https://")
        .or_else(|| arg.strip_prefix("http://"))
    {
        if let Some((h, p)) = rest.split_once('/') {
            let host = h.to_lowercase();
            let mut repo = p
                .split("/-/merge_requests/")
                .next()
                .unwrap_or(p)
                .split("/pull/")
                .next()
                .unwrap_or(p)
                .split("/-/issues/")
                .next()
                .unwrap_or(p)
                .trim_end_matches('/')
                .to_string();
            // Formes gerrit en dernier recours uniquement : une URL
            // GitLab peut avoir un groupe « c » ou « pull » légitime.
            if !p.contains("/-/merge_requests/") && !p.contains("/pull/") {
                repo = repo
                    .split("/+/")
                    .next()
                    .unwrap_or(&repo)
                    .trim_start_matches("c/")
                    .to_string();
            }
            return (repo, Some(host));
        }
        return (arg.to_string(), None);
    }
    if let Some((h, p)) = arg.split_once(':') {
        // `host:path` ou `host:port:path` (libellé d'affichage copié —
        // cf. `repo_label`) — un hôte ressemble à un domaine (point)
        // et un 2e segment purement numérique est un port, pas un
        // repo. Le path est déjà le repo : aucun trim d'URL ici (un
        // groupe « c » gitlab resterait sinon écrêté).
        if h.contains('.') {
            return match p.split_once(':') {
                Some((port, rest))
                    if !port.is_empty()
                        && port.chars().all(|c| c.is_ascii_digit())
                        && !rest.is_empty() =>
                {
                    (rest.to_string(), Some(format!("{h}:{port}").to_lowercase()))
                }
                _ => (p.to_string(), Some(h.to_lowercase())),
            };
        }
    }
    (arg.to_string(), None)
}

/// Sélecteur explicite d'une entrée du registre pour `rm`/`note` :
/// chaque champ Some = filtre (forge, hôte, repo insensible à la casse).
/// À la différence de `detect_target`, rien n'est DEVINÉ ici — le
/// remote n'intervient qu'en préférence quand le numéro seul matche
/// plusieurs entrées.
#[derive(Default)]
struct PrSel {
    pr: u64,
    change_id: Option<String>,
    forge: Option<String>,
    host: Option<String>,
    repo: Option<String>,
}

/// Lit le sélecteur de `pr rm|note <N|I<cid>> [--forge F] [--host H]
/// [--repo R]` — les flags sont des filtres, pas une re-clé.
fn select_ref(args: &[String], from: usize) -> Result<PrSel> {
    let arg = positional_at(args, from)
        .map(|i| args[i].clone())
        .context("pr <rm|note> <N|I<Change-Id>> [--forge F] [--host H] [--repo R]")?;
    let (pr, change_id) = parse_ref_arg(&arg)?;
    let (repo, host_from_repo) = flag_val(args, "--repo")
        .map(split_repo_arg)
        .map(|(r, h)| (Some(r), h))
        .unwrap_or_default();
    Ok(PrSel {
        pr,
        change_id,
        forge: flag_val(args, "--forge").map(|s| s.to_lowercase()),
        host: flag_val(args, "--host")
            .map(|s| s.to_lowercase())
            .or(host_from_repo),
        repo,
    })
}

/// Match des champs d'identité d'une entrée du registre (défauts
/// legacy : forge « github », hôte de la forge).
fn entry_matches(e: &Value, sel: &PrSel) -> bool {
    let e_forge = proto::forge_kind(e["forge"].as_str().unwrap_or(""));
    let e_host = e["host"]
        .as_str()
        .unwrap_or_else(|| proto::forge_default_host(e_forge));
    let e_repo = e["repo"].as_str().unwrap_or("");
    if let Some(cid) = sel.change_id.as_deref() {
        if !e["change_id"]
            .as_str()
            .is_some_and(|c| c.eq_ignore_ascii_case(cid))
        {
            return false;
        }
    } else if e["pr"].as_u64() != Some(sel.pr) {
        return false;
    }
    if let Some(f) = &sel.forge {
        if !e_forge.eq_ignore_ascii_case(f) {
            return false;
        }
    }
    if let Some(h) = &sel.host {
        if !e_host.eq_ignore_ascii_case(h) {
            return false;
        }
    }
    if let Some(r) = &sel.repo {
        if !e_repo.eq_ignore_ascii_case(r) {
            return false;
        }
    }
    true
}

/// Libellé court d'une entrée candidate (message d'ambiguïté).
fn entry_label(e: &Value) -> String {
    format!("{} {} {}", badge_of(e), repo_label(e), e["session"].as_str().unwrap_or(""))
}

/// Résout le sélecteur dans le registre listé : numéro/Change-Id +
/// filtres explicites ; sans repo explicite, le remote du cwd sert de
/// préférence (`rm 42` lancé dans le clone = la PR du clone). Ambigu →
/// erreur listant les candidates ; aucun candidat → Ok(None).
fn resolve_in(entries: &[Value], sel: &PrSel) -> Result<Option<Value>> {
    let mut cands: Vec<&Value> = entries.iter().filter(|e| entry_matches(e, sel)).collect();
    if cands.len() > 1 && sel.repo.is_none() {
        // Préférence remote : un `rm <N>` nu dans un clone cible le repo
        // du clone (comportement historique github).
        if let Some((rh, rp)) = git_remote() {
            let pref: Vec<&Value> = cands
                .iter()
                .filter(|e| {
                    e["repo"].as_str().is_some_and(|r| r.eq_ignore_ascii_case(&rp))
                        && {
                            let f = proto::forge_kind(e["forge"].as_str().unwrap_or(""));
                            match f {
                                "github" => rh == "github.com",
                                "gitlab" => rh != "github.com",
                                _ => true,
                            }
                        }
                })
                .copied()
                .collect();
            if !pref.is_empty() {
                cands = pref;
            }
        }
    }
    match cands.len() {
        0 => Ok(None),
        1 => Ok(Some(cands[0].clone())),
        _ => anyhow::bail!(
            "{} entrées correspondent — précisez --forge/--host/--repo :\n  {}",
            cands.len(),
            cands.iter().map(|e| entry_label(e)).collect::<Vec<_>>().join("\n  ")
        ),
    }
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

/// Résout la cible forge/host/repo d'un `pr add` : flags
/// `--forge`/`--host`/`--repo` > remote git matché sur les forges
/// configurées (hostname seul comparé — ssh :29418 ≠ web :8080) >
/// gh repo view (github) — comportement historique. `rm`/`note` ne
/// passent pas par ici : ils résolvent le sélecteur dans le registre
/// (`select_ref` + `resolve_in`).
async fn detect_target(args: &[String], c: &mut Conn, from: usize) -> Result<PrTarget> {
    let arg = positional_at(args, from)
        .map(|i| args[i].clone())
        .context("pr add <N|I<Change-Id>>")?;
    let (pr, change_id) = parse_ref_arg(&arg)?;
    let forge_flag = flag_val(args, "--forge").map(|s| s.to_lowercase());
    let host_flag = flag_val(args, "--host").map(|s| s.to_lowercase());
    let repo_flag = flag_val(args, "--repo").map(String::from);
    let set = forge_set(c).await;

    // `--repo` en URL ou `host:path` donne host+path (lien PR/MR/change
    // ou libellé d'affichage collé).
    let (mut repo, host_from_url) = repo_flag
        .as_deref()
        .map(split_repo_arg)
        .unwrap_or_default();

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
                forgesync::gh_out(&["repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner"])
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

/// Libellé d'une update sync pour la sortie CLI : `#N` github,
/// `{host}:{repo}!{n}` gitlab, `gerrit {host} {ref}` gerrit.
fn update_label(u: &forgesync::PrUpdate) -> String {
    match u.forge.as_str() {
        "gitlab" => format!("{}:{}!{}", u.host, u.repo, u.pr),
        "gerrit" => format!(
            "gerrit {} {}",
            u.host,
            u.change_id.as_deref().unwrap_or(&u.pr.to_string())
        ),
        _ => format!("#{}", u.pr),
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
                // Entrée jamais résolue par le sync → état dédié, pas
                // un « open » trompeur éternel (les statuts finaux
                // merged/closed gardent leur vérité).
                let st = if !matches!(st, "merged" | "closed")
                    && e["sync_fails"].as_u64().unwrap_or(0)
                        >= proto::PR_UNRESOLVED_AFTER as u64
                {
                    "unresolved"
                } else {
                    st
                };
                let who = session_label(&sessions, e["session"].as_str().unwrap_or(""));
                let mine = me.as_deref() == e["session"].as_str();
                println!(
                    "{:<12} {:<24} [{:<10}] {:<28} {}{}{}",
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
            let info = forgesync::live_ref(
                &set,
                &forgesync::RefTarget {
                    forge: t.forge.clone(),
                    host: t.host.clone(),
                    repo: t.repo.clone(),
                    pr: t.pr,
                    change_id: t.change_id.clone(),
                },
            );
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
                    // live_ref a répondu : l'entrée est résoluble —
                    // une fiche « non résoluble » reprend un compteur
                    // propre au lieu d'attendre le prochain sync.
                    sync_fails: info.as_ref().map(|_| 0),
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
            let sel = select_ref(args, 2)?;
            // Le texte suit la référence et s'arrête au premier flag.
            let words: Vec<String> = args[idx + 1..]
                .iter()
                .take_while(|a| !a.starts_with("--"))
                .cloned()
                .collect();
            anyhow::ensure!(!words.is_empty(), "pr note <N|I<cid>> <txt…> — texte vide");
            let v = c
                .call(proto::ReqKind::PrList { session: None })
                .await
                .context("registre illisible (démon trop ancien pour pr_list ?)")?;
            let entries = v.as_array().cloned().unwrap_or_default();
            let e = resolve_in(&entries, &sel)?
                .context("aucune entrée ne correspond (cf. acctl pr ls --all)")?;
            c.call(proto::ReqKind::PrTrack {
                session: None,
                repo: e["repo"].as_str().unwrap_or("").to_string(),
                pr: e["pr"].as_u64().unwrap_or(0),
                forge: e["forge"].as_str().map(String::from),
                host: e["host"].as_str().map(String::from),
                change_id: e["change_id"].as_str().map(String::from),
                url: None,
                branch: None,
                title: None,
                status: None,
                note: Some(words.join(" ")),
                sync_fails: None,
            })
            .await?;
            println!("{}: note maj", badge_of(&e));
        }
        "rm" => {
            let sel = select_ref(args, 2)?;
            // Résolution multi-forges : la cible est cherchée dans le
            // registre (clé complète forge+host+repo+numéro), pas
            // reconstruite par défaut github — `rm <N>` trouve les MR
            // GitLab et changes Gerrit comme les PR GitHub.
            let v = c
                .call(proto::ReqKind::PrList { session: None })
                .await
                .context("registre illisible (démon trop ancien pour pr_list ?)")?;
            let entries = v.as_array().cloned().unwrap_or_default();
            match resolve_in(&entries, &sel)? {
                Some(e) => {
                    println!(
                        "{}",
                        c.call(proto::ReqKind::PrUntrack {
                            repo: e["repo"].as_str().unwrap_or("").to_string(),
                            pr: e["pr"].as_u64().unwrap_or(0),
                            forge: e["forge"].as_str().map(String::from),
                            host: e["host"].as_str().map(String::from),
                            change_id: e["change_id"].as_str().map(String::from),
                        })
                        .await?
                    );
                }
                // Vieux démon sans pr_list ou entrée absente : réponse
                // identique à un untrack qui n'a rien trouvé.
                None => println!("{}", json!({ "removed": false })),
            }
        }
        "sync" => {
            // Rafraîchit les statuts par forge (gh pour github,
            // REST pour gitlab/gerrit — dégradation propre par
            // hôte injoignable). Collecte partagée avec le pr_sync du
            // démon (crate forgesync) ; ici les updates repartent en
            // pr_track puisque acctl est un client du registre.
            let set = forge_set(c).await;
            let v = c.call(proto::ReqKind::PrList { session: None }).await?;
            let entries: Vec<Value> =
                v.as_array().map(|a| a.clone()).unwrap_or_default();
            let out = forgesync::collect_updates(&set, &entries);
            // Compteur d'échecs du registre : vu live → 0, injoignable
            // → +1 (les updates sont des « vues » — Some(0) en même
            // temps que le statut). Champ ignoré par un vieux démon.
            let cur_fails = |s: &forgesync::PrSeen| -> u32 {
                entries
                    .iter()
                    .find(|e| entry_matches(e, &PrSel {
                        pr: s.pr,
                        change_id: s.change_id.clone(),
                        forge: Some(s.forge.clone()),
                        host: Some(s.host.clone()),
                        repo: Some(s.repo.clone()),
                    }))
                    .and_then(|e| e["sync_fails"].as_u64())
                    .unwrap_or(0) as u32
            };
            let track_fails = |repo: &str, pr: u64, forge: &str, host: &str,
                              change_id: Option<String>, fails: u32| {
                proto::ReqKind::PrTrack {
                    session: None,
                    repo: repo.to_string(),
                    pr,
                    forge: Some(forge.to_string()),
                    host: Some(host.to_string()),
                    change_id,
                    url: None,
                    branch: None,
                    title: None,
                    status: None,
                    note: None,
                    sync_fails: Some(fails),
                }
            };
            for u in &out.updates {
                let _ = c
                    .call(proto::ReqKind::PrTrack {
                        session: None,
                        repo: u.repo.clone(),
                        pr: u.pr,
                        forge: Some(u.forge.clone()),
                        host: Some(u.host.clone()),
                        change_id: u.change_id.clone(),
                        url: u.url.clone(),
                        branch: u.branch.clone(),
                        title: u.title.clone(),
                        status: Some(u.status.clone()),
                        note: None,
                        sync_fails: Some(0),
                    })
                    .await;
                println!("{}: {} -> {}", update_label(u), u.old_status, u.status);
            }
            // Vues live sans changement de statut (les updates ont déjà
            // repoussé leur remise à zéro ci-dessus). Pas de pr_track
            // no-op : il bumperait updated_at et réécrirait prs.json
            // pour chaque entrée à chaque sync.
            for s in &out.seen {
                if out.updates.iter().any(|u| {
                    u.forge == s.forge && u.host == s.host
                        && u.repo.eq_ignore_ascii_case(&s.repo) && u.pr == s.pr
                }) {
                    continue;
                }
                if cur_fails(s) == 0 {
                    continue;
                }
                let _ = c
                    .call(track_fails(&s.repo, s.pr, &s.forge, &s.host, s.change_id.clone(), 0))
                    .await;
            }
            for s in &out.failed {
                let nf = cur_fails(s).saturating_add(1);
                let _ = c
                    .call(track_fails(&s.repo, s.pr, &s.forge, &s.host, s.change_id.clone(), nf))
                    .await;
                if nf == proto::PR_UNRESOLVED_AFTER {
                    let mark = match s.forge.as_str() {
                        "gitlab" => "!",
                        "gerrit" => "c#",
                        _ => "#",
                    };
                    println!(
                        "{}:{}{}{}: non résoluble après {} échecs de sync",
                        s.host, s.repo, mark, s.pr, nf
                    );
                }
            }
            for n in &out.notes {
                println!("{n}");
            }
            println!("sync ok ({} vérifiées, {} maj)", out.checked, out.updates.len());
        }
        other => anyhow::bail!("pr: sous-commande inconnue: {other} (add|ls|note|rm|sync)"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Entrée de registre factice (champs d'identité seulement).
    fn ent(forge: &str, host: &str, repo: &str, pr: u64) -> Value {
        json!({
            "forge": forge, "host": host, "repo": repo, "pr": pr,
            "session": "s-x", "status": "open",
        })
    }

    fn sel(pr: u64) -> PrSel {
        PrSel { pr, ..Default::default() }
    }

    #[test]
    fn split_repo_arg_forms() {
        assert_eq!(
            split_repo_arg("grp/sub/proj"),
            ("grp/sub/proj".into(), None)
        );
        assert_eq!(
            split_repo_arg("gitlab.com:grp/sub/proj"),
            ("grp/sub/proj".into(), Some("gitlab.com".into()))
        );
        // Port conservé dans l'hôte, pas dans le repo.
        assert_eq!(
            split_repo_arg("gerrit.corp:8080:core/app"),
            ("core/app".into(), Some("gerrit.corp:8080".into()))
        );
        // URL complète : suffixe MR/pull/gerrit strippé.
        assert_eq!(
            split_repo_arg("https://gitlab.com/g/p/-/merge_requests/42"),
            ("g/p".into(), Some("gitlab.com".into()))
        );
        assert_eq!(
            split_repo_arg("https://github.com/o/r/pull/9"),
            ("o/r".into(), Some("github.com".into()))
        );
        assert_eq!(
            split_repo_arg("https://gerrit.corp/c/core/app/+/7"),
            ("core/app".into(), Some("gerrit.corp".into()))
        );
    }

    #[test]
    fn resolve_gitlab_by_number() {
        // Le cas rapporté : `rm <N>` nu doit trouver une MR GitLab —
        // et retourner l'identité complète pour la clé exacte.
        let entries = vec![
            ent("gitlab", "gitlab.com", "grp/sub/proj", 42),
            ent("github", "github.com", "acme/widgets", 9),
        ];
        let hit = resolve_in(&entries, &sel(42)).unwrap().unwrap();
        assert_eq!(hit["forge"], "gitlab");
        assert_eq!(hit["repo"], "grp/sub/proj");
        // Absente → None (→ removed:false), pas d'erreur.
        assert!(resolve_in(&entries, &sel(77)).unwrap().is_none());
    }

    #[test]
    fn resolve_ambiguous() {
        // Deux entrées de même numéro sur des forges distinctes et
        // aucune préférence remote applicable (repo différent du cwd
        // de toute façon — git_remote() lit le vrai clone de test).
        let entries = vec![
            ent("gitlab", "gitlab.com", "x/one", 42),
            ent("gitlab", "gitlab.corp", "y/two", 42),
        ];
        match resolve_in(&entries, &sel(42)) {
            Err(e) => assert!(e.to_string().contains("ambigu") || e.to_string().contains("entrées")),
            _ => panic!("ambiguïté attendue"),
        }
        // --forge lève l'ambiguïté.
        let s = PrSel { pr: 42, forge: Some("gitlab".into()), host: Some("gitlab.corp".into()), ..Default::default() };
        assert_eq!(resolve_in(&entries, &s).unwrap().unwrap()["host"], "gitlab.corp");
        // --repo aussi (insensible à la casse).
        let s = PrSel { pr: 42, repo: Some("Y/TWO".into()), ..Default::default() };
        assert_eq!(resolve_in(&entries, &s).unwrap().unwrap()["host"], "gitlab.corp");
        // --repo host:path copié depuis l'affichage.
        let s = PrSel { pr: 42, repo: Some("two".into()), ..Default::default() };
        assert!(resolve_in(&entries, &s).unwrap().is_none());
    }

    #[test]
    fn resolve_gerrit_by_change_id() {
        let cid = "Ideadbeefcafe0123456789abcdef0123456789ab";
        let mut e = ent("gerrit", "gerrit.corp:8080", "", 0);
        e["change_id"] = json!(cid);
        let entries = vec![e];
        let s = PrSel { pr: 0, change_id: Some(cid.to_uppercase()), ..Default::default() };
        assert!(resolve_in(&entries, &s).unwrap().is_some());
    }
}
