//! Registre PR ↔ session : quel agent a créé quelle pull request.
//!
//! Persisté en JSON (`<root>/prs.json`) — le démon est l'unique writer,
//! un BTreeMap + rewrite atomique suffit (pas de concurrents directs).
//! Alimenté par :
//!  - la détection automatique : un watcher par session scanne les
//!    extraits des tool calls à la recherche d'URLs
//!    `github.com/<owner>/<repo>/pull/<N>` (sortie typique de
//!    `gh pr create`) — voir `watch_prs` dans server/prs.rs ;
//!  - les upserts manuels (`acctl pr add` → ReqKind::PrTrack), qui
//!    priment toujours sur `via: "detected"`.

use anyhow::{Context, Result};
use proto::PrEntry;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct PrRegistry {
    path: PathBuf,
    map: Mutex<BTreeMap<(String, u64), PrEntry>>,
}

impl PrRegistry {
    /// Charge `<root>/prs.json` — un fichier absent ou illisible démarre
    /// un registre vide (le démon ne doit pas mourir pour ça).
    pub fn open(root: &Path) -> Self {
        let path = root.join("prs.json");
        let mut map = BTreeMap::new();
        if path.exists() {
            match std::fs::read_to_string(&path)
                .context("prs.json")
                .and_then(|s| Ok(serde_json::from_str::<serde_json::Value>(&s)?))
            {
                Ok(v) => {
                    for e in v
                        .get("prs")
                        .and_then(|a| a.as_array())
                        .map(|a| a.as_slice())
                        .unwrap_or(&[])
                    {
                        if let Ok(e) = serde_json::from_value::<PrEntry>(e.clone()) {
                            let k = key(&e.repo, e.pr);
                            map.insert(k, e);
                        }
                    }
                }
                Err(e) => tracing::warn!("prs.json ignoré ({e:#}) — registre vide"),
            }
        }
        Self {
            path,
            map: Mutex::new(map),
        }
    }

    pub fn list(&self) -> Vec<PrEntry> {
        // BTreeMap trié (repo, pr) — présenté du plus récent au plus ancien.
        self.map.lock().unwrap().values().rev().cloned().collect()
    }

    /// Upsert manuel (`pr_track`) : `Some` écrase, `None` conserve ;
    /// `via: "manual"` (jamais rétrogradé par la détection) uniquement
    /// quand `session` est fourni — une retouche de métadonnées (note,
    /// sync) ne doit pas sanctifier une attribution auto.
    pub fn track(
        &self,
        session: Option<String>,
        repo: &str,
        pr: u64,
        branch: Option<String>,
        title: Option<String>,
        status: Option<String>,
        note: Option<String>,
    ) -> Result<PrEntry> {
        anyhow::ensure!(!repo.is_empty() && pr > 0, "repo/pr invalides");
        let mut g = self.map.lock().unwrap();
        let t = proto::now_iso();
        let e = match g.get_mut(&key(repo, pr)) {
            Some(old) => {
                if let Some(s) = session {
                    old.session = s;
                    old.via = "manual".into();
                }
                if let Some(v) = branch {
                    old.branch = Some(v);
                }
                if let Some(v) = title {
                    old.title = Some(v);
                }
                if let Some(v) = status {
                    old.status = v;
                }
                if let Some(v) = note {
                    old.note = Some(v);
                }
                old.updated_at = t;
                old.clone()
            }
            None => {
                let session =
                    session.context("session requise pour une nouvelle entrée")?;
                let e = PrEntry {
                    repo: repo.into(),
                    pr,
                    session,
                    branch,
                    title,
                    status: status.unwrap_or_else(|| "open".into()),
                    note,
                    via: "manual".into(),
                    created_at: t.clone(),
                    updated_at: t,
                };
                g.insert(key(repo, pr), e.clone());
                e
            }
        };
        drop(g);
        self.save()?;
        Ok(e)
    }

    /// Détection auto (watcher) : n'écrase JAMAIS une entrée manuelle ;
    /// n'écrase une détection existante que si `strong` (le tool call
    /// ressemblait à un `gh pr create` — pas un simple affichage de la PR
    /// d'un autre) et que la session diffère — sinon no-op (évite de
    /// ré-émettre/bumper à chaque rescan de journal au boot).
    /// Retourne true quand l'entrée a été créée/réattribuée.
    pub fn detected(&self, session: &str, repo: &str, pr: u64, strong: bool) -> Result<bool> {
        let mut g = self.map.lock().unwrap();
        match g.get_mut(&key(repo, pr)) {
            Some(old)
                if old.via == "manual" || !strong || old.session == session =>
            {
                return Ok(false);
            }
            Some(old) => {
                old.session = session.into();
                old.updated_at = proto::now_iso();
            }
            None => {
                let t = proto::now_iso();
                g.insert(
                    key(repo, pr),
                    PrEntry {
                        repo: repo.into(),
                        pr,
                        session: session.into(),
                        branch: None,
                        title: None,
                        status: "open".into(),
                        note: None,
                        via: "detected".into(),
                        created_at: t.clone(),
                        updated_at: t,
                    },
                );
            }
        }
        drop(g);
        self.save()?;
        Ok(true)
    }

    /// Statut de PR vu dans une sortie outil (ligne « Merged pull
    /// request o/r#N ») : fait objectif, pas une attribution — met à
    /// jour `status` quelle que soit la session propriétaire ou `via`.
    /// No-op si l'entrée est absente (rien à retirer) ou déjà à ce
    /// statut (rescans du journal au boot). Retourne true si changé.
    /// NB ordre : au backfill du boot, les journaux sont scannés dans
    /// l'ordre d'attache des sessions — « dernier scanné gagne », pas
    /// chronologiquement entre sessions (cas étroit : reopen dans le
    /// journal de A après un close dans celui de B).
    pub fn state_seen(&self, repo: &str, pr: u64, status: &str) -> Result<bool> {
        anyhow::ensure!(!repo.is_empty() && pr > 0, "repo/pr invalides");
        let mut g = self.map.lock().unwrap();
        let Some(old) = g.get_mut(&key(repo, pr)) else {
            return Ok(false);
        };
        if old.status == status {
            return Ok(false);
        }
        old.status = status.into();
        old.updated_at = proto::now_iso();
        drop(g);
        self.save()?;
        Ok(true)
    }

    pub fn remove(&self, repo: &str, pr: u64) -> Result<bool> {
        anyhow::ensure!(!repo.is_empty() && pr > 0, "repo/pr invalides");
        let gone = self.map.lock().unwrap().remove(&key(repo, pr)).is_some();
        if gone {
            self.save()?;
        }
        Ok(gone)
    }

    /// Rewrite atomique (tmp + rename) — un kill en cours d'écriture ne
    /// laisse pas un fichier tronqué.
    fn save(&self) -> Result<()> {
        let v = {
            let g = self.map.lock().unwrap();
            serde_json::json!({ "prs": g.values().collect::<Vec<_>>() })
        };
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&v)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// Extrait les `github.com/<owner>/<repo>/pull/<N>` d'un texte (extrait
/// de sortie outil). Renvoie des paires ("owner/repo", numéro) dédupliquées.
pub fn extract_pr_refs(text: &str) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("github.com/") {
        rest = &rest[i + "github.com/".len()..];
        // L'URL s'arrête au premier séparateur classique.
        let end = rest
            .find(|c: char| {
                c.is_whitespace() || matches!(c, '"' | '\'' | ')' | ']' | '<' | '\\' | '>' | '|')
            })
            .unwrap_or(rest.len());
        let mut parts = rest[..end].split('/');
        if let (Some(owner), Some(repo), Some(kind), Some(num)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        {
            if kind == "pull" && !owner.is_empty() && !repo.is_empty() {
                // Numéro = préfixe digit ("54#issuecomment-…", "54?tab=…").
                let digits: String = num.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(n) = digits.parse::<u64>() {
                    let key = format!("{owner}/{repo}");
                    if n > 0 && !out.iter().any(|(r, p)| r == &key && *p == n) {
                        out.push((key, n));
                    }
                }
            }
        }
        rest = &rest[end..];
    }
    out
}

/// Clé canonique du registre — GitHub est insensible à la casse sur
/// owner/repo : sans normalisation, `Wameuh/AgentControl` et
/// `wameuh/agentcontrol` feraient deux entrées pour la même PR.
/// `PrEntry.repo` garde la casse d'origine pour l'affichage.
fn key(repo: &str, pr: u64) -> (String, u64) {
    (repo.to_lowercase(), pr)
}

/// Tokens `owner/repo#N` — format compact des sorties gh (« cli/cli#123 »).
fn extract_repo_num_refs(text: &str) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    for tok in text.split(|c: char| {
        !(c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '#'))
    }) {
        let Some((repo, num)) = tok.split_once('#') else {
            continue;
        };
        if repo.matches('/').count() != 1 || repo.starts_with('/') || repo.ends_with('/') {
            continue;
        }
        let digits: String = num.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(n) = digits.parse::<u64>() {
            if n > 0 && !out.iter().any(|(r, p)| r == repo && *p == n) {
                out.push((repo.to_string(), n));
            }
        }
    }
    out
}

/// Extrait les fins de PR d'une sortie outil : formulations d'action de
/// gh — « Merged pull request o/r#N » (`gh pr merge`), « Closed pull
/// request … », « Reopened pull request … » — une URL `…/pull/N` sur la
/// ligne vaut aussi comme référence. Gardes :
///  - lignes JSON (`{`/`[` en tête) ignorées : `gh pr view --json`
///    contient `mergedAt`/`closedAt`/`"merged":false` sans être un merge ;
///  - la forme adjacente « merged pull request » exclut les négations
///    (« was not merged »), « mergeable » et les clés JSON ;
///  - « will be merged » / « queued » = auto-merge différé, pas un fait.
/// Renvoie ("owner/repo", N, statut) — dernier statut vu gagne dans le
/// texte (scrollback reopened→merged dans la même sortie).
pub fn extract_state_refs(text: &str) -> Vec<(String, u64, &'static str)> {
    let mut out: Vec<(String, u64, &'static str)> = Vec::new();
    for line in text.lines() {
        let l = line.to_lowercase();
        if l.trim_start().starts_with('{') || l.trim_start().starts_with('[') {
            continue; // sortie JSON mono-ligne (gh --json, gh api)
        }
        let status = if l.contains("merged pull request") {
            "merged"
        } else if l.contains("closed pull request") {
            "closed"
        } else if l.contains("reopened pull request") {
            "open"
        } else {
            continue;
        };
        if l.contains("will be") || l.contains("queued") {
            continue;
        }
        for (repo, n) in extract_repo_num_refs(line)
            .into_iter()
            .chain(extract_pr_refs(line))
        {
            match out.iter_mut().find(|(r, p, _)| r == &repo && *p == n) {
                Some(e) => e.2 = status, // dernier vu gagne
                None => out.push((repo, n, status)),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{extract_pr_refs, extract_state_refs};

    #[test]
    fn states() {
        assert_eq!(
            extract_state_refs("✓ Merged pull request Wameuh/AgentControl#65"),
            vec![("Wameuh/AgentControl".into(), 65, "merged")]
        );
        assert_eq!(
            extract_state_refs("✓ Closed pull request o/r#7."),
            vec![("o/r".into(), 7, "closed")]
        );
        assert_eq!(
            extract_state_refs("✓ Reopened pull request o/r#2"),
            vec![("o/r".into(), 2, "open")]
        );
        // Statut via l'URL quand gh l'affiche plutôt que o/r#N.
        assert_eq!(
            extract_state_refs("Merged pull request https://github.com/o/r/pull/9"),
            vec![("o/r".into(), 9, "merged")]
        );
        // « merged » sans la forme d'action gh + URL ≠ un merge
        // (les négations « pull/5 was not merged » utilisent ce schéma).
        assert!(extract_state_refs("merged https://github.com/o/r/pull/9").is_empty());
        // Auto-merge différé : la PR reste ouverte.
        assert!(extract_state_refs(
            "✓ Pull request o/r#9 will be merged when all requirements are met"
        )
        .is_empty());
        // Titre de merge-commit git ≠ merge effectué (« Merge », pas « Merged »).
        assert!(extract_state_refs("Merge pull request #65 from x/y").is_empty());
        // « not mergeable » ne contient pas « merged pull request ».
        assert!(extract_state_refs("X Pull request o/r#5 is not mergeable").is_empty());
        // Négations : « merged » n'est pas adjacent à « pull request ».
        assert!(extract_state_refs("o/r#5 was not merged").is_empty());
        // JSON mono-ligne (gh pr view --json) : mergedAt/closedAt ne sont
        // pas un merge — la PR ouverte ne doit pas être marquée merged.
        assert!(extract_state_refs(
            r#"{"state":"OPEN","mergedAt":null,"closedAt":null,"url":"https://github.com/o/r/pull/5"}"#
        )
        .is_empty());
        assert!(extract_state_refs(
            r#"{"merged":false,"closed_at":null,"html_url":"https://github.com/o/r/pull/5"}"#
        )
        .is_empty());
        // Dernier statut vu gagne : reopened puis merged dans la même sortie.
        assert_eq!(
            extract_state_refs("✓ Reopened pull request o/r#5\n✓ Merged pull request o/r#5"),
            vec![("o/r".into(), 5, "merged")]
        );
        assert!(extract_state_refs("gh pr view https://github.com/o/r/pull/3").is_empty());
    }

    #[test]
    fn urls() {
        assert_eq!(
            extract_pr_refs("Create: https://github.com/Wameuh/AgentControl/pull/54"),
            vec![("Wameuh/AgentControl".into(), 54)]
        );
        assert_eq!(
            extract_pr_refs(
                "https://github.com/o/r/pull/12#issuecomment-99 puis https://github.com/o/r/pull/7?x=1"
            ),
            vec![("o/r".into(), 12), ("o/r".into(), 7)]
        );
        assert!(extract_pr_refs("https://github.com/o/r/issues/3").is_empty());
        assert!(extract_pr_refs("pas d'url").is_empty());
    }
}
