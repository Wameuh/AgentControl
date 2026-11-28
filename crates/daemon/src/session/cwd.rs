//! Cwd « effectif » déclaré par l'agent (`session_set_cwd`,
//! `acctl cwd`) : persisté dans meta.json (`reported_cwd`) — même
//! pattern que les autres champs meta, pas de sidecar. Consommé par
//! term_open : le shell PTY s'ouvre dans le worktree où l'agent
//! travaille réellement quand il diffère du cwd de spawn
//! (`meta.cwd`), avec repli sur `meta.cwd` si la déclaration est
//! absente ou devenue invalide (worktree supprimé…).
//! `meta.cwd` n'est JAMAIS réécrit : il reste le workspace bornant
//! les outils fichier et le cwd de session/new|load.

use super::*;
use anyhow::{Context, Result};

impl Session {
    /// Déclare (ou efface, `None`) le cwd effectif : le chemin doit
    /// être absolu et désigner un dossier existant — canonicalisé
    /// avant stockage (les worktrees peuvent être sous un symlink).
    /// L'agent est un déclarant de confiance : il a déjà un shell
    /// complet sur l'hôte, la déclaration n'élargit aucune surface —
    /// elle ne fait que choisir le cwd du PTY terminal.
    /// Renvoie la valeur stockée (None = déclaration effacée).
    pub fn set_reported_cwd(&self, cwd: Option<String>) -> Result<Option<String>> {
        let stored = match cwd {
            None => None,
            Some(c) => {
                let p = std::path::Path::new(&c);
                anyhow::ensure!(p.is_absolute(), "cwd must be absolute: {c}");
                let canon = std::fs::canonicalize(p).with_context(|| format!("cwd: {c}"))?;
                anyhow::ensure!(canon.is_dir(), "cwd is not a directory: {c}");
                Some(canon.display().to_string())
            }
        };
        {
            let mut m = self.meta.lock().unwrap();
            if m.reported_cwd == stored {
                return Ok(stored);
            }
            m.reported_cwd = stored.clone();
        }
        self.save_meta();
        Ok(stored)
    }

    /// Cwd dans lequel ouvrir le terminal PTY : `reported_cwd` s'il
    /// est encore valide (absolu, dossier existant — un worktree
    /// supprimé retombe proprement sur le cwd de session), sinon
    /// `meta.cwd`.
    pub fn term_cwd(&self) -> String {
        let m = self.meta_snapshot();
        if let Some(c) = &m.reported_cwd {
            let p = std::path::Path::new(c);
            if p.is_absolute() && p.is_dir() {
                return c.clone();
            }
        }
        m.cwd
    }
}
