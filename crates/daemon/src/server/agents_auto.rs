//! Auto-découverte des adaptateurs ACP connus (item #261).
//!
//! Le démon embarque un registre d'adaptateurs ACP usuels et sonde le
//! PATH (mêmes dossiers que `command_path` — installs post-boot
//! incluses) à chaque `reload_agent_profiles`, donc à chaque
//! list_agents / daemon_status / spawn : un adaptateur installé après
//! le démarrage voit son profil apparaître sans restart ni section
//! `[agents.*]`.
//!
//! Un profil `[agents.*]` déclaré dans config.toml prime toujours sur
//! l'auto-détection (même id → `via:config`, upsert du hot-reload) ;
//! les presets built-in (devin, cursor) ne sont pas dans le registre.
//!
//! `commands` = fallbacks essayés dans l'ordre (ex. claude-code-acp
//! puis claude-agent-acp) ; `detect` = binaires « outil » dont la
//! présence suffit à lister le profil même sans adaptateur — c'est le
//! cas « Claude Code installé mais adaptateur absent » : le profil
//! apparaît `available:false` avec le hint d'install au lieu de rester
//! invisible.

use super::cfg::{AgentProfile, AgentVia, DEFAULT_MAX_INSTANCES};
use std::collections::HashMap;

/// Un adaptateur ACP connu du registre.
pub(crate) struct AdapterSpec {
    /// Id du profil auto (aussi la clé `[agents.<id>]` qui primerait).
    pub id: &'static str,
    /// Nom affiché dans le sélecteur de session.
    pub name: &'static str,
    /// Commandes candidates (argv complets), essayées dans l'ordre —
    /// la première dont argv[0] résout gagne.
    pub commands: &'static [&'static [&'static str]],
    /// Binaires « outil » détectées : leur présence liste le profil
    /// même quand aucun adaptateur ACP n'est installé (diagnostic
    /// « pourquoi indisponible » au lieu d'invisibilité).
    pub detect: &'static [&'static str],
    /// Hint affiché quand le profil est indisponible.
    pub install_hint: &'static str,
}

/// Registre des adaptateurs ACP connus. `claude` : le CLI `claude`
/// (installeur natif → ~/.local/bin/claude) n'est PAS un adaptateur —
/// il faut `claude-code-acp` (@zed-industries) ou `claude-agent-acp`
/// (@agentclientprotocol). `codex` : idem, `codex` seul ne parle pas
/// ACP — l'adaptateur `codex-acp` est requis. `gemini` et `opencode`
/// portent leur mode ACP dans leurs propres flags/sous-commandes.
pub(crate) const KNOWN_ADAPTERS: &[AdapterSpec] = &[
    AdapterSpec {
        id: "claude",
        name: "Claude",
        commands: &[&["claude-code-acp"], &["claude-agent-acp"]],
        detect: &["claude"],
        install_hint: "npm i -g @zed-industries/claude-code-acp \
             (ou @agentclientprotocol/claude-agent-acp) — le CLI `claude` \
             seul n'expose pas ACP",
    },
    AdapterSpec {
        id: "codex",
        name: "Codex CLI",
        commands: &[&["codex-acp"]],
        detect: &["codex"],
        install_hint: "npm i -g @zed-industries/codex-acp \
             (ou @agentclientprotocol/codex-acp) — le CLI `codex` \
             seul n'expose pas ACP",
    },
    AdapterSpec {
        id: "gemini",
        name: "Gemini CLI",
        commands: &[&["gemini", "--experimental-acp"]],
        detect: &[],
        install_hint: "npm i -g @google/gemini-cli",
    },
    AdapterSpec {
        id: "opencode",
        name: "OpenCode",
        commands: &[&["opencode", "acp"]],
        detect: &[],
        install_hint: "npm i -g opencode-ai",
    },
];

/// Profil auto d'un adaptateur du registre — `commands` = tous les
/// candidats (la résolution à chaud choisit le premier présent),
/// `via:Auto`. Pas d'extra_models/advisor : ces réglages fins restent
/// l'affaire d'une section `[agents.<id>]` qui prime sur l'auto.
fn auto_profile(spec: &AdapterSpec) -> AgentProfile {
    AgentProfile {
        name: spec.name.to_string(),
        commands: spec
            .commands
            .iter()
            .map(|c| c.iter().map(|s| s.to_string()).collect())
            .collect(),
        via: AgentVia::Auto,
        env: HashMap::new(),
        compact: Default::default(),
        max_instances: DEFAULT_MAX_INSTANCES,
        catalogue_cmd: None,
        model_override_cmd: None,
        extra_models: Vec::new(),
        advisor_models: Vec::new(),
        provision_skills: true,
    }
}

/// Vrai si le spec est « présent » : au moins une commande candidate
/// dont argv[0] résout, ou un binaire `detect` résolu (outil installé
/// sans adaptateur → profil listé indisponible avec hint).
fn spec_present(spec: &AdapterSpec, resolves: &dyn Fn(&str) -> bool) -> bool {
    spec.commands
        .iter()
        .any(|c| c.first().map(|b| resolves(b)).unwrap_or(false))
        || spec.detect.iter().any(|b| resolves(b))
}

/// Resynchronise les profils auto dans la table vivante :
/// - id absent de la table et spec présent → inséré (`via:auto`) ;
/// - profil `via:auto` dont le spec n'est plus détecté → retiré
///   (l'outil/désadaptateur a disparu du PATH) ;
/// - id déjà présent avec une autre provenance (builtin/config) →
///   jamais touché : la config gagne toujours.
///
/// `resolves` = prédicat « argv[0] résolvable » (command_available en
/// prod, injectable en test). Retourne (ajoutés, retirés) pour le log.
pub(crate) fn sync_auto_agents(
    agents: &mut HashMap<String, AgentProfile>,
    resolves: &dyn Fn(&str) -> bool,
) -> (Vec<String>, Vec<String>) {
    let mut added = Vec::new();
    let mut removed = Vec::new();
    for spec in KNOWN_ADAPTERS {
        match agents.get(spec.id) {
            // Déjà là : config/builtin priment ; un auto existant est
            // conservé (le profil est statique, `available` est calculé
            // à chaud — inutile de le réécrire à chaque scan).
            Some(p) if p.via != AgentVia::Auto => continue,
            Some(_) => {
                if !spec_present(spec, resolves) {
                    agents.remove(spec.id);
                    removed.push(spec.id.to_string());
                }
                continue;
            }
            None => {
                if spec_present(spec, resolves) {
                    agents.insert(spec.id.to_string(), auto_profile(spec));
                    added.push(spec.id.to_string());
                }
            }
        }
    }
    (added, removed)
}

/// Hint d'install pour un id de profil — registre d'abord (couvre les
/// profils auto ET les `[agents.<id>]` déclarés à la main pour un
/// adaptateur connu), `None` sinon (hint générique côté appelant).
pub(crate) fn install_hint_for(id: &str) -> Option<&'static str> {
    KNOWN_ADAPTERS
        .iter()
        .find(|s| s.id == id)
        .map(|s| s.install_hint)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(commands: &[&[&str]]) -> AgentProfile {
        AgentProfile {
            name: "t".into(),
            commands: commands
                .iter()
                .map(|c| c.iter().map(|s| s.to_string()).collect())
                .collect(),
            via: AgentVia::Auto,
            env: HashMap::new(),
            compact: Default::default(),
            max_instances: DEFAULT_MAX_INSTANCES,
            catalogue_cmd: None,
            model_override_cmd: None,
            extra_models: Vec::new(),
            advisor_models: Vec::new(),
            provision_skills: true,
        }
    }

    /// Rien ne résout → aucun profil auto ; un adaptateur trouvé →
    /// profil `via:auto` avec tous les candidats en fallbacks.
    #[test]
    fn sync_ajoute_seulement_les_specs_detectes() {
        let mut agents = HashMap::new();
        let (a, r) = sync_auto_agents(&mut agents, &|_| false);
        assert!(a.is_empty() && r.is_empty() && agents.is_empty());

        // claude-code-acp présent → profil claude auto.
        let (a, _) = sync_auto_agents(&mut agents, &|b| b == "claude-code-acp");
        assert_eq!(a, vec!["claude".to_string()]);
        let p = &agents["claude"];
        assert_eq!(p.via, AgentVia::Auto);
        assert_eq!(p.commands.len(), 2);
        // Second scan idempotent : ni ajout ni retrait.
        let (a2, r2) = sync_auto_agents(&mut agents, &|b| b == "claude-code-acp");
        assert!(a2.is_empty() && r2.is_empty());
        // Adaptateur disparu → profil auto retiré.
        let (_, r3) = sync_auto_agents(&mut agents, &|_| false);
        assert_eq!(r3, vec!["claude".to_string()]);
        assert!(!agents.contains_key("claude"));
    }

    /// Outil présent sans adaptateur (claude CLI seul) → profil listé
    /// (diagnostic), mais `commands` restent les adaptateurs.
    #[test]
    fn detect_sans_adaptateur_liste_le_profil() {
        let mut agents = HashMap::new();
        sync_auto_agents(&mut agents, &|b| b == "claude");
        assert!(agents.contains_key("claude"));
        assert_eq!(
            agents["claude"].commands[0],
            vec!["claude-code-acp".to_string()]
        );
    }

    /// `[agents.claude]` déclaré (via:config) → l'auto ne l'écrase
    /// jamais, même si l'adaptateur est détecté.
    #[test]
    fn config_prime_sur_auto() {
        let mut agents = HashMap::new();
        agents.insert(
            "claude".to_string(),
            AgentProfile {
                via: AgentVia::Config,
                ..profile(&[&["mon-adaptateur"]])
            },
        );
        let (a, r) = sync_auto_agents(&mut agents, &|b| b == "claude-code-acp");
        assert!(a.is_empty() && r.is_empty());
        assert_eq!(agents["claude"].commands[0], vec!["mon-adaptateur"]);
    }

    /// `install_hint_for` couvre les ids du registre (auto et déclarés).
    #[test]
    fn hint_pour_les_ids_connus() {
        assert!(install_hint_for("claude")
            .unwrap()
            .contains("claude-code-acp"));
        assert!(install_hint_for("codex").is_some());
        assert!(install_hint_for("devin").is_none());
    }
}
