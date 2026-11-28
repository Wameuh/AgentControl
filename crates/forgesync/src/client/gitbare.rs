//! Transport `git` nu — repo sans forge : aucune capacité automatique.
//! Le pipeline dégrade explicitement : pas de list/comment/review auto,
//! déclenchement manuel (`acctl pr review` interne) et verdict rapporté
//! localement par le démon au lieu d'une note forge.

use super::{ChangeComment, ChangeInfo, ForgeCaps, ForgeClient, ForgeSpec, ForgeTarget, ListQuery};
use crate::cli_run;
use std::time::Duration;

/// Git nu — toutes les ops forge renvoient vide ; `caps()` expose le
/// mode dégradé pour que l'orchestrateur shunte le scan auto.
pub struct GitBareClient {
    target: ForgeTarget,
}

impl GitBareClient {
    pub fn new(_spec: ForgeSpec, target: ForgeTarget) -> Self {
        Self { target }
    }
}

impl ForgeClient for GitBareClient {
    fn caps(&self) -> ForgeCaps {
        ForgeCaps {
            list: false,
            comments: false,
            post: false,
            mark_ready: false,
            checks: false,
            rollup: false,
        }
    }

    /// « Disponible » = le checkout local est un repo git — le verdict
    /// local du démon reste possible sans forge.
    fn available(&self) -> bool {
        cli_run(
            "git",
            &["rev-parse", "--git-dir"],
            &self.target.cwd,
            Duration::from_secs(5),
        )
        .is_some()
    }

    fn list_changes(&self, _q: &ListQuery) -> Option<Vec<ChangeInfo>> {
        None
    }

    fn get_change(&self, _id: &str) -> Option<ChangeInfo> {
        None
    }

    fn list_comments(&self, _id: &str, _since: Option<&str>) -> Option<Vec<ChangeComment>> {
        None
    }

    fn post_comment(&self, _id: &str, _body: &str) -> bool {
        false
    }

    /// Jamais vérifiable — un repo git nu n'a pas de référence forge.
    fn verify(&self, _id: &str) -> Option<bool> {
        None
    }
}
