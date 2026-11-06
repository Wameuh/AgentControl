//! Sondes de version de l'outil managé + seuil `min_version`.
//!
//! `version_argv`/`min_version` sont des champs de spec que le
//! catalogue préremplit (`suggested_spec`) ; pour un composant adossé
//! installé avant leur introduction, l'entrée homonyme fait office de
//! repli — merge à la lecture, la spec persistée n'est pas réécrite.

use proto::{ComponentKind, ComponentSpec};

/// argv de la sonde de version effective : `version_argv` de la spec
/// > `version_argv` de l'entrée catalogue homonyme > `command` d'un
/// `tool` (sa commande *est* sa sonde version). `~` est expansé par
/// `run_version_probe` comme pour les autres argv de la spec.
pub(super) fn version_argv(spec: &ComponentSpec) -> Option<Vec<String>> {
    if let Some(argv) = &spec.version_argv {
        if !argv.is_empty() {
            return Some(argv.clone());
        }
    }
    if let Some(e) = super::catalog::entry_named(&spec.name) {
        if let Some(argv) = &e.suggested_spec.version_argv {
            if !argv.is_empty() {
                return Some(argv.clone());
            }
        }
    }
    (spec.kind == ComponentKind::Tool && !spec.command.is_empty()).then(|| spec.command.clone())
}

/// Seuil `min_version` effectif : valeur de la spec, sinon celle de
/// l'entrée catalogue homonyme (composants adossés dont la spec
/// persistée date d'avant le champ).
pub(super) fn min_version(spec: &ComponentSpec) -> Option<String> {
    spec.min_version.clone().or_else(|| {
        super::catalog::entry_named(&spec.name).and_then(|e| e.suggested_spec.min_version.clone())
    })
}

/// Composantes numériques du premier groupe de chiffres pointés
/// (« agent-reviewer 0.3.1 » → [0,3,1] ; « v2 » → [2]) — le préfixe
/// textuel et le suffixe (qualifier, build metadata) sont ignorés.
/// Aucun chiffre → None.
pub(super) fn nums(s: &str) -> Option<Vec<u64>> {
    let i = s.find(|c: char| c.is_ascii_digit())?;
    let run: String = s[i..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let v: Option<Vec<u64>> = run
        .split('.')
        .filter(|p| !p.is_empty())
        .map(|p| p.parse().ok())
        .collect();
    v.filter(|v| !v.is_empty())
}

/// `probed` sous le seuil `min` — comparaison par composantes
/// numériques complétées à 0 : « 0.2.0 » < « 0.3 », « 0.3.1 » ≥ « 0.3 ».
/// Une version indéchiffrable (sonde ou seuil sans chiffre) ne
/// déclare jamais l'obsolescence — pas de faux badge.
pub(super) fn version_below_min(probed: &str, min: &str) -> bool {
    let (Some(v), Some(m)) = (nums(probed), nums(min)) else {
        return false;
    };
    let n = v.len().max(m.len());
    let pad = |x: &[u64]| {
        (0..n)
            .map(|i| x.get(i).copied().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    pad(&v) < pad(&m)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le parse extrait le premier groupe de chiffres, ignore le
    /// préfixe texte et le suffixe, et n'invente rien sans chiffre.
    #[test]
    fn nums_premier_groupe_de_chiffres() {
        assert_eq!(nums("agent-reviewer 0.3.1").unwrap(), [0, 3, 1]);
        assert_eq!(nums("v2").unwrap(), [2]);
        assert_eq!(nums("0.3.0-rc.1").unwrap(), [0, 3, 0]);
        assert_eq!(nums("sans chiffre"), None);
        assert_eq!(nums(""), None);
    }

    /// Comparaison souple : composantes manquantes = 0, ordre
    /// numérique (0.10 > 0.3), jamais d'obsolescence sur entrée
    /// indéchiffrable.
    #[test]
    fn version_below_min_comparaison() {
        assert!(version_below_min("agent-reviewer 0.2.0", "0.3"));
        assert!(!version_below_min("agent-reviewer 0.3.0", "0.3"));
        assert!(!version_below_min("agent-reviewer 0.3.1", "0.3"));
        assert!(!version_below_min("0.10.0", "0.3"));
        assert!(version_below_min("1.9", "2"));
        assert!(!version_below_min("illiso", "0.3"));
        assert!(!version_below_min("0.2", "sans"));
    }
}
