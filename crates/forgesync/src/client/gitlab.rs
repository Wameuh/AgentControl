//! Transports GitLab : REST API v4 (`token_env` → `PRIVATE-TOKEN`,
//! défaut `auto`/`rest`/`mcp`) et `glab` CLI (`glab api` passthrough —
//! même JSON, mêmes mappers).
//!
//! `mcp` n'a pas d'impl : le serveur MCP GitLab n'offre rien que
//! `/api/v4` ne fasse déjà (notes, pipelines, MR) sans dépendance
//! runtime — résolu en `rest` par `spec_for`, décision documentée dans
//! docs/forges.md.

use super::{
    cli_present, norm_ts, token_value, ChangeComment, ChangeInfo, ForgeCaps, ForgeClient,
    ForgeSpec, ForgeTarget, ListQuery,
};
use crate::{
    cli_run, glab_ci_state, glab_review_state, glab_state, host_base, http_get, http_send, urlenc,
    Http, CLI_TIMEOUT,
};
use serde_json::{json, Value};

/// MR JSON (REST ou `glab api`) → ChangeInfo normalisé.
fn mr_change(v: &Value) -> ChangeInfo {
    ChangeInfo {
        number: v["iid"].as_u64().unwrap_or(0),
        title: v["title"].as_str().unwrap_or("").into(),
        body: v["description"].as_str().unwrap_or("").into(),
        state: glab_state(v),
        branch: v["source_branch"].as_str().unwrap_or("").into(),
        base: v["target_branch"].as_str().unwrap_or("").into(),
        head_sha: v["sha"].as_str().unwrap_or("").into(),
        url: v["web_url"].as_str().unwrap_or("").into(),
        review: glab_review_state(v),
        ci: glab_ci_state(v),
        ..Default::default()
    }
}

/// Note GitLab → commentaire normalisé — `system` conservé : les events
/// (« added 1 commit ») ne doivent jamais déclencher une review.
fn mr_note(v: &Value) -> ChangeComment {
    ChangeComment {
        id: v["id"].as_u64().unwrap_or(0),
        author: v["author"]["username"].as_str().unwrap_or("").into(),
        author_id: v["author"]["id"]
            .as_u64()
            .map(|i| i.to_string())
            .unwrap_or_default(),
        // Pas d'équivalent `author_association` dans le payload note —
        // résolu à la demande via `author_assoc` (members API).
        assoc: String::new(),
        body: v["body"].as_str().unwrap_or("").into(),
        created_at: norm_ts(v["created_at"].as_str().unwrap_or("")),
        system: v["system"].as_bool().unwrap_or(false),
    }
}

/// Titre undraft : retire le préfixe « Draft: » / « WIP: » — l'ancienne
/// API `work_in_progress` est retirée, le titre porte le draft.
fn undraft_title(t: &str) -> Option<String> {
    for p in ["Draft:", "Draft :", "draft:", "WIP:", "wip:"] {
        if let Some(rest) = t.strip_prefix(p) {
            let rest = rest.trim();
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }
    None
}

/// Titre draft : ajoute « Draft: » si absent — inverse de
/// `undraft_title` (garde release → re-draft).
fn draft_title(t: &str) -> String {
    if undraft_title(t).is_some() {
        t.to_string()
    } else {
        format!("Draft: {}", t.trim())
    }
}

/// `access_level` GitLab → assoc normalisée : owner (50) → OWNER,
/// developer+ (≥30) → MEMBER, reporter/guest → NONE. Non-membre → NONE.
fn member_assoc(level: u64) -> String {
    match level {
        50 => "OWNER".into(),
        l if l >= 30 => "MEMBER".into(),
        _ => "NONE".into(),
    }
}

/// REST v4 — `PRIVATE-TOKEN` depuis `token_env` ; lecture anonyme sur
/// les instances publiques, écriture soumise au token.
pub struct GitlabRestClient {
    #[allow(dead_code)]
    host: String,
    target: ForgeTarget,
    /// `https://<host>/api/v4`.
    api: String,
    token: Option<String>,
}

impl GitlabRestClient {
    pub fn new(spec: ForgeSpec, target: ForgeTarget) -> Self {
        Self {
            api: format!("{}/api/v4", host_base(&spec.host)),
            host: spec.host,
            target,
            token: token_value(spec.token_env.as_deref()),
        }
    }

    fn headers(&self) -> Vec<(&'static str, String)> {
        let mut h = Vec::new();
        if let Some(t) = &self.token {
            h.push(("PRIVATE-TOKEN", t.clone()));
        }
        h
    }

    /// `/projects/{enc}` préfixé — le chemin de projet est urlencodé
    /// une fois (`group%2Fsub%2Fproj`).
    fn proj(&self, path: &str) -> String {
        format!("{}/projects/{}{path}", self.api, urlenc(&self.target.repo))
    }

    fn get(&self, url: &str) -> Http {
        http_get(url, &self.headers())
    }

    /// PUT titre sans « Draft: » — l'impl partagée de mark_ready
    /// (glab la rejoue via `glab api` PUT).
    fn put_undraft(&self, id: &str) -> bool {
        let title = match self.get(&self.proj(&format!("/merge_requests/{id}"))) {
            Http::Ok(v) => v["title"].as_str().unwrap_or("").to_string(),
            _ => return false,
        };
        let Some(title) = undraft_title(&title) else {
            return false;
        };
        matches!(
            http_send(
                "PUT",
                &self.proj(&format!("/merge_requests/{id}")),
                &self.headers(),
                &json!({"title": title}),
            ),
            Http::Ok(_)
        )
    }

    /// PUT titre « Draft: …» — mark_draft.
    fn put_draft(&self, id: &str) -> bool {
        let title = match self.get(&self.proj(&format!("/merge_requests/{id}"))) {
            Http::Ok(v) => v["title"].as_str().unwrap_or("").to_string(),
            _ => return false,
        };
        let title = draft_title(&title);
        matches!(
            http_send(
                "PUT",
                &self.proj(&format!("/merge_requests/{id}")),
                &self.headers(),
                &json!({"title": title}),
            ),
            Http::Ok(_)
        )
    }
}

impl ForgeClient for GitlabRestClient {
    fn caps(&self) -> ForgeCaps {
        ForgeCaps {
            list: true,
            comments: true,
            post: self.token.is_some(),
            mark_ready: self.token.is_some(),
            checks: true,
            rollup: true,
        }
    }

    fn list_changes(&self, q: &ListQuery) -> Option<Vec<ChangeInfo>> {
        let state = if q.state.as_deref() == Some("open") {
            "opened"
        } else {
            "all"
        };
        match self.get(&self.proj(&format!(
            "/merge_requests?state={state}&per_page={}&order_by=updated_at",
            q.limit.min(100)
        ))) {
            Http::Ok(v) => v.as_array().map(|a| a.iter().map(mr_change).collect()),
            _ => None,
        }
    }

    fn get_change(&self, id: &str) -> Option<ChangeInfo> {
        match self.get(&self.proj(&format!("/merge_requests/{id}"))) {
            Http::Ok(v) => Some(mr_change(&v)),
            _ => None,
        }
    }

    fn list_comments(&self, id: &str, since: Option<&str>) -> Option<Vec<ChangeComment>> {
        match self.get(&self.proj(&format!(
            "/merge_requests/{id}/notes?sort=asc&order_by=created_at&per_page=100"
        ))) {
            Http::Ok(v) => v.as_array().map(|a| {
                a.iter()
                    .map(mr_note)
                    .filter(|c| since.is_none_or(|s| c.created_at.as_str() >= s))
                    .collect()
            }),
            _ => None,
        }
    }

    fn post_comment(&self, id: &str, body: &str) -> bool {
        if self.token.is_none() {
            return false;
        }
        matches!(
            http_send(
                "POST",
                &self.proj(&format!("/merge_requests/{id}/notes")),
                &self.headers(),
                &json!({"body": body}),
            ),
            Http::Ok(_)
        )
    }

    fn mark_ready(&self, id: &str) -> bool {
        self.token.is_some() && self.put_undraft(id)
    }

    fn mark_draft(&self, id: &str) -> bool {
        self.token.is_some() && self.put_draft(id)
    }

    fn head_ref(&self, id: &str) -> Option<String> {
        Some(format!("merge-requests/{id}/head"))
    }

    /// Appartenance au projet — `/members/all/<user_id>` : résolution
    /// lazy (uniquement pour les notes candidates au trigger).
    fn author_assoc(&self, comment: &ChangeComment) -> String {
        if !comment.assoc.is_empty() {
            return comment.assoc.clone();
        }
        if comment.author_id.is_empty() {
            return "NONE".into();
        }
        match self.get(&self.proj(&format!("/members/all/{}", comment.author_id))) {
            Http::Ok(v) => member_assoc(v["access_level"].as_u64().unwrap_or(0)),
            _ => "NONE".into(),
        }
    }

    fn verify(&self, id: &str) -> Option<bool> {
        match self.get(&self.proj(&format!("/merge_requests/{id}"))) {
            Http::Ok(_) => Some(true),
            // 401/403 : illisible sans droits — jamais résolvable par
            // un sync ; 404/410 : absente.
            Http::Code(401) | Http::Code(403) | Http::Code(404) | Http::Code(410) => Some(false),
            _ => None,
        }
    }
}

/// `glab` CLI — `glab api` en passthrough REST : même endpoints, même
/// parsing que `GitlabRestClient` (le payload est identique).
pub struct GlabClient {
    host: String,
    target: ForgeTarget,
}

impl GlabClient {
    pub fn new(spec: ForgeSpec, target: ForgeTarget) -> Self {
        Self {
            host: spec.host,
            target,
        }
    }

    /// `glab api <endpoint>` — GET par défaut ; `extra` = args après
    /// l'endpoint (`-X POST -f body=…`).
    fn api(&self, endpoint: &str, extra: &[&str]) -> Option<String> {
        let mut argv: Vec<&str> = vec!["api", "--hostname", &self.host, endpoint];
        argv.extend_from_slice(extra);
        cli_run("glab", &argv, &self.target.cwd, CLI_TIMEOUT)
    }

    fn api_json(&self, endpoint: &str) -> Option<Value> {
        self.api(endpoint, &[])
            .and_then(|s| serde_json::from_str(&s).ok())
    }
}

impl ForgeClient for GlabClient {
    fn caps(&self) -> ForgeCaps {
        ForgeCaps {
            list: true,
            comments: true,
            post: true,
            mark_ready: true,
            checks: true,
            rollup: true,
        }
    }

    fn available(&self) -> bool {
        cli_present("glab")
            && self
                .api("user", &[])
                .or_else(|| self.api("version", &[]))
                .is_some()
    }

    fn list_changes(&self, q: &ListQuery) -> Option<Vec<ChangeInfo>> {
        let state = if q.state.as_deref() == Some("open") {
            "opened"
        } else {
            "all"
        };
        self.api_json(&format!(
            "projects/{}/merge_requests?state={state}&per_page={}",
            urlenc(&self.target.repo),
            q.limit.min(100)
        ))
        .and_then(|v| v.as_array().map(|a| a.iter().map(mr_change).collect()))
    }

    fn get_change(&self, id: &str) -> Option<ChangeInfo> {
        self.api_json(&format!(
            "projects/{}/merge_requests/{id}",
            urlenc(&self.target.repo)
        ))
        .map(|v| mr_change(&v))
    }

    fn list_comments(&self, id: &str, since: Option<&str>) -> Option<Vec<ChangeComment>> {
        self.api_json(&format!(
            "projects/{}/merge_requests/{id}/notes?sort=asc&order_by=created_at&per_page=100",
            urlenc(&self.target.repo)
        ))
        .and_then(|v| {
            v.as_array().map(|a| {
                a.iter()
                    .map(mr_note)
                    .filter(|c| since.is_none_or(|s| c.created_at.as_str() >= s))
                    .collect()
            })
        })
    }

    fn post_comment(&self, id: &str, body: &str) -> bool {
        self.api(
            &format!(
                "projects/{}/merge_requests/{id}/notes",
                urlenc(&self.target.repo)
            ),
            &["-X", "POST", "-f", &format!("body={body}")],
        )
        .is_some()
    }

    fn mark_ready(&self, id: &str) -> bool {
        let title = self.get_change(id).and_then(|c| undraft_title(&c.title));
        let Some(title) = title else {
            return false;
        };
        self.api(
            &format!("projects/{}/merge_requests/{id}", urlenc(&self.target.repo)),
            &["-X", "PUT", "-f", &format!("title={title}")],
        )
        .is_some()
    }

    fn mark_draft(&self, id: &str) -> bool {
        let title = self.get_change(id).map(|c| draft_title(&c.title));
        let Some(title) = title else {
            return false;
        };
        self.api(
            &format!("projects/{}/merge_requests/{id}", urlenc(&self.target.repo)),
            &["-X", "PUT", "-f", &format!("title={title}")],
        )
        .is_some()
    }

    fn head_ref(&self, id: &str) -> Option<String> {
        Some(format!("merge-requests/{id}/head"))
    }

    /// Appartenance via `glab api members/all/<id>` — même règle que
    /// le client REST.
    fn author_assoc(&self, comment: &ChangeComment) -> String {
        if !comment.assoc.is_empty() {
            return comment.assoc.clone();
        }
        if comment.author_id.is_empty() {
            return "NONE".into();
        }
        self.api_json(&format!(
            "projects/{}/members/all/{}",
            urlenc(&self.target.repo),
            comment.author_id
        ))
        .map(|v| member_assoc(v["access_level"].as_u64().unwrap_or(0)))
        .unwrap_or_else(|| "NONE".into())
    }

    fn verify(&self, id: &str) -> Option<bool> {
        if !self.available() {
            return None;
        }
        // `glab api` ne classe pas le statut HTTP : absence de JSON
        // = indécidable (404 ou panne) — le REST reste le transport
        // de vérif précis ; ici présence → true, sinon None.
        self.api_json(&format!(
            "projects/{}/merge_requests/{id}",
            urlenc(&self.target.repo)
        ))
        .map(|_| true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undraft_title_strips() {
        assert_eq!(undraft_title("Draft: feat x"), Some("feat x".into()));
        assert_eq!(undraft_title("WIP: fix"), Some("fix".into()));
        assert_eq!(undraft_title("feat x"), None);
        assert_eq!(undraft_title("Draft:"), None);
    }
}
