//! Transports GitHub : `gh` CLI (défaut `auto` quand authentifié) et
//! REST v3 (`token_env`, repli `GH_TOKEN`/`GITHUB_TOKEN`, anonyme sinon).
//!
//! Le REST est volontairement dégradé : pas de `reviewDecision` ni de
//! rollup CI dans les listes (`caps.rollup = false` → le sync conserve
//! les badges stockés), `mark_ready` via GraphQL seulement quand un
//! token existe, post/commentaires d'écriture soumis à auth.

use super::{
    token_value, ChangeComment, ChangeInfo, ForgeCaps, ForgeClient, ForgeSpec, ForgeTarget,
    ListQuery,
};
use crate::{
    gh_api, gh_ci_state, gh_ok, gh_review_state, gh_run, gh_state, github_repo_ok, host_base,
    http_get, http_send, GhCall, Http,
};
use serde_json::{json, Value};
use std::path::Path;

/// Champs demandés à `gh pr list|view` — tout `ChangeInfo` tient dedans.
const GH_FIELDS: &str = "number,title,body,state,isDraft,headRefName,headRefOid,\
     baseRefName,url,reviewRequests,reviewDecision,statusCheckRollup";

/// `gh --repo` accepte `HOST/OWNER/REPO` (GHES compris) — forme unique.
fn repo_arg(host: &str, repo: &str) -> String {
    format!("{host}/{repo}")
}

/// Payload `gh pr …` → ChangeInfo normalisé.
fn gh_change(v: &Value) -> ChangeInfo {
    ChangeInfo {
        number: v["number"].as_u64().unwrap_or(0),
        title: v["title"].as_str().unwrap_or("").into(),
        body: v["body"].as_str().unwrap_or("").into(),
        state: gh_state(v),
        branch: v["headRefName"].as_str().unwrap_or("").into(),
        base: v["baseRefName"].as_str().unwrap_or("").into(),
        head_sha: v["headRefOid"].as_str().unwrap_or("").into(),
        url: v["url"].as_str().unwrap_or("").into(),
        review: gh_review_state(v),
        ci: gh_ci_state(v),
        ..Default::default()
    }
}

/// Payload REST `/pulls` → ChangeInfo — `merged_at` tranche merged vs
/// closed ; review/ci absents de ce endpoint → `""` (caps.rollup false).
fn rest_change(v: &Value) -> ChangeInfo {
    let state = if v["state"].as_str() == Some("open") {
        if v["draft"].as_bool().unwrap_or(false) {
            "draft"
        } else {
            "open"
        }
    } else if v["merged_at"].is_string() {
        "merged"
    } else {
        "closed"
    };
    ChangeInfo {
        number: v["number"].as_u64().unwrap_or(0),
        title: v["title"].as_str().unwrap_or("").into(),
        body: v["body"].as_str().unwrap_or("").into(),
        state: state.into(),
        branch: v["head"]["ref"].as_str().unwrap_or("").into(),
        base: v["base"]["ref"].as_str().unwrap_or("").into(),
        head_sha: v["head"]["sha"].as_str().unwrap_or("").into(),
        url: v["html_url"].as_str().unwrap_or("").into(),
        ..Default::default()
    }
}

/// Commentaire `issues/comments` (REST) → forme normalisée — `gh api`
/// rend le même JSON que le client REST, mapping partagé.
fn issue_comment(v: &Value) -> ChangeComment {
    ChangeComment {
        id: v["id"].as_u64().unwrap_or(0),
        author: v["user"]["login"].as_str().unwrap_or("").into(),
        author_id: v["user"]["login"].as_str().unwrap_or("").into(),
        assoc: v["author_association"]
            .as_str()
            .unwrap_or("")
            .to_uppercase(),
        body: v["body"].as_str().unwrap_or("").into(),
        created_at: super::norm_ts(v["created_at"].as_str().unwrap_or("")),
        system: false,
    }
}

/// `gh` CLI — transport historique, complet (rollup GraphQL de `gh pr`
/// inclus review+CI en un appel).
pub struct GhCliClient {
    host: String,
    target: ForgeTarget,
}

impl GhCliClient {
    pub fn new(spec: ForgeSpec, target: ForgeTarget) -> Self {
        Self {
            host: spec.host,
            target,
        }
    }

    fn repo(&self) -> String {
        repo_arg(&self.host, &self.target.repo)
    }
}

impl ForgeClient for GhCliClient {
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
        gh_ok(
            &["auth", "status", "--hostname", &self.host],
            &self.target.cwd,
        )
    }

    fn list_changes(&self, q: &ListQuery) -> Option<Vec<ChangeInfo>> {
        let state = if q.state.as_deref() == Some("open") {
            "open"
        } else {
            "all"
        };
        gh_out_args(
            &[
                "pr",
                "list",
                "--repo",
                &self.repo(),
                "--state",
                state,
                "--limit",
                &q.limit.to_string(),
                "--json",
                GH_FIELDS,
            ],
            &self.target.cwd,
        )
        .and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok())
        .map(|v| v.iter().map(gh_change).collect())
    }

    fn get_change(&self, id: &str) -> Option<ChangeInfo> {
        gh_out_args(
            &[
                "pr",
                "view",
                id,
                "--repo",
                &self.repo(),
                "--json",
                GH_FIELDS,
            ],
            &self.target.cwd,
        )
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .map(|v| gh_change(&v))
    }

    fn list_comments(&self, id: &str, since: Option<&str>) -> Option<Vec<ChangeComment>> {
        // `?since=` filtre côté serveur — le trigger `/mr review` vit
        // dans les issue comments ; per_page=100 borne comme l'ancien
        // `--json comments` de `gh pr view`.
        let mut ep = format!(
            "repos/{}/issues/{id}/comments?per_page=100",
            self.target.repo
        );
        if let Some(s) = since {
            ep.push_str(&format!("&since={s}"));
        }
        match gh_api(&self.host, &ep, &[]) {
            GhCall::Ok(body) => serde_json::from_str::<Vec<Value>>(&body)
                .ok()
                .map(|v| v.iter().map(issue_comment).collect()),
            _ => None,
        }
    }

    fn post_comment(&self, id: &str, body: &str) -> bool {
        gh_run(
            &["pr", "comment", id, "--repo", &self.repo(), "--body", body],
            &self.target.cwd,
        )
        .is_some()
    }

    fn mark_ready(&self, id: &str) -> bool {
        gh_run(
            &["pr", "ready", id, "--repo", &self.repo()],
            &self.target.cwd,
        )
        .is_some()
    }

    fn mark_draft(&self, id: &str) -> bool {
        gh_run(
            &["pr", "ready", id, "--repo", &self.repo(), "--undo"],
            &self.target.cwd,
        )
        .is_some()
    }

    fn head_ref(&self, id: &str) -> Option<String> {
        Some(format!("pull/{id}/head"))
    }

    fn verify(&self, id: &str) -> Option<bool> {
        if self.host != "github.com" && !self.host.contains('.') && !self.host.contains(':') {
            // Hôte malformé → jamais résolvable.
            return Some(false);
        }
        if !github_repo_ok(&self.target.repo) {
            return Some(false);
        }
        if !self.available() {
            return None;
        }
        match gh_api(
            &self.host,
            &format!("repos/{}/pulls/{id}", self.target.repo),
            &[],
        ) {
            GhCall::Ok(_) => Some(true),
            GhCall::Http(404) | GhCall::Http(410) => Some(false),
            // 403 (rate-limit, SSO non granté), 5xx, échec sans statut →
            // indécidable : à retenter, pas un refus définitif.
            _ => None,
        }
    }
}

/// `gh_out` version client — stdout vide accepté.
fn gh_out_args(args: &[&str], cwd: &Path) -> Option<String> {
    gh_run(args, cwd)
}

/// REST v3 — `api.github.com` ou `<host>/api/v3` (GHES). Lecture
/// anonyme possible sur repo public ; écriture/undraft = token requis.
pub struct GhRestClient {
    target: ForgeTarget,
    /// Base API (`https://api.github.com` ou `https://h/api/v3`).
    api: String,
    /// Token résolu (token_env, puis GH_TOKEN/GITHUB_TOKEN).
    token: Option<String>,
}

impl GhRestClient {
    pub fn new(spec: ForgeSpec, target: ForgeTarget) -> Self {
        let api = if spec.host == "github.com" {
            "https://api.github.com".into()
        } else {
            format!("{}/api/v3", host_base(&spec.host))
        };
        // Repli implicite : les variables standard de `gh` servent de
        // token quand `token_env` n'est pas configuré.
        let token = token_value(spec.token_env.as_deref())
            .or_else(|| token_value(Some("GH_TOKEN")))
            .or_else(|| token_value(Some("GITHUB_TOKEN")));
        Self { target, api, token }
    }

    fn headers(&self) -> Vec<(&'static str, String)> {
        let mut h = vec![
            ("Accept", "application/vnd.github+json".to_string()),
            ("User-Agent", "agentcontrol-forgesync".to_string()),
        ];
        if let Some(t) = &self.token {
            h.push(("Authorization", format!("Bearer {t}")));
        }
        h
    }

    fn get(&self, path: &str) -> Http {
        http_get(&format!("{}{path}", self.api), &self.headers())
    }

    /// Check runs + status contexts du head → pending|success|failure.
    fn rollup(&self, sha: &str) -> String {
        let mut vals: Vec<String> = Vec::new();
        if let Http::Ok(v) = self.get(&format!(
            "/repos/{}/commits/{sha}/check-runs?per_page=100",
            self.target.repo
        )) {
            for r in v["check_runs"].as_array().cloned().unwrap_or_default() {
                let c = r["conclusion"]
                    .as_str()
                    .or_else(|| r["status"].as_str())
                    .unwrap_or("")
                    .to_uppercase();
                vals.push(if c == "COMPLETED" {
                    r["conclusion"].as_str().unwrap_or("").to_uppercase()
                } else {
                    c
                });
            }
        }
        if let Http::Ok(v) = self.get(&format!("/repos/{}/commits/{sha}/status", self.target.repo))
        {
            for s in v["statuses"].as_array().cloned().unwrap_or_default() {
                vals.push(s["state"].as_str().unwrap_or("").to_uppercase());
            }
        }
        crate::gh_ci_state(&json!({
            "statusCheckRollup": vals.iter().map(|c| json!({"conclusion": c})).collect::<Vec<_>>()
        }))
    }

    /// Mutation draft/undraft GraphQL — `mutation` =
    /// `markPullRequestReadyForReview` ou `convertPullRequestToDraft`
    /// (aucun équivalent REST v3 ; token requis).
    fn graphql_draft(&self, id: &str, mutation: &str) -> bool {
        if self.token.is_none() {
            return false;
        }
        let node = match self.get(&format!("/repos/{}/pulls/{id}", self.target.repo)) {
            Http::Ok(v) => v["node_id"].as_str().unwrap_or("").to_string(),
            _ => return false,
        };
        if node.is_empty() {
            return false;
        }
        matches!(
            http_send(
                "POST",
                &format!("{}/graphql", self.api),
                &self.headers(),
                &json!({"query": format!(
                    "mutation {{ {mutation}(input: {{pullRequestId: \"{node}\"}}) {{ pullRequest {{ isDraft }} }} }}"
                )}),
            ),
            Http::Ok(_)
        )
    }
}

impl ForgeClient for GhRestClient {
    fn caps(&self) -> ForgeCaps {
        let authed = self.token.is_some();
        ForgeCaps {
            list: true,
            comments: true,
            post: authed,
            mark_ready: authed,
            checks: true,
            rollup: false,
        }
    }

    fn list_changes(&self, q: &ListQuery) -> Option<Vec<ChangeInfo>> {
        let state = if q.state.as_deref() == Some("open") {
            "open"
        } else {
            "all"
        };
        match self.get(&format!(
            "/repos/{}/pulls?state={state}&per_page={}",
            self.target.repo,
            q.limit.min(100)
        )) {
            Http::Ok(v) => v.as_array().map(|a| a.iter().map(rest_change).collect()),
            _ => None,
        }
    }

    fn get_change(&self, id: &str) -> Option<ChangeInfo> {
        match self.get(&format!("/repos/{}/pulls/{id}", self.target.repo)) {
            Http::Ok(v) => {
                let mut c = rest_change(&v);
                if !c.head_sha.is_empty() {
                    c.ci = self.rollup(&c.head_sha);
                }
                Some(c)
            }
            _ => None,
        }
    }

    fn list_comments(&self, id: &str, since: Option<&str>) -> Option<Vec<ChangeComment>> {
        let mut ep = format!(
            "/repos/{}/issues/{id}/comments?per_page=100",
            self.target.repo
        );
        if let Some(s) = since {
            ep.push_str(&format!("&since={s}"));
        }
        match self.get(&ep) {
            Http::Ok(v) => v.as_array().map(|a| a.iter().map(issue_comment).collect()),
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
                &format!(
                    "{}/repos/{}/issues/{id}/comments",
                    self.api, self.target.repo
                ),
                &self.headers(),
                &json!({"body": body}),
            ),
            Http::Ok(_)
        )
    }

    /// Undraft = mutation GraphQL `markPullRequestReadyForReview` — pas
    /// d'équivalent REST v3 ; token requis (sinon caps.post l'exclut).
    fn mark_ready(&self, id: &str) -> bool {
        self.graphql_draft(id, "markPullRequestReadyForReview")
    }

    /// Re-draft = mutation `convertPullRequestToDraft` — mêmes gardes.
    fn mark_draft(&self, id: &str) -> bool {
        self.graphql_draft(id, "convertPullRequestToDraft")
    }

    fn head_ref(&self, id: &str) -> Option<String> {
        Some(format!("pull/{id}/head"))
    }

    fn checks_status(&self, id: &str) -> Option<String> {
        let sha = self.get_change(id)?.head_sha;
        if sha.is_empty() {
            return None;
        }
        Some(self.rollup(&sha)).filter(|s| !s.is_empty())
    }

    fn verify(&self, id: &str) -> Option<bool> {
        if !github_repo_ok(&self.target.repo) {
            return Some(false);
        }
        match self.get(&format!("/repos/{}/pulls/{id}", self.target.repo)) {
            Http::Ok(_) => Some(true),
            Http::Code(404) | Http::Code(410) => Some(false),
            // 401 : repo privé sans token → jamais lisible ; 403 :
            // rate-limit/SSO → indécidable, à retenter.
            Http::Code(401) => Some(false),
            _ => None,
        }
    }
}
