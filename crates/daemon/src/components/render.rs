//! Paramètres de composant : validation des valeurs selon le schéma
//! déclaré (`ParamSpec`) et rendu de la config utilisateur depuis un
//! template TOML à placeholders `{{key}}` (`ConfigRender`).
//!
//! Le mécanisme est générique : l'entrée catalogue déclare le schéma +
//! le template, la spec posée en hérite — `component_set_params` et
//! l'install n'ont alors plus besoin de connaître le composant.

use anyhow::{Context, Result};
use proto::{ComponentSpec, ParamKind, ParamSpec};
use std::collections::HashMap;

use super::catalog::{expand_home_str, write_home_file};

/// Paramètres `required` sans valeur effective (ni valeur posée ni
/// défaut) — tant que la liste n'est pas vide : `config_required` dans
/// le statut, rendu gelé (dernier état complet conservé) et spawn
/// refusé.
pub(super) fn missing_required(spec: &ComponentSpec) -> Vec<String> {
    spec.params_schema
        .iter()
        .filter(|p| p.required && effective(spec, &p.key).is_none())
        .map(|p| p.key.clone())
        .collect()
}

/// Valeur effective d'un paramètre : valeur posée (non vide), sinon
/// le défaut déclaré — `None` = non renseigné.
fn effective(spec: &ComponentSpec, key: &str) -> Option<String> {
    if let Some(v) = spec.params.get(key) {
        if !v.is_empty() {
            return Some(v.clone());
        }
    }
    spec.params_schema
        .iter()
        .find(|p| p.key == key)
        .and_then(|p| p.default.clone())
        .filter(|d| !d.is_empty())
}

/// Valeurs effectives complètes (défauts ∪ valeurs posées — la valeur
/// explicite prime, vide inclus = retrait explicite).
pub(super) fn effective_params(spec: &ComponentSpec) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for p in &spec.params_schema {
        if let Some(d) = &p.default {
            if !d.is_empty() {
                out.insert(p.key.clone(), d.clone());
            }
        }
    }
    for (k, v) in &spec.params {
        // Les specs sans schéma acceptent des params libres — inclus
        // tels quels (la boucle schéma ne les connaît pas).
        if v.is_empty() {
            out.remove(k);
        } else {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

/// Copie de la spec pour les réponses RPC : `params` remplacé par les
/// valeurs **effectives** (défauts ∪ valeurs posées) avec les `secret`
/// masqués — components.toml garde le clair (le fichier rendu
/// l'exige) mais aucun client ne doit pouvoir le relire par RPC.
pub(super) fn masked_spec(spec: &ComponentSpec) -> ComponentSpec {
    let mut s = spec.clone();
    s.params = effective_params(spec);
    for p in &s.params_schema {
        if p.secret && s.params.contains_key(&p.key) {
            s.params.insert(p.key.clone(), "••••••".into());
        }
    }
    s
}

/// Validation + normalisation d'une valeur pour `ps` — renvoie la
/// forme canonique stockée dans `spec.params`. La validation côté
/// schéma (clé déclarée) est faite par l'appelant (`set_params`).
pub(super) fn validate_value(ps: &ParamSpec, raw: &str) -> Result<String> {
    let v = raw.trim();
    match ps.kind {
        ParamKind::String | ParamKind::Path => {
            if let Some(re) = &ps.validation {
                let ok = regex::Regex::new(re)
                    .with_context(|| format!("validation « {} » invalide : {re}", ps.key))?
                    .is_match(v);
                anyhow::ensure!(
                    ok,
                    "« {} » : valeur « {v} » rejetée par la validation ({re})",
                    ps.key
                );
            }
            Ok(v.to_string())
        }
        ParamKind::Int => Ok(v
            .parse::<i64>()
            .with_context(|| format!("« {} » : entier attendu, reçu « {v} »", ps.key))?
            .to_string()),
        ParamKind::Duration => {
            anyhow::ensure!(
                valid_duration(v),
                "« {} » : durée invalide « {v} » (ex. 3h, 90min, 1h30m)",
                ps.key
            );
            Ok(v.to_string())
        }
        ParamKind::Bool => match v.to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Ok("true".into()),
            "false" | "no" | "off" | "0" => Ok("false".into()),
            _ => anyhow::bail!("« {} » : booléen attendu, reçu « {v} »", ps.key),
        },
        ParamKind::Enum => {
            anyhow::ensure!(
                ps.choices.iter().any(|c| c == v),
                "« {} » : « {v} » hors des choix ({})",
                ps.key,
                ps.choices.join(", ")
            );
            Ok(v.to_string())
        }
        ParamKind::List => Ok(v
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(",")),
    }
}

/// Durée humaine minimale : segments `nombre+unité` (ms|s|m|min|h|d|w)
/// éventuellement séparés d'espaces — « 3h », « 90min », « 1h 30m ».
fn valid_duration(v: &str) -> bool {
    const UNITS: [&str; 7] = ["ms", "min", "s", "m", "h", "d", "w"];
    let mut rest = v.trim();
    if rest.is_empty() {
        return false;
    }
    while !rest.is_empty() {
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits == 0 {
            return false;
        }
        rest = &rest[digits..];
        let unit = UNITS.iter().find(|u| rest.starts_with(**u));
        match unit {
            Some(u) => rest = rest[u.len()..].trim_start(),
            None => return false,
        }
    }
    true
}

/// Rend `spec.render` depuis les paramètres effectifs et écrit le
/// fichier (borné HOME, atomique, 0600 si un secret est déclaré).
/// `Ok(false)` = pas de rendu configuré, ou params `required` incomplets
/// — le fichier garde alors son dernier état complet.
pub(super) async fn render_if_ready(spec: &ComponentSpec) -> Result<bool> {
    let Some(render) = &spec.render else {
        return Ok(false);
    };
    if !missing_required(spec).is_empty() {
        return Ok(false);
    }
    let txt = render_template(&render.template, spec)
        .with_context(|| format!("rendu de « {} »", render.path))?;
    write_home_file(&render.path, &txt).await?;
    if spec.params_schema.iter().any(|p| p.secret) {
        let path = super::catalog::expand_home(&render.path);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                &path,
                std::fs::Permissions::from_mode(0o600),
            );
        }
    }
    Ok(true)
}

/// Substitution `{{key}}` → littéral TOML du paramètre. Une ligne dont
/// un placeholder reste sans valeur est omise (clé absente du fichier
/// = défaut du composant). Placeholder inconnu du schéma → erreur :
/// le template est audité côté catalogue, un `{{typo}}` ne doit pas
/// passer silencieusement.
fn render_template(tpl: &str, spec: &ComponentSpec) -> Result<String> {
    let mut out = String::with_capacity(tpl.len());
    // split('\n') d'un template fini par '\n' rend un segment vide de
    // trop — on le retire (les vides intermédiaires restent émis).
    let mut lines: Vec<&str> = tpl.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    for line in lines {
        let mut rendered = String::with_capacity(line.len());
        let mut rest = line;
        let mut drop_line = false;
        while let Some(start) = rest.find("{{") {
            rendered.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let end = after
                .find("}}")
                .with_context(|| "placeholder « {{ » non fermé")?;
            let key = after[..end].trim();
            let ps = spec
                .params_schema
                .iter()
                .find(|p| p.key == key)
                .with_context(|| format!("placeholder {{{{{key}}}}} hors schéma"))?;
            match effective(spec, key) {
                Some(v) => rendered.push_str(&toml_literal(ps, &v)?),
                None => {
                    drop_line = true;
                    break;
                }
            }
            rest = &after[end + 2..];
        }
        if !drop_line {
            rendered.push_str(rest);
            out.push_str(&rendered);
            out.push('\n');
        }
    }
    Ok(out)
}

/// Littéral TOML du paramètre — string/path/enum/duration → chaîne
/// échappée, int → nombre nu, bool → true/false, list → tableau.
fn toml_literal(ps: &ParamSpec, value: &str) -> Result<String> {
    let v = match ps.kind {
        ParamKind::Path => expand_home_str(value),
        _ => value.to_string(),
    };
    Ok(match ps.kind {
        ParamKind::Int => v
            .parse::<i64>()
            .with_context(|| format!("« {} » : entier attendu", ps.key))?
            .to_string(),
        ParamKind::Bool => v.to_string(), // déjà « true »/« false » (validate_value)
        ParamKind::List => format!(
            "[{}]",
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| toml_str(s))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => toml_str(&v),
    })
}

/// Chaîne TOML basique `"…"` — échappe `\`, `"` et les contrôles.
fn toml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::{ComponentKind, ComponentSpec, Healthcheck, RestartPolicy};

    fn ps(key: &str, kind: ParamKind, required: bool) -> ParamSpec {
        ParamSpec {
            key: key.into(),
            label: key.into(),
            description: None,
            required,
            kind,
            default: None,
            secret: false,
            validation: None,
            choices: vec![],
        }
    }

    fn spec_with(schema: Vec<ParamSpec>, params: &[(&str, &str)]) -> ComponentSpec {
        ComponentSpec {
            name: "t".into(),
            kind: ComponentKind::Process,
            command: vec!["sh".into()],
            env: HashMap::new(),
            cwd: None,
            autostart: false,
            restart: RestartPolicy::Never,
            restart_sec: 0,
            healthcheck: Healthcheck::None,
            description: None,
            queries: HashMap::new(),
            catalog: Some("t".into()),
            params_schema: schema,
            params: params
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            render: None,
        }
    }

    #[test]
    fn required_manquant_liste_les_cles() {
        let s = spec_with(
            vec![
                ps("repo", ParamKind::String, true),
                ps("group", ParamKind::String, false),
            ],
            &[],
        );
        assert_eq!(missing_required(&s), vec!["repo".to_string()]);
        let s = spec_with(
            vec![ps("repo", ParamKind::String, true)],
            &[("repo", "a/b")],
        );
        assert!(missing_required(&s).is_empty());
        // Un défaut comble un required.
        let mut p = ps("repo", ParamKind::String, true);
        p.default = Some("a/b".into());
        assert!(missing_required(&spec_with(vec![p], &[])).is_empty());
    }

    #[test]
    fn validate_par_kind() {
        let s = ps("i", ParamKind::Int, false);
        assert_eq!(validate_value(&s, " 42 ").unwrap(), "42");
        assert!(validate_value(&s, "x").is_err());
        let b = ps("b", ParamKind::Bool, false);
        assert_eq!(validate_value(&b, "yes").unwrap(), "true");
        assert!(validate_value(&b, "peut-être").is_err());
        let d = ps("d", ParamKind::Duration, false);
        assert_eq!(validate_value(&d, "3h").unwrap(), "3h");
        assert_eq!(validate_value(&d, "1h30m").unwrap(), "1h30m");
        assert!(validate_value(&d, "un moment").is_err());
        let mut e = ps("e", ParamKind::Enum, false);
        e.choices = vec!["a".into(), "b".into()];
        assert_eq!(validate_value(&e, "b").unwrap(), "b");
        assert!(validate_value(&e, "c").is_err());
        let l = ps("l", ParamKind::List, false);
        assert_eq!(validate_value(&l, "a, b ,, c").unwrap(), "a,b,c");
    }

    #[test]
    fn validation_regex_appliquee() {
        let mut p = ps("repo", ParamKind::String, true);
        p.validation = Some(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$".into());
        assert!(validate_value(&p, "Wameuh/AgentControl").is_ok());
        assert!(validate_value(&p, "sans-slash").is_err());
    }

    #[test]
    fn rendu_types_et_lignes_omises() {
        let mut cmd = ps("agent.command", ParamKind::String, false);
        cmd.default = Some("devin".into());
        let mut repo = ps("repo", ParamKind::String, true);
        repo.validation = Some(r"^[\w.-]+/[\w.-]+$".into());
        let spec = spec_with(
            vec![
                repo,
                ps("max_parallel", ParamKind::Int, false),
                ps("profile", ParamKind::String, false),
                cmd,
            ],
            &[("repo", "a/b"), ("max_parallel", "2")],
        );
        let tpl = "repo = {{repo}}\nprofile = {{profile}}\nmax = {{max_parallel}}\n[agent]\ncommand = {{agent.command}}\n";
        let out = render_template(tpl, &spec).unwrap();
        assert_eq!(
            out,
            "repo = \"a/b\"\nmax = 2\n[agent]\ncommand = \"devin\"\n"
        );
    }

    #[test]
    fn rendu_echappe_et_placeholder_inconnu() {
        let spec = spec_with(
            vec![ps("k", ParamKind::String, false)],
            &[("k", "a\"b\\c")],
        );
        let out = render_template("v = {{k}}\n", &spec).unwrap();
        assert_eq!(out, "v = \"a\\\"b\\\\c\"\n");
        assert!(render_template("v = {{inconnu}}\n", &spec).is_err());
    }

    #[test]
    fn spec_masquee_pour_rpc() {
        let mut p = ps("token", ParamKind::String, false);
        p.secret = true;
        let spec = spec_with(vec![p], &[("token", "shhh")]);
        let m = masked_spec(&spec);
        assert_eq!(m.params["token"], "••••••");
        assert_eq!(spec.params["token"], "shhh"); // l'originale garde le clair
    }
}
