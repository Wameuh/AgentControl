//! Forges de review reconnues par le watcher PR : GitHub (toujours
//! actif), GitLab (gitlab.com + hôtes déclarés) et Gerrit (hôtes
//! déclarés uniquement — rien ne distingue un path Gerrit d'un autre
//! service sans déclaration).
//!
//! Config (`config.toml` du démon) :
//! ```toml
//! [[forges]]
//! type = "gitlab"                    # "gitlab" | "gerrit"
//! hosts = ["gitlab.example.com"]     # host[:port][/prefix] sans schéma
//! token_env = "GITLAB_TOKEN"         # optionnel — nom d'une var d'env
//!                                    # portant un token API (jamais le
//!                                    # token en clair dans la config)
//! ```
//!
//! `hosts` peut inclure un préfixe de chemin (installations en
//! sous-chemin : `example.com/gerrit` matche `…/gerrit/c/p/+/N`).

use serde_json::{json, Value};

/// Une entrée `[[forges]]` de config.toml.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ForgeCfg {
    /// "gitlab" | "gerrit" — github est implicite, le déclarer est un no-op.
    #[serde(rename = "type")]
    pub kind: String,
    /// Hôtes déclarés, forme `host[:port][/prefix]` (sans schéma).
    #[serde(default)]
    pub hosts: Vec<String>,
    /// Nom d'une variable d'environnement contenant le token API de la
    /// forge (GitLab `PRIVATE-TOKEN` pour les instances privées) — lu
    /// dans l'env de l'appelant du sync (`acctl`), jamais stocké.
    #[serde(default)]
    pub token_env: Option<String>,
}

/// Hôte résolu d'une forge (normalisé : minuscules, sans schéma ni `/`
/// terminal — les entrées peuvent porter un préfixe de chemin).
#[derive(Debug, Clone)]
pub struct ForgeHost {
    pub host: String,
    pub token_env: Option<String>,
}

/// Set effectif de forges pour la détection : github implicite,
/// gitlab = gitlab.com + déclarés, gerrit = déclarés seulement.
#[derive(Debug, Clone)]
pub struct Forges {
    pub gitlab: Vec<ForgeHost>,
    pub gerrit: Vec<ForgeHost>,
}

/// Référence de review extraite d'une sortie outil.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrRef {
    /// "github" | "gitlab" | "gerrit"
    pub forge: &'static str,
    /// Hôte rencontré dans l'URL (ou l'hôte gerrit choisi pour un
    /// Change-Id nu — voir `extract_refs`).
    pub host: String,
    /// "o/r" | "group/sub/proj" | "project" — "" pour un Change-Id nu.
    pub repo: String,
    /// Numéro de la référence — 0 pour un Change-Id non résolu.
    pub pr: u64,
    /// Change-Id Gerrit ("I<40 hex>").
    pub change_id: Option<String>,
    /// URL canonique web (identique à `proto::forge_url`).
    pub url: String,
}

impl PrRef {
    /// Affichage « repo#N » des messages système du journal — cohérent
    /// avec les logs historiques github (`o/r#N`).
    pub fn display(&self) -> String {
        let badge = proto::forge_badge(self.forge, self.pr, self.change_id.as_deref());
        if self.repo.is_empty() {
            format!("{}:{}", self.host, badge)
        } else {
            format!("{}{}", self.repo, badge)
        }
    }
}

fn norm_host(h: &str) -> String {
    let h = h.trim().to_lowercase();
    h.trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_matches('/')
        .to_string()
}

impl Forges {
    /// Résout `[[forges]]` en set effectif : hosts normalisés,
    /// `gitlab.com` toujours présent (plus les hôtes déclarés), gerrit
    /// uniquement sur déclaration. Type inconnu → warn + ignoré.
    pub fn resolve(cfgs: &[ForgeCfg]) -> Self {
        let mut f = Forges {
            gitlab: vec![ForgeHost {
                host: "gitlab.com".into(),
                token_env: None,
            }],
            gerrit: Vec::new(),
        };
        for c in cfgs {
            let kind = c.kind.trim().to_lowercase();
            if kind == "github" {
                tracing::warn!("[[forges]] type=github implicite — entrée ignorée");
                continue;
            }
            let list = match kind.as_str() {
                "gitlab" => &mut f.gitlab,
                "gerrit" => &mut f.gerrit,
                _ => {
                    tracing::warn!("[[forges]] type inconnu « {} » — ignoré", c.kind);
                    continue;
                }
            };
            for h in &c.hosts {
                let host = norm_host(h);
                if host.is_empty() {
                    continue;
                }
                if list.iter().any(|x| x.host == host) {
                    continue;
                }
                list.push(ForgeHost {
                    host,
                    token_env: c.token_env.clone(),
                });
            }
        }
        f
    }

    /// `forge_list` RPC — set effectif exposé aux clients (acctl sync,
    /// settings). `hosts` = objets `{host, token_env?}` : le token de
    /// deux hôtes d'une même forge peut différer. `token_env` est un
    /// NOM de variable, pas un secret.
    pub fn list_json(&self) -> Value {
        let hosts = |v: &[ForgeHost]| -> Vec<Value> {
            v.iter()
                .map(|h| match &h.token_env {
                    Some(e) => json!({"host": h.host, "token_env": e}),
                    None => json!({"host": h.host}),
                })
                .collect()
        };
        Value::Array(vec![
            json!({"type": "github", "hosts": [{"host": "github.com"}]}),
            json!({"type": "gitlab", "hosts": hosts(&self.gitlab)}),
            json!({"type": "gerrit", "hosts": hosts(&self.gerrit)}),
        ])
    }

    /// Extrait toutes les références de review d'un texte (extrait de
    /// sortie outil). Dédupliqué par (forge, host, repo, pr, change_id).
    pub fn extract_refs(&self, text: &str) -> Vec<PrRef> {
        let mut out: Vec<PrRef> = Vec::new();
        scan_host_refs(text, "github.com", "github", &mut out);
        for h in &self.gitlab {
            scan_host_refs(text, &h.host.clone(), "gitlab", &mut out);
        }
        for h in &self.gerrit {
            scan_host_refs(text, &h.host.clone(), "gerrit", &mut out);
        }
        // Change-Id nu (`Change-Id: I<sha>` — trailers de commit dans un
        // `git log`/`git show`). Sans hôte dans le texte : l'hôte gerrit
        // vu dans la MÊME sortie l'emporte, sinon l'unique hôte
        // configuré, sinon le premier (documenté — un Change-Id est
        // ambigu entre plusieurs Gerrit).
        if !self.gerrit.is_empty() {
            for cid in extract_change_ids(text) {
                if out
                    .iter()
                    .any(|r| r.forge == "gerrit" && r.change_id.as_deref() == Some(cid.as_str()))
                {
                    continue; // l'URL du même change était dans le texte
                }
                let host = out
                    .iter()
                    .find(|r| r.forge == "gerrit")
                    .map(|r| r.host.clone())
                    .unwrap_or_else(|| self.gerrit[0].host.clone());
                push_ref(
                    &mut out,
                    PrRef {
                        forge: "gerrit",
                        host: host.clone(),
                        repo: String::new(),
                        pr: 0,
                        url: proto::forge_url("gerrit", &host, "", 0, Some(&cid)),
                        change_id: Some(cid),
                    },
                );
            }
        }
        out
    }
}

/// Libellés génériques utilisés par les docs, specs et tests comme
/// placeholder de chemin de projet (`o/r`, `owner/repo`, `g/p`,
/// `grp/sub/proj`, `foo/bar`, `exemple.com/…`). Un chemin n'est refusé
/// que si TOUS ses segments sont génériques — un segment réel
/// (`acme/widgets`) suffit à garder la référence.
const PLACEHOLDER_WORDS: &[&str] = &[
    "o", "r", "owner", "repo", "repository", "repos", "g", "p", "group",
    "grp", "sub", "subgroup", "proj", "project", "projects", "a", "b",
    "x", "y", "foo", "bar", "baz", "qux", "user", "username", "user-name",
    "name", "org", "organisation", "organization", "myorg", "my-org",
    "your-org", "your_org", "yourorg", "your-repo", "your_project",
    "exemple", "example", "placeholder", "sample", "demo", "dummy",
    "test", "namespace", "path", "votre", "projet", "depot", "repo-name",
    "owner-name", "project-name", "my-project", "mon-projet",
];

/// `true` si `repo` est un placeholder documentaire : chaque segment
/// tient en un caractère ou appartient à `PLACEHOLDER_WORDS` —
/// `o/r`, `g/p`, `owner/repo`, `grp/sub/proj`, `a/b`, `x/y`, `foo/bar`,
/// `exemple/truc`… Ne s'applique qu'à la détection automatique : un
/// `pr add` manuel reste possible (intention explicite).
pub(crate) fn placeholder_repo(repo: &str) -> bool {
    let mut seen = false;
    for seg in repo.split('/') {
        let s = seg.trim();
        if s.is_empty() {
            continue;
        }
        seen = true;
        let ls = s.to_lowercase();
        if ls.chars().count() > 1 && !PLACEHOLDER_WORDS.contains(&ls.as_str()) {
            return false;
        }
    }
    seen
}

/// Ajoute `r` si absent (dédoublonnage sur l'identité complète).
fn push_ref(out: &mut Vec<PrRef>, r: PrRef) {
    let dup = out.iter().any(|x| {
        x.forge == r.forge
            && x.host == r.host
            && x.repo.eq_ignore_ascii_case(&r.repo)
            && x.pr == r.pr
            && x.change_id == r.change_id
    });
    if !dup {
        out.push(r);
    }
}

/// Caractère précédant le host trouvé : doit clore l'URL/host et non en
/// faire partie — sinon `notgitlab.com/` ou `sub.gitlab.com/` matcheraient
/// `gitlab.com/`. Lettres, chiffres, `-` et `.` refusés (`.`
/// = sous-domaine ; le port `:` est accepté : `host:8080/path`).
fn host_boundary_ok(text: &str, match_start: usize) -> bool {
    match text[..match_start].chars().next_back() {
        None => true,
        Some(c) => !(c.is_ascii_alphanumeric() || c == '-' || c == '.'),
    }
}

/// Fin d'URL : premier séparateur classique (même convention que
/// l'extracteur github historique).
fn url_end(path: &str) -> usize {
    path.find(|c: char| {
        c.is_whitespace() || matches!(c, '"' | '\'' | ')' | ']' | '<' | '\\' | '>' | '|')
    })
    .unwrap_or(path.len())
}

/// Parse le chemin d'URL selon la forge et pousse la référence.
///  - github  : `<o>/<r>/pull/<N>`
///  - gitlab  : `<g>(/sous)*/-/merge_requests/<N>`
///  - gerrit  : `[#/](c|r)/<projet>/+/<N>` ou `q/<Change-Id>`
fn parse_path(
    host: &str,
    forge: &'static str,
    path: &str,
    out: &mut Vec<PrRef>,
) {
    match forge {
        "github" => {
            let mut parts = path.split('/');
            if let (Some(owner), Some(repo), Some(kind), Some(num)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            {
                if kind == "pull" && !owner.is_empty() && !repo.is_empty() {
                    if let Some(n) = leading_num(num) {
                        let repo = format!("{owner}/{repo}");
                        if placeholder_repo(&repo) {
                            return;
                        }
                        push_ref(
                            out,
                            PrRef {
                                forge,
                                host: host.into(),
                                url: proto::forge_url(forge, host, &repo, n, None),
                                repo,
                                pr: n,
                                change_id: None,
                            },
                        );
                    }
                }
            }
        }
        "gitlab" => {
            if let Some((proj, tail)) = path.split_once("/-/merge_requests/") {
                if !proj.is_empty() && !placeholder_repo(proj) {
                    if let Some(n) = leading_num(tail) {
                        push_ref(
                            out,
                            PrRef {
                                forge,
                                host: host.into(),
                                url: proto::forge_url(forge, host, proj, n, None),
                                repo: proj.into(),
                                pr: n,
                                change_id: None,
                            },
                        );
                    }
                }
            }
        }
        "gerrit" => {
            // `q/I<sha>` : URL de recherche d'un Change-Id.
            if let Some(q) = path.strip_prefix("q/") {
                if let Some(cid) = take_change_id(q) {
                    push_ref(
                        out,
                        PrRef {
                            forge,
                            host: host.into(),
                            repo: String::new(),
                            pr: 0,
                            url: proto::forge_url(forge, host, "", 0, Some(&cid)),
                            change_id: Some(cid),
                        },
                    );
                }
                return;
            }
            let rest = path
                .strip_prefix("c/")
                .or_else(|| path.strip_prefix("r/"))
                .or_else(|| path.strip_prefix("#/c/"))
                .or_else(|| path.strip_prefix("#/r/"));
            if let Some(rest) = rest {
                if let Some((proj, tail)) = rest.rsplit_once("/+/") {
                    if !proj.is_empty() && !placeholder_repo(proj) {
                        if let Some(n) = leading_num(tail) {
                            push_ref(
                                out,
                                PrRef {
                                    forge,
                                    host: host.into(),
                                    url: proto::forge_url(forge, host, proj, n, None),
                                    repo: proj.into(),
                                    pr: n,
                                    change_id: None,
                                },
                            );
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// Cherche les occurrences de `{host}/` dans le texte et parse chaque
/// chemin selon la forge. `host` peut porter un préfixe de chemin
/// (`example.com/gerrit` → needle `example.com/gerrit/`).
fn scan_host_refs(text: &str, host: &str, forge: &'static str, out: &mut Vec<PrRef>) {
    let needle = format!("{host}/");
    let mut off = 0usize;
    while let Some(i) = text[off..].find(&needle) {
        let abs = off + i;
        if !host_boundary_ok(text, abs) {
            off = abs + needle.len();
            continue;
        }
        let path_start = abs + needle.len();
        let end = url_end(&text[path_start..]);
        parse_path(host, forge, &text[path_start..path_start + end], out);
        off = path_start + end;
    }
}

/// Numéro en tête de segment ("54#issuecomment-…", "54?tab=…", "5/diffs").
fn leading_num(s: &str) -> Option<u64> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    let n = digits.parse::<u64>().ok()?;
    (n > 0).then_some(n)
}

/// `I<40 hex>` en tête de chaîne → le Change-Id complet. Teste les
/// OCTETS avant tout slice : tout-ASCII garantit la frontière UTF-8
/// (un `é` en position 41 ne doit pas paniquer le watcher).
fn take_change_id(s: &str) -> Option<String> {
    let b = s.as_bytes();
    if b.len() >= 41 && b[0] == b'I' && b[1..41].iter().all(|b| b.is_ascii_hexdigit()) {
        Some(s[..41].to_string())
    } else {
        None
    }
}

/// `Change-Id: I<40 hex>` — le trailer de commit, dans une sortie
/// `git log`/`git show`/`git review`. Le préfixe « Change-Id: » est
/// requis : un `I<40hex>` nu matcherait n'importe quel hash affiché.
fn extract_change_ids(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let needle = "Change-Id:";
    let mut off = 0usize;
    while let Some(i) = text[off..].find(needle) {
        let mut rest = &text[off + i + needle.len()..];
        rest = rest.trim_start();
        if let Some(cid) = take_change_id(rest) {
            if !out.contains(&cid) {
                out.push(cid);
            }
        }
        off += i + needle.len();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forges() -> Forges {
        Forges::resolve(&[
            ForgeCfg {
                kind: "gitlab".into(),
                hosts: vec!["gitlab.example.com".into()],
                token_env: None,
            },
            ForgeCfg {
                kind: "gerrit".into(),
                hosts: vec![
                    "gerrit-review.googlesource.com".into(),
                    "gerrit.internal:8080".into(),
                ],
                token_env: None,
            },
        ])
    }

    fn github_only() -> Forges {
        Forges::resolve(&[])
    }

    #[test]
    fn github_urls() {
        let f = github_only();
        assert_eq!(
            f.extract_refs("Create: https://github.com/Wameuh/AgentControl/pull/54"),
            vec![PrRef {
                forge: "github",
                host: "github.com".into(),
                repo: "Wameuh/AgentControl".into(),
                pr: 54,
                change_id: None,
                url: "https://github.com/Wameuh/AgentControl/pull/54".into(),
            }]
        );
        assert_eq!(
            f.extract_refs(
                "https://github.com/acme/widgets/pull/12#issuecomment-99 puis https://github.com/acme/widgets/pull/7?x=1"
            )
            .len(),
            2
        );
        assert!(f.extract_refs("https://github.com/acme/widgets/issues/3").is_empty());
        // Pas de faux hôte : xgithub.com / github.com.evil non reconnus.
        assert!(f.extract_refs("https://xgithub.com/acme/widgets/pull/3").is_empty());
        assert!(f.extract_refs("https://github.com.evil/acme/widgets/pull/3").is_empty());
    }

    #[test]
    fn placeholder_repos_rejected() {
        // Les sorties outils reproduisent le contenu des fichiers lus
        // (docs, specs, tests, AGENTS.md) — les URLs placeholder qu'ils
        // contiennent ne doivent pas créer d'entrées fantômes.
        let f = forges();
        for txt in [
            "https://github.com/o/r/pull/9",
            "https://github.com/owner/repo/pull/3",
            "https://github.com/x/y/pull/1",
            "https://github.com/foo/bar/pull/42",
            "https://github.com/votre/projet/pull/7",
            "https://gitlab.com/g/p/-/merge_requests/42",
            "https://gitlab.com/grp/sub/proj/-/merge_requests/42",
            "https://gitlab.com/owner/repo/-/merge_requests/1",
            "https://gitlab.com/a/b/-/merge_requests/2",
            "https://gerrit.internal:8080/c/p/+/9",
            "https://gerrit.internal:8080/c/proj/+/9",
        ] {
            assert!(f.extract_refs(txt).is_empty(), "refusé attendu : {txt}");
        }
        // Un seul segment réel suffit : le placeholder n'est que
        // partiel → la référence est conservée.
        for (txt, repo) in [
            ("https://github.com/acme/widgets/pull/9", "acme/widgets"),
            ("https://gitlab.com/corp/tools/-/merge_requests/42", "corp/tools"),
            ("https://gerrit.internal:8080/c/platform/build/+/9", "platform/build"),
        ] {
            let r = f.extract_refs(txt);
            assert_eq!(r.len(), 1, "conservé attendu : {txt}");
            assert_eq!(r[0].repo, repo);
        }
    }

    #[test]
    fn gitlab_urls() {
        let f = forges();
        // gitlab.com implicite + sous-groupes.
        let r = f.extract_refs("MR: https://gitlab.com/infra-team/deploy/tool/-/merge_requests/42");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].forge, "gitlab");
        assert_eq!(r[0].repo, "infra-team/deploy/tool");
        assert_eq!(r[0].pr, 42);
        assert_eq!(
            r[0].url,
            "https://gitlab.com/infra-team/deploy/tool/-/merge_requests/42"
        );
        // Hôte auto-hébergé déclaré.
        let r = f.extract_refs("voir https://gitlab.example.com/corp/tools/-/merge_requests/7/diffs");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].host, "gitlab.example.com");
        assert_eq!(r[0].pr, 7);
        // Hôte gitlab NON déclaré → pas de détection.
        assert!(f.extract_refs("https://gitlab.inconnu.io/corp/tools/-/merge_requests/7").is_empty());
        // Borne : notgitlab.example.com ne matche pas gitlab.example.com.
        assert!(f
            .extract_refs("https://notgitlab.example.com/corp/tools/-/merge_requests/7")
            .is_empty());
        // Issue ≠ MR.
        assert!(f.extract_refs("https://gitlab.com/corp/tools/-/issues/7").is_empty());
    }

    #[test]
    fn gerrit_urls() {
        let f = forges();
        let r = f.extract_refs(
            "remote: https://gerrit-review.googlesource.com/c/platform/build/+/123456",
        );
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].forge, "gerrit");
        assert_eq!(r[0].repo, "platform/build");
        assert_eq!(r[0].pr, 123456);
        assert_eq!(
            r[0].url,
            "https://gerrit-review.googlesource.com/c/platform/build/+/123456"
        );
        // Variante /r/ et fragment #/c/.
        assert_eq!(
            f.extract_refs("https://gerrit.internal:8080/r/apps/web/+/77")[0].repo,
            "apps/web"
        );
        assert_eq!(
            f.extract_refs("https://gerrit.internal:8080/#/c/apps/web/+/88")[0].pr,
            88
        );
        // Port dans l'hôte configuré.
        assert_eq!(
            f.extract_refs("https://gerrit.internal:8080/c/apps/web/+/9")[0].host,
            "gerrit.internal:8080"
        );
        // URL de recherche d'un Change-Id.
        let cid = "I0123456789abcdef0123456789abcdef01234567";
        let r = f.extract_refs(&format!("https://gerrit.internal:8080/q/{cid}"));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].change_id.as_deref(), Some(cid));
        assert_eq!(r[0].pr, 0);
        // Gerrit non configuré → rien.
        assert!(github_only()
            .extract_refs("https://gerrit-review.googlesource.com/c/p/+/1")
            .is_empty());
    }

    #[test]
    fn gerrit_change_id_trailer() {
        let f = forges();
        let cid = "Ideadbeefcafe0123456789abcdef0123456789ab";
        let r = f.extract_refs(&format!("commit abc\n    Change-Id: {cid}\n"));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].change_id.as_deref(), Some(cid));
        // Change-Id seul + plusieurs gerrit → premier hôte configuré.
        assert_eq!(r[0].host, "gerrit-review.googlesource.com");
        // Le même texte contient l'URL du change → pas de doublon cid.
        let r = f.extract_refs(&format!(
            "https://gerrit.internal:8080/q/{cid}\nChange-Id: {cid}"
        ));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].host, "gerrit.internal:8080");
        // Sans gerrit configuré, un trailer Change-Id ne produit rien.
        assert!(github_only()
            .extract_refs(&format!("Change-Id: {cid}"))
            .is_empty());
        // Un I<hex> nu (pas de préfixe « Change-Id: ») n'est pas capté.
        assert!(f.extract_refs(&format!("le hash {cid}")).is_empty());
    }

    #[test]
    fn same_host_two_forges() {
        // Un hôte peut servir GitLab ET Gerrit : le path discrimine.
        let f = Forges::resolve(&[
            ForgeCfg {
                kind: "gitlab".into(),
                hosts: vec!["code.corp".into()],
                token_env: None,
            },
            ForgeCfg {
                kind: "gerrit".into(),
                hosts: vec!["code.corp".into()],
                token_env: None,
            },
        ]);
        let r = f.extract_refs(
            "https://code.corp/corp/tools/-/merge_requests/1 https://code.corp/c/core/app/+/2",
        );
        assert_eq!(r.len(), 2);
        assert!(r.iter().any(|x| x.forge == "gitlab" && x.pr == 1));
        assert!(r.iter().any(|x| x.forge == "gerrit" && x.pr == 2));
    }

    #[test]
    fn resolve_normalise() {
        let f = Forges::resolve(&[ForgeCfg {
            kind: "GitLab".into(),
            hosts: vec!["HTTPS://GL.Corp/".into()],
            token_env: Some("GL_TOK".into()),
        }]);
        assert!(f.gitlab.iter().any(|h| h.host == "gl.corp"));
        assert!(f.gitlab.iter().any(|h| h.host == "gitlab.com"));
        // Type inconnu → ignoré sans panique.
        let f = Forges::resolve(&[ForgeCfg {
            kind: "bitbucket".into(),
            hosts: vec!["bb.corp".into()],
            token_env: None,
        }]);
        assert!(f.gerrit.is_empty());
    }

    #[test]
    fn change_id_non_ascii() {
        // Un multi-octet pile à la borne 41 : le test sur bytes évite le
        // panic de slicing (régression : `s[1..41]` sur frontière UTF-8).
        let cid39 = format!("I{}é", "a".repeat(39));
        assert_eq!(take_change_id(&cid39), None);
        let cid40 = format!("I{}zzz", "a".repeat(40));
        assert_eq!(take_change_id(&cid40).as_deref(), Some(cid40[..41].to_string().as_str()));
    }
}
