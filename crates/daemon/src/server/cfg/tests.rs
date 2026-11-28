use super::*;

fn policy(enabled: bool, at_tokens: Option<u64>, at_pct: Option<f64>) -> CompactPolicy {
    CompactPolicy {
        enabled,
        at_tokens,
        at_pct,
    }
}

/// exceeds : OU des deux règles — seuil tokens, seuil % (fenêtre
/// requise), et `enabled = false` court-circuite tout.
#[test]
fn compact_exceeds() {
    let p = policy(true, Some(100_000), None);
    assert!(p.exceeds(100_000, 0));
    assert!(!p.exceeds(99_999, 0));
    // % seul : dépend de la fenêtre rapportée.
    let p = policy(true, None, Some(0.9));
    assert!(p.exceeds(90_000, 100_000));
    assert!(!p.exceeds(89_999, 100_000));
    // Pas de fenêtre connue (size 0) → la règle % est inerte.
    assert!(!p.exceeds(u64::MAX, 0));
    // OU : l'un des deux suffit.
    let p = policy(true, Some(50_000), Some(0.9));
    assert!(p.exceeds(50_000, 1_000_000)); // tokens seuls
    assert!(!p.exceeds(0, 0)); // aucun — used 0 sous les seuils
    assert!(p.exceeds(950_000, 1_000_000)); // pct seul
                                            // Désactivé → jamais de déclenchement.
    let p = policy(false, Some(1), Some(0.01));
    assert!(!p.exceeds(u64::MAX, u64::MAX));
}

/// Merge par champ : l'override agent ne remplace que ce qu'il pose,
/// le reste hérite du global.
#[test]
fn compact_merge_agent() {
    let mut by_agent = HashMap::new();
    by_agent.insert(
        "devin".to_string(),
        CompactPartial {
            enabled: None,
            at_tokens: Some(10_000),
            at_pct: None,
        },
    );
    by_agent.insert(
        "codex".to_string(),
        CompactPartial {
            enabled: Some(false),
            at_tokens: None,
            at_pct: None,
        },
    );
    let src = CompactSrc {
        global: CompactCfg {
            enabled: true,
            at_tokens: None,
            at_pct: Some(0.8),
        },
        by_agent,
    };
    // Agent sans override → global intégral.
    let p = src.base_for("claude");
    assert!(p.enabled && p.at_tokens.is_none() && p.at_pct == Some(0.8));
    // Override partiel : at_tokens posé, pct hérité.
    let p = src.base_for("devin");
    assert!(p.enabled && p.at_tokens == Some(10_000) && p.at_pct == Some(0.8));
    // Override enabled=false → coupe tout pour cet agent.
    let p = src.base_for("codex");
    assert!(!p.enabled && p.at_pct == Some(0.8));
}

/// Bornes : `0`/hors borne = règle « off » — un seuil nul ou un %
/// hors ]0,1] (TOML n'a pas de null, compact.json peut être
/// trafiqué à la main) ne doit jamais produire un seuil
/// toujours-vrai. Le retrait fonctionne aussi en override : un
/// `0` décoche la règle héritée.
#[test]
fn compact_bounds() {
    // Global : 0 = off, hors borne ignoré.
    let src = CompactSrc {
        global: CompactCfg {
            enabled: true,
            at_tokens: Some(0),
            at_pct: Some(0.0),
        },
        by_agent: HashMap::new(),
    };
    let p = src.base_for("x");
    assert!(p.enabled && p.at_tokens.is_none() && p.at_pct.is_none());
    assert!(!p.exceeds(u64::MAX, u64::MAX)); // inerte, jamais de trigger
    let src = CompactSrc {
        global: CompactCfg {
            enabled: true,
            at_tokens: None,
            at_pct: Some(1.5),
        },
        by_agent: HashMap::new(),
    };
    assert!(src.base_for("x").at_pct.is_none());
    // Override : Some(0)/hors borne retire la règle héritée.
    let mut p = policy(true, Some(100), Some(0.5));
    CompactPartial {
        enabled: None,
        at_tokens: Some(0),
        at_pct: Some(1.5),
    }
    .apply_to(&mut p);
    assert!(p.at_tokens.is_none() && p.at_pct.is_none());
    CompactPartial {
        enabled: None,
        at_tokens: Some(50),
        at_pct: Some(0.4),
    }
    .apply_to(&mut p);
    assert_eq!(p.at_tokens, Some(50));
    assert_eq!(p.at_pct, Some(0.4));
}

/// Défauts neutres (#320) : aucune valeur calibrée pour
/// l'environnement d'origine — workspace dérivé de la machine
/// (non vide, jamais « /workspace » en dur), routage vide,
/// agent/mode/modèle/effort vides (= résolus à l'usage).
#[test]
fn orch_defaults_env_neutres() {
    let c = OrchCfg::default();
    assert!(!c.workspace.is_empty(), "workspace must be env-derived");
    assert!(c.repo_workspaces.is_empty());
    assert!(c.agent.is_empty());
    assert!(c.mode.is_empty() && c.model.is_empty() && c.thought_level.is_empty());
    assert!(c.planner_mode.is_empty() && c.reviewer_mode.is_empty());
    assert!(c.implement_mode.is_empty() && c.pr_reviewer_mode.is_empty());
    assert!(
        c.state_dir.is_empty(),
        "state_dir reste vide — résolu à l'usage"
    );
    // Vide → défaut historique partagé avec le script legacy.
    assert_eq!(c.state_dir(), default_state_dir());
}

/// `workspace = ""` explicite (config.toml) = clé absente :
/// `normalize` y remet le défaut env — un champ cassant ne reste
/// jamais vide après le boot (OrchProbe::open).
#[test]
fn normalize_workspace_vide() {
    let mut c = OrchCfg {
        workspace: "  ".into(),
        ..Default::default()
    };
    c.normalize();
    assert_eq!(c.workspace, OrchCfg::default().workspace);
    // Non vide → inchangé.
    let mut c = OrchCfg {
        workspace: "/custom/ws".into(),
        ..Default::default()
    };
    c.normalize();
    assert_eq!(c.workspace, "/custom/ws");
}

/// `rules_files` édité à la main dans config.toml passe par
/// `normalize` sans `apply_patch` : les entrées hors forme
/// (absolu, `..`, vide) sont rejetées — globales ET overrides —
/// et une liste entièrement invalide retombe sur le défaut.
#[test]
fn normalize_rules_files_invalide() {
    // Globale : entrées invalides filtrées, valides conservées.
    let mut c: OrchCfg =
        toml::from_str(r#"rules_files = ["AGENTS.md", "/etc/motd", "../up.md", "docs/r.md"]"#)
            .unwrap();
    c.normalize();
    assert_eq!(c.rules_files, ["AGENTS.md", "docs/r.md"]);
    // Toute invalide → défaut ; séparateurs/drive Windows refusés
    // aussi (portable quel que soit l'OS qui vérifie).
    let mut c: OrchCfg = toml::from_str(
        r#"rules_files = ["/abs.md", "a/../b.md", "", "..\\up.md", "C:\\x\\r.md", "\\\\srv\\r.md", "C:r.md"]"#,
    )
    .unwrap();
    c.normalize();
    assert_eq!(c.rules_files, ["AGENTS.md", "CLAUDE.md"]);
    // Override par repo : même filtre, vide = hérite du global.
    let mut c: OrchCfg = toml::from_str(
        r#"[review."o/r"]
        rules_files = ["/etc/shadow", "docs/maintainer.md"]"#,
    )
    .unwrap();
    c.normalize();
    assert_eq!(c.rules_files_for("o/r"), ["docs/maintainer.md"]);
    let mut c: OrchCfg = toml::from_str(
        r#"[review."o/r"]
        rules_files = ["../x.md"]"#,
    )
    .unwrap();
    c.normalize();
    assert_eq!(c.rules_files_for("o/r"), ["AGENTS.md", "CLAUDE.md"]);
}

/// `state_dir` : chemin custom pris tel quel, `~` expandé comme
/// les cwd de session (expand_cwd).
#[test]
fn state_dir_custom_et_expansion() {
    let c = OrchCfg {
        state_dir: "/tmp/orch-state-x".into(),
        ..Default::default()
    };
    assert_eq!(c.state_dir(), std::path::PathBuf::from("/tmp/orch-state-x"));
    if let Some(home) = std::env::var_os("HOME") {
        let c = OrchCfg {
            state_dir: "~/orch-state".into(),
            ..Default::default()
        };
        assert_eq!(
            c.state_dir(),
            std::path::PathBuf::from(home).join("orch-state")
        );
    }
}

/// `agent_confs` (hot-reload) : ne lit que `[agents.*]` — le reste
/// de la config (sections orchestrateur, compact…) est ignoré même
/// s'il est présent et valide.
#[test]
fn agent_confs_ne_lit_que_agents() {
    let text = r#"
[agents.claude]
command = ["claude-code-acp"]

[agents.codex]
name = "Codex"
command = ["codex-acp"]
max_instances = 2
[agents.codex.env]
FOO = "bar"

[backlog_orchestrator]
enabled = true
agent = "devin"
"#;
    let confs = agent_confs(text).unwrap();
    assert_eq!(confs.len(), 2);
    assert_eq!(
        confs["claude"].command.argvs(),
        vec![vec!["claude-code-acp"]]
    );
    assert_eq!(confs["codex"].name.as_deref(), Some("Codex"));
    assert_eq!(confs["codex"].max_instances, Some(2));
    assert_eq!(
        confs["codex"].env.get("FOO").map(String::as_str),
        Some("bar")
    );
    // Section absente → table vide (≠ None : TOML valide sans agents).
    assert!(agent_confs("[backlog_orchestrator]\nenabled = true\n")
        .unwrap()
        .is_empty());
    // TOML invalide → None (l'appelant conserve les profils).
    assert!(agent_confs("ceci n'est pas du toml = [").is_none());
}

/// `command` en fallbacks : `[[argv], [argv]]` → plusieurs
/// candidates dans l'ordre ; la forme plate reste un seul argv.
#[test]
fn agent_command_fallbacks() {
    let confs = agent_confs(
        r#"[agents.claude]
command = [["claude-code-acp"], ["claude-agent-acp"]]

[agents.devin]
command = ["devin", "acp"]
"#,
    )
    .unwrap();
    assert_eq!(
        confs["claude"].command.argvs(),
        vec![vec!["claude-code-acp"], vec!["claude-agent-acp"]]
    );
    assert_eq!(confs["devin"].command.argvs(), vec![vec!["devin", "acp"]]);
    // profile_from_conf reporte les candidates telles quelles.
    let p = profile_from_conf("claude", confs["claude"].clone());
    assert_eq!(p.commands.len(), 2);
    assert_eq!(p.via, AgentVia::Config);
}

/// `profile_from_conf` : mapping AgentConf → AgentProfile — défauts
/// (name = id, max_instances = DEFAULT_MAX_INSTANCES) et recopie
/// intégrale des champs optionnels.
#[test]
fn profile_from_conf_mapping() {
    let conf: AgentConf = toml::from_str(
        r#"command = ["a", "b"]
name = "Mon Agent"
max_instances = 3
catalogue_cmd = "cat list"
model_override_cmd = "set -m {model}"
advisor_models = ["opus"]
extra_models = [{ value = "fable", label = "Fable" }]
[compact]
enabled = false
"#,
    )
    .unwrap();
    let p = profile_from_conf("x", conf);
    assert_eq!(p.name, "Mon Agent");
    assert_eq!(p.commands, vec![vec!["a", "b"]]);
    assert_eq!(p.via, AgentVia::Config);
    assert_eq!(p.max_instances, 3);
    assert_eq!(p.catalogue_cmd.as_deref(), Some("cat list"));
    assert_eq!(p.model_override_cmd.as_deref(), Some("set -m {model}"));
    assert_eq!(p.advisor_models, vec!["opus"]);
    assert_eq!(p.extra_models[0].value, "fable");
    assert!(!p.compact.enabled.unwrap_or(true));
    // Défauts : name = id, max_instances = plafond par défaut.
    let p = profile_from_conf(
        "y",
        toml::from_str::<AgentConf>(r#"command = ["c"]"#).unwrap(),
    );
    assert_eq!(p.name, "y");
    assert_eq!(p.max_instances, DEFAULT_MAX_INSTANCES);
}

/// env_mode : les ids de session ACP incompatibles env/CLI sont omis
/// (le boot CLI les refuserait) ; les valeurs du domaine env passent.
#[test]
fn env_mode_acp_vs_cli() {
    for acp_only in ["plan", "smart", "ask"] {
        assert_eq!(env_mode(acp_only), "", "{acp_only} doit être omis de l'env");
    }
    for cli_ok in [
        "bypass",
        "dangerous",
        "normal",
        "accept-edits",
        "autonomous",
        "yolo",
        "",
    ] {
        assert_eq!(env_mode(cli_ok), cli_ok, "{cli_ok} doit passer tel quel");
    }
    // Valeur arbitraire (autre agent, autre vocab) : conservée — le
    // filtre ne touche que les ids ACP connus pour casser le boot.
    assert_eq!(env_mode("custom-mode"), "custom-mode");
}
