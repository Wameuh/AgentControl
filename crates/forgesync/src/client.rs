//! Trait `ForgeClient` + résolution des transports — le pipeline
//! (orchestrateur, review, registre, sync) n'adresse que des valeurs
//! normalisées (`ChangeInfo`, `ChangeComment`) et ne connaît ni `gh`
//! ni GitHub : une impl par transport, chacune optionnelle et
//! indépendante.
//!
//! Transports :
//! - `github` : `gh` (CLI, défaut `auto`) ou `rest` (API + `token_env`,
//!   repli implicite `GH_TOKEN`)
//! - `gitlab` : `rest` (API v4 + `token_env`, défaut) ou `glab` (CLI
//!   `glab api`) — `mcp` accepté en config, résolu en `rest` : le
//!   serveur MCP GitLab n'apporte rien que REST ne fasse déjà sans
//!   dépendance runtime supplémentaire (voir docs/forges.md)
//! - `gerrit` : `rest` (`/changes/`, lecture anonyme, écriture auth)
//! - `git` : git nu — mode dégradé explicite, aucune capacité forge
//!   (trigger manuel, verdict rapporté localement par le démon)
//!
//! `auto` = le CLI de la forge s'il répond (`gh auth status`, `glab
//! auth status`), `rest` sinon.

mod gerrit;
mod gitbare;
mod github;
mod gitlab;

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use gerrit::GerritRestClient;
pub use gitbare::GitBareClient;
pub use github::{GhCliClient, GhRestClient};
pub use gitlab::{GitlabRestClient, GlabClient};

/// Repo ciblé + contexte local d'un client — `cwd` ne sert qu'aux
/// transports CLI (gh/glab/git résolvent le repo depuis le checkout) ;
/// les clients REST l'ignorent.
#[derive(Debug, Clone)]
pub struct ForgeTarget {
    /// `owner/repo` github, `group/sub/proj` gitlab, nom de projet gerrit.
    pub repo: String,
    pub cwd: PathBuf,
}

/// Change normalisé — PR GitHub, MR GitLab ou change Gerrit. `state`,
/// `review` et `ci` reprennent le vocabulaire du registre (`open`,
/// `draft`, `merged`, `closed` / `requested`, `approved`,
/// `changes_requested`, `required` / `pending`, `success`, `failure`).
#[derive(Debug, Clone, Default)]
pub struct ChangeInfo {
    /// Numéro visible (PR #, MR iid, change _number).
    pub number: u64,
    /// Change-Id Gerrit (`I<sha>`) — None sur les autres forges.
    pub change_id: Option<String>,
    pub title: String,
    pub body: String,
    /// `open` | `draft` | `merged` | `closed`.
    pub state: String,
    /// Branche source (head) — celle qu'une review checkout.
    pub branch: String,
    /// Branche cible (base) — lue par la garde release de verify_ref.
    pub base: String,
    /// SHA du head — la review se pose dessus, sans confiance au
    /// symbole de branche (#313 : `""` → fallback daemon).
    pub head_sha: String,
    pub url: String,
    /// Badge review normalisé — `""` = aucune info.
    pub review: String,
    /// Badge CI normalisé — `""` = aucune info.
    pub ci: String,
    /// Projet résolu par la forge (Gerrit `project` — autorité sur le
    /// repo deviné) ; None = `target.repo` fait foi.
    pub resolved_repo: Option<String>,
}

/// Commentaire/note normalisé — même forme quelle que soit la forge.
#[derive(Debug, Clone)]
pub struct ChangeComment {
    /// Id ordonné du commentaire — le « vu jusqu'à » du scan de
    /// trigger (`id > last_trigger`). Gerrit : index chronologique.
    pub id: u64,
    /// Login affiché (github `user.login`, gitlab `author.username`).
    pub author: String,
    /// Identifiant forge de l'auteur — github = login, gitlab = id
    /// numérique, gerrit = `_account_id` — sert aux lookups d'apparte-
    /// nance (`author_assoc`).
    pub author_id: String,
    /// Association normalisée quand la forge la fournit dans le
    /// payload — vocabulaire github : OWNER | MEMBER | COLLABORATOR |
    /// CONTRIBUTOR | NONE. "" = à résoudre via `author_assoc` (lazy).
    pub assoc: String,
    pub body: String,
    /// Horodatage normalisé `YYYY-MM-DDTHH:MM:SS[Z]` — comparable
    /// lexicalement à `since`.
    pub created_at: String,
    /// Note système (event Gerrit/GitLab, pas un commentaire humain) —
    /// le scan de trigger les ignore.
    pub system: bool,
}

/// Filtre de `list_changes` — `state` = label registre (`open`
/// inclut les drafts), None = tous.
#[derive(Debug, Clone)]
pub struct ListQuery {
    pub state: Option<String>,
    pub limit: usize,
}

impl ListQuery {
    /// Ouverts (drafts inclus) — scan de triggers de review.
    pub fn open(limit: usize) -> Self {
        Self {
            state: Some("open".into()),
            limit,
        }
    }
    /// Tous les états — sync du registre.
    pub fn all(limit: usize) -> Self {
        Self { state: None, limit }
    }
}

/// Capacités d'un transport — lues avant usage pour dégrader
/// proprement (git nu : tout false → pas de list/comment/review auto).
#[derive(Debug, Clone, Copy)]
pub struct ForgeCaps {
    /// `list_changes` supporté.
    pub list: bool,
    /// `list_comments` supporté.
    pub comments: bool,
    /// `post_comment` supporté (auth requise → reflète le token résolu).
    pub post: bool,
    /// `mark_ready` (undraft) supporté.
    pub mark_ready: bool,
    /// `checks_status` utilisable.
    pub checks: bool,
    /// `list_changes`/`get_change` remplissent `review`+`ci` — false
    /// (REST github) → le sync conserve les badges stockés plutôt que
    /// les effacer par une info absente.
    pub rollup: bool,
}

/// Accès forge uniforme — chaque op renvoie `Option`/bool plutôt
/// qu'erreur détaillée : l'appelant dégrade (note de sync, review
/// « failed ») sans distinguer les transports.
pub trait ForgeClient: Send + Sync {
    /// Caps du transport — reflète le token résolu quand pertinent.
    fn caps(&self) -> ForgeCaps;
    /// Transport joignable (`gh`/`glab` installé + authentifié) —
    /// défaut `true` : REST a toujours « un transport », un hôte mort
    /// échoue aux appels eux-mêmes. Sert à distinguer « binaire absent
    /// » (entrées inchangées, pas d'échec) de « repo injoignable ».
    fn available(&self) -> bool {
        true
    }
    /// Liste les changements — None si lecture impossible.
    fn list_changes(&self, q: &ListQuery) -> Option<Vec<ChangeInfo>>;
    /// Lit un change — `id` = numéro décimal, ou Change-Id Gerrit.
    fn get_change(&self, id: &str) -> Option<ChangeInfo>;
    /// Commentaires depuis `since` (horodatage inclusif, ISO) —
    /// None = tous. None retour = lecture impossible.
    fn list_comments(&self, id: &str, since: Option<&str>) -> Option<Vec<ChangeComment>>;
    /// Poste un commentaire/note — false si caps.post == false ou échec.
    fn post_comment(&self, id: &str, body: &str) -> bool;
    /// draft → ready où la forge l'expose (gh `pr ready`, GitLab titre
    /// sans « Draft: », Gerrit `:ready` — écriture, auth requise).
    fn mark_ready(&self, id: &str) -> bool {
        let _ = id;
        false
    }
    /// ready → draft (gh `pr ready --undo`, GitLab préfixe « Draft: »,
    /// Gerrit `:wip`) — la garde release repasse la PR en draft quand
    /// du travail revient.
    fn mark_draft(&self, id: &str) -> bool {
        let _ = id;
        false
    }
    /// Ref distante du head d'un change pour `git fetch` —
    /// `pull/N/head` github, `merge-requests/<iid>/head` gitlab,
    /// `refs/changes/<2>/<num>/<ps>` gerrit. None = pas de ref forge :
    /// le fetch tombe sur le SHA direct (fallback #313).
    fn head_ref(&self, id: &str) -> Option<String> {
        let _ = id;
        None
    }
    /// Association normalisée de l'auteur d'un commentaire —
    /// OWNER | MEMBER | COLLABORATOR | CONTRIBUTOR | NONE. Défaut :
    /// `assoc` du payload ; les forges sans champ (GitLab) résolvent
    /// l'appartenance au projet à la demande (un appel, seulement pour
    /// les notes candidates au trigger).
    fn author_assoc(&self, comment: &ChangeComment) -> String {
        comment.assoc.clone()
    }
    /// Checks du head : pending|success|failure — None = inconnu.
    /// Défaut : `ci` du change quand il est rempli.
    fn checks_status(&self, id: &str) -> Option<String> {
        self.get_change(id).map(|c| c.ci).filter(|s| !s.is_empty())
    }
    /// Existence-check : Some(true) = la forge connaît la référence,
    /// Some(false) = absent définitif (404, 401 sur hôte lu),
    /// None = indécidable (panne, rate-limit, transport absent).
    fn verify(&self, id: &str) -> Option<bool>;
}

/// Transport résolu + token d'un hôte — sortie de `spec_for`.
#[derive(Debug, Clone)]
pub struct ForgeSpec {
    pub kind: String,
    pub host: String,
    /// Transport effectif : `gh` | `rest` | `glab` | `git` — `auto` et
    /// `mcp` sont déjà résolus.
    pub transport: String,
    /// NOM de la variable d'env du token — jamais la valeur.
    pub token_env: Option<String>,
}

/// Valeur du token configuré (var d'env lue à la construction du
/// client) — None si `token_env` absent, non définie ou vide.
pub(crate) fn token_value(env_name: Option<&str>) -> Option<String> {
    env_name
        .and_then(|n| std::env::var(n).ok())
        .filter(|t| !t.is_empty())
}

/// `true` si le binaire répond sur `--version` — probe cheap de
/// présence avant de choisir un transport CLI en `auto`.
pub(crate) fn cli_present(bin: &str) -> bool {
    crate::cli_run(
        bin,
        &["--version"],
        Path::new("."),
        std::time::Duration::from_secs(5),
    )
    .is_some()
}

/// `gh` utilisable sur `host` (binaire + auth) — probe `auto` github.
fn gh_auth_ok(host: &str) -> bool {
    crate::gh_ok(&["auth", "status", "--hostname", host], Path::new("."))
}

/// `glab` utilisable sur `host` — probe `auto` gitlab.
fn glab_auth_ok(host: &str) -> bool {
    cli_present("glab")
        && crate::cli_run(
            "glab",
            &["auth", "status", "--hostname", host],
            Path::new("."),
            crate::CLI_TIMEOUT,
        )
        .is_some()
}

/// Déclaration d'un hôte dans le set `forge_list` — `(connu, transport
/// demandé, token_env)`.
fn declared(set: &[Value], kind: &str, host: &str) -> (bool, Option<String>, Option<String>) {
    for (h, tok, tr) in crate::forge_hosts(set, kind) {
        if h == host {
            return (true, tr.map(str::to_lowercase), tok.map(str::to_string));
        }
    }
    (false, None, None)
}

/// L'hôte est-il déclaré pour ce type (implicites github.com/gitlab.com
/// compris — ils figurent dans `forge_list`).
pub fn host_known(set: &[Value], kind: &str, host: &str) -> bool {
    declared(set, kind, host).0
}

/// Résout le transport effectif d'un hôte : `auto` → CLI si auth OK,
/// REST sinon ; `mcp` → `rest` (spike clos — docs/forges.md) ; gerrit
/// → `rest` ; git → `git`. None = hôte non déclaré ou kind inconnu.
pub fn spec_for(set: &[Value], kind: &str, host: &str) -> Option<ForgeSpec> {
    let (known, tr, tok) = declared(set, kind, host);
    if !known {
        return None;
    }
    let transport = match kind {
        "github" => match tr.as_deref() {
            Some("gh") => "gh",
            Some("rest") => "rest",
            Some("mcp") => "rest",
            Some("auto") | None | Some(_) => {
                if gh_auth_ok(host) {
                    "gh"
                } else {
                    "rest"
                }
            }
        },
        "gitlab" => match tr.as_deref() {
            Some("glab") => "glab",
            Some("gh") => "rest",
            Some("rest") | Some("mcp") => "rest",
            Some("auto") | None | Some(_) => {
                if glab_auth_ok(host) {
                    "glab"
                } else {
                    "rest"
                }
            }
        },
        // REST seul implémenté — `auto`/`gh`/`glab` retombent dessus
        // (lecture anonyme suffit au pipeline).
        "gerrit" => "rest",
        "git" => "git",
        _ => return None,
    };
    Some(ForgeSpec {
        kind: kind.to_string(),
        host: host.to_string(),
        transport: transport.to_string(),
        token_env: tok,
    })
}

/// Client forge résolu pour (kind, host) — None si l'hôte n'est pas
/// déclaré, le kind inconnu, ou le transport sans impl.
pub fn client_for(
    set: &[Value],
    kind: &str,
    host: &str,
    repo: &str,
    cwd: &Path,
) -> Option<Arc<dyn ForgeClient>> {
    let spec = spec_for(set, kind, host)?;
    let target = ForgeTarget {
        repo: repo.to_string(),
        cwd: cwd.to_path_buf(),
    };
    let client: Arc<dyn ForgeClient> = match (spec.kind.as_str(), spec.transport.as_str()) {
        ("github", "gh") => Arc::new(GhCliClient::new(spec, target)),
        ("github", "rest") => Arc::new(GhRestClient::new(spec, target)),
        ("gitlab", "glab") => Arc::new(GlabClient::new(spec, target)),
        ("gitlab", "rest") => Arc::new(GitlabRestClient::new(spec, target)),
        ("gerrit", "rest") => Arc::new(GerritRestClient::new(spec, target)),
        ("git", "git") => Arc::new(GitBareClient::new(spec, target)),
        _ => return None,
    };
    Some(client)
}

/// Horodatage → forme `YYYY-MM-DDTHH:MM:SSZ` comparable à `since` —
/// normalise le format Gerrit (`2024-01-02 03:04:05.000000000`).
pub(crate) fn norm_ts(s: &str) -> String {
    let s = s.split('.').next().unwrap_or(s);
    let s = s.replacen(' ', "T", 1);
    if s.ends_with('Z') {
        s
    } else {
        format!("{s}Z")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn set(entries: &[(&str, &str, Option<&str>, Option<&str>)]) -> Value {
        json!(entries
            .iter()
            .map(|(k, h, tok, tr)| {
                let mut host = json!({"host": h});
                if let Some(t) = tok {
                    host["token_env"] = json!(t);
                }
                if let Some(t) = tr {
                    host["transport"] = json!(t);
                }
                json!({"type": k, "hosts": [host]})
            })
            .collect::<Vec<_>>())
    }

    /// Transport forcé : la valeur déclarée prime, sans probe réseau.
    #[test]
    fn spec_for_honours_declared_transport() {
        let s = set(&[("gitlab", "lab.ex", Some("TOK"), Some("rest"))]);
        let spec = spec_for(s.as_array().unwrap(), "gitlab", "lab.ex").unwrap();
        assert_eq!(spec.transport, "rest");
        assert_eq!(spec.token_env.as_deref(), Some("TOK"));

        let s = set(&[("gitlab", "lab.ex", None, Some("glab"))]);
        assert_eq!(
            spec_for(s.as_array().unwrap(), "gitlab", "lab.ex")
                .unwrap()
                .transport,
            "glab"
        );
        // `mcp` → rest (spike clos, jamais de client MCP).
        let s = set(&[("gitlab", "lab.ex", None, Some("mcp"))]);
        assert_eq!(
            spec_for(s.as_array().unwrap(), "gitlab", "lab.ex")
                .unwrap()
                .transport,
            "rest"
        );
    }

    #[test]
    fn spec_for_covers_all_kinds() {
        let s = set(&[
            ("github", "github.com", None, Some("rest")),
            ("gerrit", "g.ex", None, Some("auto")),
            ("git", "code.ex", None, None),
        ]);
        let arr = s.as_array().unwrap();
        assert_eq!(
            spec_for(arr, "github", "github.com").unwrap().transport,
            "rest"
        );
        // Gerrit : tout transport déclaré retombe sur REST.
        assert_eq!(spec_for(arr, "gerrit", "g.ex").unwrap().transport, "rest");
        assert_eq!(spec_for(arr, "git", "code.ex").unwrap().transport, "git");
        // Hôte non déclaré / kind inconnu → None.
        assert!(spec_for(arr, "github", "gh.ex").is_none());
        assert!(spec_for(arr, "svn", "x").is_none());
        assert!(!host_known(arr, "github", "gh.ex"));
        assert!(host_known(arr, "git", "code.ex"));
    }

    /// `git` et `mcp→rest` résolvent un client ; kind inconnu non.
    #[test]
    fn client_for_routes() {
        let s = set(&[
            ("git", "code.ex", None, None),
            ("gitlab", "lab.ex", None, Some("mcp")),
        ]);
        let arr = s.as_array().unwrap();
        let git = client_for(arr, "git", "code.ex", "p", Path::new(".")).unwrap();
        assert!(!git.caps().list && !git.caps().post);
        let gl = client_for(arr, "gitlab", "lab.ex", "g/p", Path::new("."));
        assert!(gl.is_some());
        assert!(client_for(arr, "svn", "x", "p", Path::new(".")).is_none());
    }

    #[test]
    fn norm_ts_normalizes_gerrit() {
        assert_eq!(
            norm_ts("2024-01-02 03:04:05.123456789"),
            "2024-01-02T03:04:05Z"
        );
        assert_eq!(norm_ts("2024-01-02T03:04:05Z"), "2024-01-02T03:04:05Z");
    }
}
