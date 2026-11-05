//! Catalogue complet des modèles d'un profil agent : exécute la commande
//! `catalogue_cmd` de `[agents.*]` (ex. "devin models list") avec cache
//! TTL — la liste des variantes que l'agent n'annonce pas en ACP.
//! Provider sans `catalogue_cmd` → liste vide (comportement inchangé).

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Une entrée catalogue : uid persisté côté provider + nom affiché.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CatalogEntry {
    pub value: String,
    pub label: String,
}

pub type CatalogList = Vec<CatalogEntry>;

const TTL: Duration = Duration::from_secs(3600);
const PROBE_TIMEOUT: Duration = Duration::from_secs(25);

struct Entry {
    at: Instant,
    models: CatalogList,
}

/// Cache paresseux par profil — la sonde n'a lieu qu'à expiration.
/// Les échecs ne sont pas mis en cache : l'appel suivant réessaie.
#[derive(Default)]
pub struct CatalogueCache {
    inner: Mutex<std::collections::HashMap<String, Entry>>,
}

impl CatalogueCache {
    /// Modèles du profil — vide si `cmd` est None, si la sonde échoue
    /// ou si la sortie ne contient aucune ligne modèle parsable.
    pub async fn get(&self, agent: &str, cmd: Option<&str>) -> CatalogList {
        if let Some(c) = self.cached(agent) {
            return c;
        }
        let Some(argv) = cmd.and_then(argv_of) else {
            return CatalogList::new();
        };
        let models = tokio::task::spawn_blocking(move || probe(&argv))
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        if !models.is_empty() {
            self.inner.lock().unwrap().insert(
                agent.into(),
                Entry {
                    at: Instant::now(),
                    models: models.clone(),
                },
            );
        }
        models
    }

    fn cached(&self, agent: &str) -> Option<CatalogList> {
        let m = self.inner.lock().unwrap();
        m.get(agent)
            .filter(|e| e.at.elapsed() < TTL)
            .map(|e| e.models.clone())
    }
}

/// `catalogue_cmd` est une ligne de commande simple (« devin models
/// list ») : split whitespace, pas de shell — la config est de
/// confiance mais rester argv-vec évite toute injection par valeur.
fn argv_of(cmd: &str) -> Option<Vec<String>> {
    let argv: Vec<String> = cmd.split_whitespace().map(String::from).collect();
    (!argv.is_empty()).then_some(argv)
}

/// Exécute la commande catalogue avec borne de temps ; stdout lu sur un
/// thread dédié (un catalogue plus gros que le buffer du pipe bloquerait
/// l'enfant). stdin null : jamais de session/création implicite.
fn probe(argv: &[String]) -> Option<CatalogList> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut out = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = out.read_to_end(&mut b);
        b
    });
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return None;
            }
        }
    };
    let bytes = reader.join().unwrap_or_default();
    if !status.success() {
        return None;
    }
    Some(parse_catalog(&String::from_utf8_lossy(&bytes)))
}

/// Lignes « <uid>  <Nom>  [<détails>] » indentées (format `devin
/// models`) ; titres de famille, alias et mode d'emploi ne le sont
/// pas. Un uid = token [a-z0-9._-]+ en tête de ligne indentée.
fn parse_catalog(out: &str) -> CatalogList {
    let mut list = CatalogList::new();
    for line in out.lines() {
        if !line.starts_with(' ') && !line.starts_with('\t') {
            continue;
        }
        let t = line.trim();
        let mut it = t.split_whitespace();
        let Some(uid) = it.next() else { continue };
        if !uid
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
        {
            continue;
        }
        // Nom affiché = reste avant le bracket de détails, sinon le
        // reste brut ; jamais vide (uid seul → label=uid).
        let rest = t[uid.len()..].trim();
        let label = rest
            .find('[')
            .map(|i| rest[..i].trim())
            .filter(|s| !s.is_empty())
            .unwrap_or(if rest.is_empty() { uid } else { rest })
            .to_string();
        list.push(CatalogEntry {
            value: uid.to_string(),
            label,
        });
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_catalog_entries() {
        let out = "Available models (2 families)\n\
SWE-2 (swe-2)\n\
  aliases: swe\n\
  swe-2-high          SWE-2 High  [262K context, Free]\n\
  swe-2-flash-max     SWE-2 Flash Max  [262K context, $1 / 1M Input]\n\
Pass a family slug to `--model`.\n";
        let m = parse_catalog(out);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].value, "swe-2-high");
        assert_eq!(m[0].label, "SWE-2 High");
        assert_eq!(m[1].value, "swe-2-flash-max");
        assert_eq!(m[1].label, "SWE-2 Flash Max");
    }
}
