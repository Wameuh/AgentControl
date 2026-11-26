//! Forge du reaper `review-queue` : résolution (kind, host, repo)
//! par dépôt revu + verdict d'état des MR/PR associées. Les décisions
//! sont des fonctions pures — testées sans forge réelle (fausses
//! `ForgeClient` et `ChangeInfo` synthétiques).

use super::cfg;
use crate::forges::{self, Forges};
use forgesync::{ChangeInfo, ForgeClient, ListQuery};
use std::path::Path;
use std::sync::Arc;

/// Verdict forge d'UN dépôt du thread : `Closed` = au moins une
/// MR/PR vue et toutes fusionnées/clos ; `Open` = au moins une
/// encore ouverte ; `Unknown` = rien de concluant (aucune MR/PR
/// trouvée pour la branche, forge illisible, transport absent,
/// forge non supportée par le reaper — gerrit/git).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepoVerdict {
    Open,
    Closed,
    Unknown,
}

/// `(kind, host, repo)` de forge visé par le reaper pour le checkout
/// `path` — `reaper_forge` = auto → remote `origin` du dépôt ;
/// `github`/`gitlab` = forcé, hôte = override `reaper_*_host`, sinon
/// celui du remote quand la forge correspond, sinon l'hôte implicite.
/// None = forge indécidable (pas de remote, hôte non déclaré).
pub(crate) fn forge_target(
    cfg: &cfg::ReviewQueueCfg,
    forges: &Forges,
    path: &Path,
) -> Option<(String, String, String)> {
    let (kind, host, repo) = forges::remote_forge(forges, path)?;
    match cfg.reaper_forge_kind() {
        "auto" => {
            // git nu/gerrit : pas de listing exploitable par le
            // reaper → Unknown en aval (remote_forge a quand même
            // répondu, on refuse juste la forge).
            if !matches!(kind.as_str(), "github" | "gitlab") {
                return None;
            }
            Some((kind, host, repo))
        }
        forced => {
            let fhost = if forced == "github" {
                &cfg.reaper_github_host
            } else {
                &cfg.reaper_gitlab_host
            };
            let host = if !fhost.trim().is_empty() {
                fhost.trim().to_string()
            } else if kind == forced {
                host
            } else {
                proto::forge_default_host(forced).to_string()
            };
            Some((forced.to_string(), host, repo))
        }
    }
}

/// Verdict d'un dépôt à partir des changes listés : seuls ceux dont
/// la branche SOURCE égale la branche reviewée comptent (une MR/PR
/// ouverte sur une autre branche ne doit pas retenir le thread).
pub(crate) fn verdict_for_changes(changes: &[ChangeInfo], branch: &str) -> RepoVerdict {
    let mut seen = false;
    for c in changes {
        if c.branch != branch {
            continue;
        }
        seen = true;
        if c.state == "open" || c.state == "draft" {
            return RepoVerdict::Open;
        }
    }
    if seen {
        RepoVerdict::Closed
    } else {
        RepoVerdict::Unknown
    }
}

/// La forge autorise-t-elle la suppression ? Oui ssi TOUS les
/// dépôts du thread ont au moins une MR/PR vue et toutes closes —
/// un seul `Open`/`Unknown` retombe sur le TTL (jamais de suppression
/// sur une preuve incomplète).
pub(crate) fn forge_clear(verdicts: &[RepoVerdict]) -> bool {
    !verdicts.is_empty() && verdicts.iter().all(|v| *v == RepoVerdict::Closed)
}

/// Décision du reaper pour un thread : `None` = conserver.
/// `busy` = review active ou tour vivant sur la session reviewer —
/// intangible par construction (garde dure, testée). `verdicts` =
/// un verdict par dépôt du thread (ordre `pairs`). Forge d'abord :
/// tous clos → suppression immédiate ; un `Open` = preuve
/// concluante que le thread est VIVANT → jamais de TTL non plus.
/// TTL : idle depuis `last_active` > `idle_ttl_secs` (0 = désactivé)
/// et seulement sur des verdicts clos/introuvables — jamais sur un
/// `Open`.
pub(crate) fn reap_reason(
    busy: bool,
    verdicts: &[RepoVerdict],
    last_active: f64,
    now: f64,
    cfg: &cfg::ReviewQueueCfg,
) -> Option<&'static str> {
    if busy {
        return None;
    }
    if forge_clear(verdicts) {
        return Some("forge_closed");
    }
    if verdicts.contains(&RepoVerdict::Open) {
        return None;
    }
    if cfg.idle_ttl_secs > 0 && now - last_active > cfg.idle_ttl_secs as f64 {
        return Some("idle_ttl");
    }
    None
}

/// Client forge résolu pour un dépôt du thread — `set` = le
/// `forge_list` vivant (`Forges::list_json`), `cwd` = le checkout
/// (transports CLI). None = hôte non déclaré / kind sans transport.
pub(crate) fn client_for_target(
    set: &[serde_json::Value],
    target: &(String, String, String),
    cwd: &Path,
) -> Option<Arc<dyn ForgeClient>> {
    forgesync::client_for(set, &target.0, &target.1, &target.2, cwd)
}

/// Changes du dépôt pour la branche reviewée — `list_changes` tous
/// états (open + merged + closed) ; None = lecture impossible
/// (transport absent, hôte mort, caps.list faux).
pub(crate) fn list_changes(client: &Arc<dyn ForgeClient>, limit: usize) -> Option<Vec<ChangeInfo>> {
    if !client.caps().list || !client.available() {
        return None;
    }
    client.list_changes(&ListQuery::all(limit))
}

#[cfg(test)]
mod tests {
    use super::*;
    use forgesync::ChangeInfo;

    fn change(branch: &str, state: &str) -> ChangeInfo {
        ChangeInfo {
            number: 1,
            change_id: None,
            title: String::new(),
            body: String::new(),
            state: state.into(),
            branch: branch.into(),
            base: "main".into(),
            head_sha: String::new(),
            url: String::new(),
            review: String::new(),
            ci: String::new(),
            resolved_repo: None,
            author: String::new(),
            author_id: String::new(),
            author_assoc: String::new(),
            cross_repo: None,
        }
    }

    #[test]
    fn verdict_open_merged_closed() {
        // Branche revue = "feat" : une MR ouverte dessus → Open.
        let cs = vec![change("feat", "open"), change("main", "merged")];
        assert_eq!(verdict_for_changes(&cs, "feat"), RepoVerdict::Open);
        // Draft compte comme ouvert.
        let cs = vec![change("feat", "draft")];
        assert_eq!(verdict_for_changes(&cs, "feat"), RepoVerdict::Open);
        // Toutes closes/fusionnées → Closed.
        let cs = vec![change("feat", "merged"), change("feat", "closed")];
        assert_eq!(verdict_for_changes(&cs, "feat"), RepoVerdict::Closed);
        // Aucune MR sur la branche (même si d'autres existent) →
        // Unknown : pas de preuve, jamais de suppression forge.
        let cs = vec![change("autre", "open")];
        assert_eq!(verdict_for_changes(&cs, "feat"), RepoVerdict::Unknown);
        assert_eq!(verdict_for_changes(&[], "feat"), RepoVerdict::Unknown);
    }

    #[test]
    fn forge_clear_exige_tous_les_depots_clos() {
        assert!(!forge_clear(&[]));
        assert!(!forge_clear(&[RepoVerdict::Closed, RepoVerdict::Unknown]));
        assert!(!forge_clear(&[RepoVerdict::Closed, RepoVerdict::Open]));
        assert!(forge_clear(&[RepoVerdict::Closed]));
        assert!(forge_clear(&[RepoVerdict::Closed, RepoVerdict::Closed]));
    }

    #[test]
    fn reap_decisions() {
        let cfg = cfg::ReviewQueueCfg::default();
        let now = 1_000_000.0;
        // Busy = intangible quoi qu'il arrive.
        assert_eq!(
            reap_reason(true, &[RepoVerdict::Closed], now - 1e9, now, &cfg),
            None
        );
        // Tous clos → suppression immédiate (forge fait foi).
        assert_eq!(
            reap_reason(false, &[RepoVerdict::Closed], now, now, &cfg),
            Some("forge_closed")
        );
        // Preuve incomplète + idle < TTL → conservation.
        assert_eq!(
            reap_reason(false, &[RepoVerdict::Unknown], now - 100.0, now, &cfg),
            None
        );
        // … + idle > TTL → suppression par TTL.
        assert_eq!(
            reap_reason(
                false,
                &[RepoVerdict::Unknown],
                now - cfg.idle_ttl_secs as f64 - 1.0,
                now,
                &cfg
            ),
            Some("idle_ttl")
        );
        // Open + idle > TTL → CONSERVÉ : un MR/PR ouvert est une
        // preuve concluante que le thread est vivant — le TTL ne
        // s'applique qu'aux verdicts clos/introuvables.
        assert_eq!(
            reap_reason(
                false,
                &[RepoVerdict::Open],
                now - cfg.idle_ttl_secs as f64 - 1.0,
                now,
                &cfg
            ),
            None
        );
        // Mixte Open+Closed → pareil, jamais de TTL.
        assert_eq!(
            reap_reason(
                false,
                &[RepoVerdict::Open, RepoVerdict::Closed],
                now - cfg.idle_ttl_secs as f64 - 1.0,
                now,
                &cfg
            ),
            None
        );
        // TTL désactivé (0) → jamais par TTL.
        let mut c2 = cfg.clone();
        c2.idle_ttl_secs = 0;
        assert_eq!(
            reap_reason(false, &[RepoVerdict::Unknown], 0.0, now, &c2),
            None
        );
    }

    #[test]
    fn forge_target_auto_vs_forced() {
        // Dépôt git local sans remote → auto ET forcé = None
        // (remote_forge répond None → indécidable).
        let tmp = std::env::temp_dir().join(format!("rq-ft-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let mut cfg = cfg::ReviewQueueCfg::default();
        let forges = crate::forges::Forges::resolve(&[]);
        assert_eq!(forge_target(&cfg, &forges, &tmp), None);
        cfg.reaper_forge = "github".into();
        cfg.reaper_github_host = "ghe.example.com".into();
        // Sans remote : repo indécidable même forcé.
        assert_eq!(forge_target(&cfg, &forges, &tmp), None);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
