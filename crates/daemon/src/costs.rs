//! Coûts des modèles par profil agent : sonde la commande catalogue du
//! provider (`devin models`…) avec cache TTL, exposée via
//! `ReqKind::ModelCosts` pour les suffixes de prix des selects du client.
//! Provider sans source connue → map vide (le client n'affiche rien,
//! comportement inchangé).

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Prix d'un modèle : `label` compact pour le libellé d'option
/// (« Free », « $1/$5 »), `detail` = texte catalogue brut pour le tooltip.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelCost {
    pub label: String,
    pub detail: String,
}

pub type CostMap = HashMap<String, ModelCost>;

const TTL: Duration = Duration::from_secs(3600);
const PROBE_TIMEOUT: Duration = Duration::from_secs(25);

struct Entry {
    at: Instant,
    costs: CostMap,
}

/// Cache paresseux par profil agent — la sonde (process externe) n'a
/// lieu qu'à expiration, jamais à chaque rendu côté client. Les échecs
/// de sonde ne sont pas mis en cache : l'appel suivant réessaie.
#[derive(Default)]
pub struct CostCache {
    inner: Mutex<HashMap<String, Entry>>,
}

impl CostCache {
    /// Coûts du profil `agent` (commande `command`) — map vide si le
    /// provider n'a pas de source de coût connue ou si la sonde échoue.
    pub async fn get(&self, agent: &str, command: &[String]) -> CostMap {
        if let Some(c) = self.cached(agent) {
            return c;
        }
        let Some(argv) = catalog_argv(command) else {
            return CostMap::new();
        };
        let costs = tokio::task::spawn_blocking(move || probe(&argv))
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        if !costs.is_empty() {
            self.inner.lock().unwrap().insert(
                agent.into(),
                Entry {
                    at: Instant::now(),
                    costs: costs.clone(),
                },
            );
        }
        costs
    }

    fn cached(&self, agent: &str) -> Option<CostMap> {
        let m = self.inner.lock().unwrap();
        m.get(agent)
            .filter(|e| e.at.elapsed() < TTL)
            .map(|e| e.costs.clone())
    }
}

/// Commande catalogue tarifé d'un profil : le binaire du profil + ses
/// arguments de listing (`devin models`). None = provider sans source
/// de coût connue → comportement inchangé.
fn catalog_argv(command: &[String]) -> Option<Vec<String>> {
    let bin = command.first()?;
    let base = Path::new(bin)
        .file_name()?
        .to_string_lossy()
        .to_lowercase();
    match base.as_str() {
        "devin" => Some(vec![bin.clone(), "models".into()]),
        _ => None,
    }
}

/// Exécute la commande catalogue avec une borne de temps (une CLI qui
/// attend une auth interactive ne doit pas pendre le handler RPC).
fn probe(argv: &[String]) -> Option<CostMap> {
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // stdout lu sur un thread dédié : sans ça un catalogue plus gros
    // que le buffer du pipe bloquerait l'enfant avant sa sortie.
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
    Some(parse_devin_models(&String::from_utf8_lossy(&bytes)))
}

/// `devin models` : lignes « <uid>  <Nom>  [<contexte>, <prix>…] »
/// indentées ; les titres de famille et le mode d'emploi ne le sont
/// pas. Le bracket final porte contexte/prix/free — parsé par `price_of`.
fn parse_devin_models(out: &str) -> CostMap {
    let mut map = CostMap::new();
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
            continue; // « aliases: », « Pass a family slug… » & co.
        }
        let Some(lb) = t.rfind('[') else { continue };
        let Some(rb) = t[lb..].rfind(']') else { continue };
        let detail = t[lb + 1..lb + rb].trim().to_string();
        let Some(label) = price_of(&detail) else { continue };
        map.insert(
            uid.to_string(),
            ModelCost { label, detail },
        );
    }
    map
}

/// Extrait le prix d'un bracket catalogue : « $X / 1M Input ·
/// $Y / 1M Output » → « $X/$Y » ; « Free » seul → « Free ». None =
/// format inconnu → pas d'affichage plutôt qu'un prix inventé.
/// Le contexte est un item à virgules du même segment que le premier
/// prix (« 262K context, $1 / 1M Input ») → recherche par motif, pas
/// par position d'item.
fn price_of(detail: &str) -> Option<String> {
    let input = price_num(detail, "Input");
    let output = price_num(detail, "Output");
    if let (Some(i), Some(o)) = (&input, &output) {
        return Some(format!("${i}/${o}"));
    }
    // « Free » n'est reconnu que comme token entier (virgule ou ·) —
    // « Sidekick: Free » ne doit PAS le déclencher.
    let free = detail
        .split(|c| c == '·' || c == ',')
        .any(|t| t.trim() == "Free");
    free.then(|| "Free".to_string())
}

/// « $<num> / 1M <kind> » → « <num> » — kind = Input|Output. Le
/// « Cached input » ne matche pas « / 1M Input » (case + préfixe).
fn price_num(detail: &str, kind: &str) -> Option<String> {
    let needle = format!("/ 1M {kind}");
    let pos = detail.find(&needle)?;
    let before = &detail[..pos];
    let dollar = before.rfind('$')?;
    let num = before[dollar + 1..].trim();
    if num.is_empty()
        || !num
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == '-')
    {
        return None;
    }
    Some(num.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"Available models (2 families)

SWE-2 (swe-2)
  aliases: swe
  swe-2-high          SWE-2 High  [262K context, Free]
  swe-2-medium-lightning  SWE-2 Medium Lightning  [262K context, $1 / 1M Input · $0.1 / 1M Cached input · $5 / 1M Output]
  fusion-a-sidekick-b  Fusion (A + B)  [1M context, $4 / 1M Input · $0.2 / 1M Cached input · $20 / 1M Output · Sidekick: Free]

Pass a family slug, alias, or model UID to `--model`.
"#;

    #[test]
    fn parse_labels() {
        let m = parse_devin_models(SAMPLE);
        assert_eq!(m["swe-2-high"].label, "Free");
        assert_eq!(m["swe-2-medium-lightning"].label, "$1/$5");
        assert_eq!(m["fusion-a-sidekick-b"].label, "$4/$20");
        assert_eq!(m.len(), 3);
        assert!(m["swe-2-medium-lightning"]
            .detail
            .contains("Cached input"));
    }
}
