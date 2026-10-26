//! Registre PR ↔ session : quel agent a créé quelle pull request.
//!
//! Persisté en JSON (`<root>/prs.json`) — le démon est l'unique writer,
//! un BTreeMap + rewrite atomique suffit (pas de concurrents directs).
//! Alimenté par :
//!  - la détection automatique : un watcher par session scanne les
//!    extraits des tool calls à la recherche d'URLs
//!    `github.com/<owner>/<repo>/pull/<N>` (sortie typique de
//!    `gh pr create`) — voir `watch_prs` dans server.rs ;
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
                            map.insert((e.repo.clone(), e.pr), e);
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
        let e = match g.get_mut(&(repo.to_string(), pr)) {
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
                g.insert((repo.into(), pr), e.clone());
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
        match g.get_mut(&(repo.to_string(), pr)) {
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
                    (repo.to_string(), pr),
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

    pub fn remove(&self, repo: &str, pr: u64) -> Result<bool> {
        anyhow::ensure!(!repo.is_empty() && pr > 0, "repo/pr invalides");
        let gone = self
            .map
            .lock()
            .unwrap()
            .remove(&(repo.to_string(), pr))
            .is_some();
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

#[cfg(test)]
mod tests {
    use super::extract_pr_refs;

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
