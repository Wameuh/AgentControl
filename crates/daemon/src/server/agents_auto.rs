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
//! `commands` = fallbacks essayés dans l'ordre — pour `claude` il n'y
//! a plus qu'`acp-claude` (les adaptateurs npm ne sont plus proposés
//! automatiquement, #285) ; `detect` = binaires « outil » dont la
//! présence suffit à lister le profil même sans adaptateur — c'est le
//! cas « Claude Code installé mais adaptateur absent » : le profil
//! apparaît `available:false` avec le hint d'install au lieu de rester
//! invisible.

use super::cfg::{AgentProfile, AgentVia, DEFAULT_MAX_INSTANCES};
use proto::ExtraModel;
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
    /// Commandes d'installation candidates (argv complets, sans shell),
    /// essayées dans l'ordre par `install_agent_adapter` (#277) jusqu'à
    /// ce qu'un binaire de `commands` resolve — `npm i -g <pkg>` pour
    /// les adaptateurs tiers du registre. Vide = pas de provisioning
    /// automatique : l'install renvoie l'erreur « no automatic install
    /// known » + `install_hint` (cas `claude` : `acp-claude` est livré
    /// avec le démon, il faut le mettre à jour — #285).
    pub install: &'static [&'static [&'static str]],
    /// Modèles sélectionnables hors annonce ACP pour ce profil —
    /// paires (value, label) fusionnées dans `extra_models` du profil
    /// auto (ex. opus de claude, absent d'availableModels alors que
    /// session/set_model l'accepte). Vide = rien à ajouter.
    pub extra_models: &'static [(&'static str, &'static str)],
    /// Modèles advisor proposables déclarés par le registre — unis à
    /// `advisor_models` du profil comme `extra_models`. Vide
    /// aujourd'hui : le mécanisme advisor reste l'affaire d'une
    /// section `[agents.<id>]` explicite.
    pub advisor_models: &'static [&'static str],
    /// Note « catalogue complet » (#305) : quand un profil via:config
    /// pin une commande qui n'est PAS un adaptateur du spec alors que
    /// l'adaptateur préféré du registre est installé, list_agents
    /// expose ce texte en `hint` — l'utilisateur voit que l'épinglé
    /// n'annonce qu'un sous-ensemble (ex. claude-code-acp : 3 alias
    /// nus, quand acp-claude rapporte les 14 modèles du CLI). `None`
    /// = pas de hint pour cet adaptateur.
    pub catalogue_note: Option<&'static str>,
}

/// Registre des adaptateurs ACP connus. `claude` : `acp-claude`
/// (#280, binaire Rust livré dans le tarball) est l'UNIQUE chemin
/// automatique — les adaptateurs npm `claude-code-acp`/
/// `claude-agent-acp` ne sont plus proposés ni installés par le
/// démon (#285 ; un `[agents.claude]` manuel peut toujours les
/// viser, override explicite de l'utilisateur). `codex` : `codex`
/// seul ne parle pas ACP — l'adaptateur npm `codex-acp` reste le
/// chemin en attendant `acp-codex`. `gemini` et `opencode` portent
/// leur mode ACP dans leurs propres flags/sous-commandes.
pub(crate) const KNOWN_ADAPTERS: &[AdapterSpec] = &[
    AdapterSpec {
        id: "claude",
        name: "Claude",
        // acp-claude (#280) : adaptateur Rust maison livré dans le
        // tarball à côté du démon — spawn `claude -p --input-format
        // stream-json`, pas de dépendance node. Unique candidat
        // automatique (#285) : les adaptateurs npm claude-code-acp /
        // claude-agent-acp ne sont plus essayés — un `[agents.claude]`
        // déclaré à la main peut toujours les viser explicitement.
        commands: &[&["acp-claude"]],
        detect: &["claude"],
        install_hint: "acp-claude ships with AgentControl — \
             reinstall or update the daemon (and install the \
             `claude` CLI if missing)",
        // Pas de provisioning npm pour claude : acp-claude arrive avec
        // le démon (daemon-update le pose depuis #284) — « installer »
        // renvoie l'erreur no-automatic-install + le hint ci-dessus.
        install: &[],
        // Catalogue claude (#305) : les alias nus absents de
        // l'annonce npm claude-code-acp (qui ne rapporte que
        // default/sonnet/haiku) + les uid versionnés relevés par
        // probe acp-claude — session/set_model les accepte en
        // pass-through CLI même via l'adaptateur npm. Les alias
        // (opus, fable) sont les pointeurs stables « dernière
        // génération » ; les uid claude-* figent une version —
        // liste à ajuster quand le CLI en publie de nouveaux.
        // acp-claude les annonce déjà tous → dédup par value au
        // handshake, rien de visible en double.
        extra_models: &[
            ("opus", "Opus"),
            ("fable", "Fable"),
            ("claude-opus-5", "Opus 5"),
            ("claude-sonnet-5", "Sonnet 5"),
            ("claude-fable-5", "Fable 5"),
            ("claude-fable-5-1", "Fable 5.1"),
            ("claude-opus-4-8", "Opus 4.8"),
            ("claude-opus-4-7", "Opus 4.7"),
            ("claude-opus-4-6", "Opus 4.6"),
            ("claude-sonnet-4-6", "Sonnet 4.6"),
            ("claude-haiku-4-5-20251001", "Haiku 4.5"),
        ],
        advisor_models: &[],
        catalogue_note: Some(
            "acp-claude (livré avec AgentControl) annonce le catalogue \
             complet du CLI claude — l'adaptateur épinglé n'en rapporte \
             qu'un sous-ensemble ; pour en profiter, repointer \
             [agents.claude] sur acp-claude",
        ),
    },
    AdapterSpec {
        id: "codex",
        name: "Codex CLI",
        commands: &[&["codex-acp"]],
        detect: &["codex"],
        install_hint: "npm i -g @agentclientprotocol/codex-acp \
             (ou @zed-industries/codex-acp, archivé) — le CLI `codex` \
             seul n'expose pas ACP",
        // zed-industries/codex-acp est archivé — @agentclientprotocol
        // est l'adaptateur vivant (cf. Known pitfalls AGENTS.md) :
        // essayée en premier, la candidate zed reste en repli.
        install: &[
            &["npm", "i", "-g", "@agentclientprotocol/codex-acp"],
            &["npm", "i", "-g", "@zed-industries/codex-acp"],
        ],
        extra_models: &[],
        advisor_models: &[],
        catalogue_note: None,
    },
    AdapterSpec {
        id: "gemini",
        name: "Gemini CLI",
        commands: &[&["gemini", "--experimental-acp"]],
        detect: &[],
        install_hint: "npm i -g @google/gemini-cli",
        install: &[&["npm", "i", "-g", "@google/gemini-cli"]],
        extra_models: &[],
        advisor_models: &[],
        catalogue_note: None,
    },
    AdapterSpec {
        id: "opencode",
        name: "OpenCode",
        commands: &[&["opencode", "acp"]],
        detect: &[],
        install_hint: "npm i -g opencode-ai",
        install: &[&["npm", "i", "-g", "opencode-ai"]],
        extra_models: &[],
        advisor_models: &[],
        catalogue_note: None,
    },
];

/// `extra_models` du spec en `Vec<ExtraModel>` — partagé entre le
/// profil auto et la fusion dans les profils déclarés.
fn spec_extra_models(spec: &AdapterSpec) -> Vec<ExtraModel> {
    spec.extra_models
        .iter()
        .map(|(v, l)| ExtraModel {
            value: (*v).to_string(),
            label: (*l).to_string(),
        })
        .collect()
}

/// Fusionne les compléments du registre (`extra_models`,
/// `advisor_models`) dans un profil DÉCLARÉ — via:config ou builtin,
/// jamais via:auto (déjà pourvu par `auto_profile`). Les valeurs de
/// l'utilisateur priment à value égal : il peut relabeller un
/// complément (« Opus 4.5 ») mais jamais le perdre. Idempotent (dédup
/// par value) : le round-trip `acctl comp set agents` persiste les
/// extras fusionnés dans `[agents.<id>]` sans doublon ni boucle — au
/// rechargement ils sont relus comme valeurs config, déjà présentes.
fn merge_registry_complements(p: &mut AgentProfile, spec: &AdapterSpec) {
    for e in spec_extra_models(spec) {
        if !p.extra_models.iter().any(|x| x.value == e.value) {
            p.extra_models.push(e);
        }
    }
    for v in spec.advisor_models {
        if !p.advisor_models.iter().any(|x| x == v) {
            p.advisor_models.push((*v).to_string());
        }
    }
}

/// Profil auto d'un adaptateur du registre — `commands` = tous les
/// candidats (la résolution à chaud choisit le premier présent),
/// `via:Auto`. `extra_models`/`advisor_models` sont repris du spec
/// (compléments d'annonce du registre).
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
        // Les extra_models du spec sont des compléments d'annonce
        // contrôlés par le registre (ex. opus de claude, absent
        // d'availableModels alors que session/set_model l'accepte).
        extra_models: spec_extra_models(spec),
        advisor_models: spec.advisor_models.iter().map(|s| s.to_string()).collect(),
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
    // Profils déclarés (via:config/builtin) dont l'id est connu du
    // registre : la commande pinnée et le reste du profil restent
    // strictement ceux de l'utilisateur — seuls les COMPLÉMENTS du
    // registre sont unis (ex. opus de claude, absent d'availableModels
    // de claude-code-acp alors que session/set_model l'accepte —
    // #305). Le merge est idempotent : rejoué à chaque sync sans
    // jamais dupliquer.
    for spec in KNOWN_ADAPTERS {
        if let Some(p) = agents.get_mut(spec.id) {
            if p.via != AgentVia::Auto {
                merge_registry_complements(p, spec);
            }
        }
    }
    (added, removed)
}

/// Hint « catalogue complet » exposé par list_agents (#305) : un
/// profil déclaré (via:config/builtin) dont la commande effective ne
/// correspond à AUCUN adaptateur du spec, alors qu'au moins un
/// adaptateur préféré du registre est installé et que le spec porte
/// une `catalogue_note`. `None` sinon — profil auto, pin sur
/// l'adaptateur du registre, spec sans note, ou adaptateur préféré
/// absent (le hint proposerait une commande introuvable).
/// `resolves` = même prédicat « argv[0] résolvable » que sync.
pub(crate) fn catalogue_hint(
    id: &str,
    p: &AgentProfile,
    resolves: &dyn Fn(&str) -> bool,
) -> Option<String> {
    if p.via == AgentVia::Auto {
        return None;
    }
    let spec = KNOWN_ADAPTERS.iter().find(|s| s.id == id)?;
    let note = spec.catalogue_note?;
    // Commande effective : premier candidat résolvable, sinon le
    // premier — même règle que misc::effective_command.
    let effective = p
        .commands
        .iter()
        .find(|c| c.first().map(|b| resolves(b)).unwrap_or(false))
        .or_else(|| p.commands.first())?;
    let pinned = effective.first()?;
    if spec
        .commands
        .iter()
        .any(|c| c.first().copied() == Some(pinned.as_str()))
    {
        return None;
    }
    spec.commands
        .iter()
        .any(|c| c.first().map(|b| resolves(b)).unwrap_or(false))
        .then(|| note.to_string())
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

/// Commandes d'installation candidates d'un id du registre — `None`
/// quand l'adaptateur n'a pas de provisioning automatique (id hors
/// registre, ou spec sans `install`). Couvre les profils auto ET les
/// `[agents.<id>]` déclarés à la main pour un adaptateur connu.
pub(crate) fn install_cmds_for(id: &str) -> Option<&'static [&'static [&'static str]]> {
    KNOWN_ADAPTERS
        .iter()
        .find(|s| s.id == id)
        .map(|s| s.install)
        .filter(|i| !i.is_empty())
}

/// Binaires `acp-*` que la release est censée livrer à côté du démon
/// — dérivés des argv[0] des specs du registre (`acp-claude`
/// aujourd'hui). Sert au rattrapage boot de `selfupdate::acp_bootstrap`
/// (#289) : « un acp-* attendu manque » = l'un de ces noms absent du
/// dossier de l'exe.
pub(crate) fn shipped_acp_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = KNOWN_ADAPTERS
        .iter()
        .flat_map(|s| s.commands.iter().filter_map(|c| c.first()))
        .filter(|b| b.starts_with("acp-"))
        .copied()
        .collect();
    names.sort_unstable();
    names.dedup();
    names
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

        // acp-claude présent → profil claude auto, unique commande.
        let (a, _) = sync_auto_agents(&mut agents, &|b| b == "acp-claude");
        assert_eq!(a, vec!["claude".to_string()]);
        let p = &agents["claude"];
        assert_eq!(p.via, AgentVia::Auto);
        assert_eq!(p.commands, vec![vec!["acp-claude".to_string()]]);
        // Catalogue claude (#305) : alias opus/fable + uid versionnés
        // — session/set_model accepte les alias CLI même hors
        // annonce → extra_models du registre.
        assert!(p.extra_models.iter().any(|e| e.value == "opus"));
        assert!(p.extra_models.iter().any(|e| e.value == "fable"));
        assert!(p.extra_models.iter().any(|e| e.value == "claude-opus-5"));
        // Second scan idempotent : ni ajout ni retrait.
        let (a2, r2) = sync_auto_agents(&mut agents, &|b| b == "acp-claude");
        assert!(a2.is_empty() && r2.is_empty());
        // Adaptateur disparu → profil auto retiré.
        let (_, r3) = sync_auto_agents(&mut agents, &|_| false);
        assert_eq!(r3, vec!["claude".to_string()]);
        assert!(!agents.contains_key("claude"));
    }

    /// #285 : les adaptateurs npm claude-code-acp / claude-agent-acp
    /// ne sont plus des candidats automatiques — même installés, ils
    /// ne font ni apparaître ni basculer le profil claude (qui reste
    /// listé via `detect` quand le CLI `claude` est là, indisponible
    /// avec le hint « mettre à jour le démon »).
    #[test]
    fn npm_adapters_ne_sont_plus_des_candidats() {
        let mut agents = HashMap::new();
        // Seuls les npm sont présents (pas de CLI claude, pas
        // d'acp-claude) → aucun profil auto.
        let (a, _) = sync_auto_agents(&mut agents, &|b| {
            b == "claude-code-acp" || b == "claude-agent-acp"
        });
        assert!(a.is_empty() && agents.is_empty());
        // CLI claude + npm résiduels, acp-claude absent → profil listé
        // (detect) mais commands = acp-claude seul : pas de bascule.
        let (a, _) = sync_auto_agents(&mut agents, &|b| {
            b == "claude" || b == "claude-code-acp" || b == "claude-agent-acp"
        });
        assert_eq!(a, vec!["claude".to_string()]);
        assert_eq!(
            agents["claude"].commands,
            vec![vec!["acp-claude".to_string()]]
        );
    }

    /// Outil présent sans adaptateur (claude CLI seul) → profil listé
    /// (diagnostic), mais `commands` restent les adaptateurs.
    #[test]
    fn detect_sans_adaptateur_liste_le_profil() {
        let mut agents = HashMap::new();
        sync_auto_agents(&mut agents, &|b| b == "claude");
        assert!(agents.contains_key("claude"));
        assert_eq!(agents["claude"].commands[0], vec!["acp-claude".to_string()]);
    }

    /// `[agents.claude]` déclaré (via:config) → l'auto n'écrase
    /// jamais la commande, même si l'adaptateur est détecté — mais
    /// les COMPLÉMENTS du registre (extra_models) sont fusionnés
    /// (#305) : un pin npm garde opus/fable/uid alors que son
    /// availableModels ne rapporte que default/sonnet/haiku.
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
        let (a, r) = sync_auto_agents(&mut agents, &|b| b == "acp-claude");
        assert!(a.is_empty() && r.is_empty());
        let p = &agents["claude"];
        assert_eq!(p.commands[0], vec!["mon-adaptateur"]);
        assert_eq!(p.via, AgentVia::Config);
        // Compléments du registre fusionnés, commande intacte.
        assert!(p.extra_models.iter().any(|e| e.value == "opus"));
        assert!(p.extra_models.iter().any(|e| e.value == "fable"));
        assert!(p.extra_models.iter().any(|e| e.value == "claude-opus-5"));
        // Idempotent : un second sync ne duplique rien.
        sync_auto_agents(&mut agents, &|b| b == "acp-claude");
        let n = agents["claude"].extra_models.len();
        sync_auto_agents(&mut agents, &|b| b == "acp-claude");
        assert_eq!(agents["claude"].extra_models.len(), n);
    }

    /// #305 : une valeur config à value égal prime sur le libellé du
    /// registre — l'utilisateur peut relabeller (« Opus 4.5 »), pas
    /// perdre ni dupliquer le complément.
    #[test]
    fn config_extra_models_priment_sur_le_registre() {
        let mut agents = HashMap::new();
        let mut p = profile(&[&["claude-code-acp"]]);
        p.via = AgentVia::Config;
        p.extra_models.push(ExtraModel {
            value: "opus".into(),
            label: "Opus 4.5".into(),
        });
        agents.insert("claude".to_string(), p);
        sync_auto_agents(&mut agents, &|_| false);
        let extras = &agents["claude"].extra_models;
        let opus: Vec<_> = extras.iter().filter(|e| e.value == "opus").collect();
        assert_eq!(opus.len(), 1);
        assert_eq!(opus[0].label, "Opus 4.5");
        // Les autres compléments du registre sont tout de même là.
        assert!(extras.iter().any(|e| e.value == "fable"));
    }

    /// #305 : `catalogue_hint` — pin npm alors qu'acp-claude est
    /// installé → hint ; pin sur l'adaptateur du registre, profil
    /// auto, ou adaptateur préféré absent → pas de hint.
    #[test]
    fn catalogue_hint_sur_pin_non_registre() {
        let resolves = |b: &str| b == "acp-claude" || b == "claude-code-acp";
        // Pin npm + acp-claude présent → hint.
        let mut p = profile(&[&["claude-code-acp"]]);
        p.via = AgentVia::Config;
        assert!(catalogue_hint("claude", &p, &resolves).is_some());
        // Pin sur l'adaptateur du registre → muet.
        let mut p2 = profile(&[&["acp-claude"]]);
        p2.via = AgentVia::Config;
        assert!(catalogue_hint("claude", &p2, &resolves).is_none());
        // Profil auto → muet (déjà le meilleur adaptateur).
        let p3 = profile(&[&["acp-claude"]]);
        assert!(catalogue_hint("claude", &p3, &resolves).is_none());
        // acp-claude absent → pas de hint (on ne propose pas une
        // commande introuvable) — profil config pour isoler le cas.
        let mut p4 = profile(&[&["claude-code-acp"]]);
        p4.via = AgentVia::Config;
        assert!(catalogue_hint("claude", &p4, &|b| b == "claude-code-acp").is_none());
        // Id hors registre ou spec sans note → muet.
        let mut p5 = profile(&[&["foo"]]);
        p5.via = AgentVia::Config;
        assert!(catalogue_hint("inconnu", &p5, &resolves).is_none());
        let mut p6 = profile(&[&["autre-chose"]]);
        p6.via = AgentVia::Config;
        assert!(catalogue_hint("codex", &p6, &resolves).is_none());
    }

    /// `install_hint_for` couvre les ids du registre (auto et déclarés).
    #[test]
    fn hint_pour_les_ids_connus() {
        // claude : acp-claude livré avec le démon → hint « update »,
        // plus aucune suggestion npm (#285).
        let hint = install_hint_for("claude").unwrap();
        assert!(hint.contains("acp-claude") && hint.contains("update"));
        assert!(!hint.contains("npm"));
        assert!(install_hint_for("codex").is_some());
        assert!(install_hint_for("devin").is_none());
    }

    /// `install_cmds_for` : codex/gemini/opencode gardent leur
    /// provisioning npm ; claude n'en a plus (acp-claude arrive avec
    /// le démon — #285) ; les ids hors registre n'en ont pas.
    #[test]
    fn install_cmds_pour_les_specs_connus() {
        for id in ["codex", "gemini", "opencode"] {
            let cmds = install_cmds_for(id).expect(id);
            assert!(cmds.iter().all(|c| c.first() == Some(&"npm")), "{id}");
        }
        assert!(install_cmds_for("claude").is_none());
        assert!(install_cmds_for("codex").unwrap().len() > 1);
        assert!(install_cmds_for("devin").is_none());
        assert!(install_cmds_for("inconnu").is_none());
    }

    /// `shipped_acp_names` : seuls les argv[0] `acp-*` du registre —
    /// acp-claude aujourd'hui (codex/gemini/opencode sont des
    /// adaptateurs npm ou des flags, pas des binaires livrés).
    #[test]
    fn shipped_acp_names_reflete_le_registre() {
        assert_eq!(shipped_acp_names(), vec!["acp-claude"]);
    }
}
