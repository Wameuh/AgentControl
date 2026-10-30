//! Messages épinglés : liste persistée dans `pinned.json` (dossier de
//! session) — survit au reload du client et visible de tout client
//! connecté à la session. L'ancre est le couple (seq, ts) de l'event :
//! un reset de journal (/clear, /new, rebind) fait repartir les seqs à 1
//! → l'épinglé est conservé avec son contenu copié plutôt que purgé
//! (choix documenté : perdre l'épinglé avec le transcript serait
//! surprenant — l'extrait reste consultable, simplement `live:false`,
//! sans saut possible vers un message disparu).

use super::*;
use proto::PinEntry;
use serde_json::json;

/// Borne anti-abus par session : au-delà, `pin_message` renvoie une
/// erreur — les épinglés restent un repère, pas un second journal.
pub(crate) const PINS_MAX: usize = 50;
/// Le texte copié est un extrait pour le panneau — borné pour ne pas
/// recopier une sortie de tool géante dans pinned.json.
const PIN_TEXT_MAX: usize = 2_000;

impl Session {
    /// Charge pinned.json — absent/vide → liste vide ; contenu
    /// illisible → ignoré (un fichier corrompu n'empêche pas
    /// l'ouverture de la session).
    fn load_pins(dir: &std::path::Path) -> Vec<PinEntry> {
        std::fs::read_to_string(dir.join("pinned.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// pinned.json initialisé au chargement de la session — appelé par
    /// `Session::open` pour remplir le champ `pinned`.
    pub(super) fn open_pins(dir: &std::path::Path) -> Mutex<Vec<PinEntry>> {
        Mutex::new(Self::load_pins(dir))
    }

    fn save_pins(&self) {
        if let Ok(p) = self.pinned.lock() {
            // .tmp + rename : pas de pinned.json tronqué si le démon
            // meurt en pleine écriture.
            let tmp = self.dir.join("pinned.json.tmp");
            if std::fs::write(&tmp, serde_json::to_string_pretty(&*p).unwrap_or_default()).is_ok() {
                let _ = std::fs::rename(&tmp, self.dir.join("pinned.json"));
            }
        }
    }

    /// Épingle l'event de `seq` : il doit exister dans le journal et
    /// porter du texte (searchable_text non vide — messages user/agent
    /// en pratique, mais tout event à contenu est accepté). Idempotent :
    /// un seq déjà épinglé rend l'entrée existante.
    pub fn pin(&self, seq: u64) -> Result<PinEntry> {
        {
            let p = self.pinned.lock().unwrap();
            if let Some(e) = p.iter().find(|e| e.seq == seq) {
                return Ok(e.clone());
            }
        }
        let ev = self
            .journal
            .get(&self.dir, seq)?
            .with_context(|| format!("aucun event au seq {seq}"))?;
        let text = ev.kind.searchable_text();
        if text.trim().is_empty() {
            anyhow::bail!(
                "event #{seq} sans contenu épinglable ({})",
                ev.kind.type_tag()
            );
        }
        let entry = PinEntry {
            seq,
            ts: ev.ts,
            kind: ev.kind.type_tag().into(),
            text: text.chars().take(PIN_TEXT_MAX).collect(),
            pinned_at: proto::now_iso(),
        };
        {
            let mut p = self.pinned.lock().unwrap();
            // Course entre le check ci-dessus et l'insert (deux clients
            // qui épinglent le même seq) → l'existant gagne.
            if let Some(e) = p.iter().find(|e| e.seq == seq) {
                return Ok(e.clone());
            }
            if p.len() >= PINS_MAX {
                anyhow::bail!("limite de {PINS_MAX} messages épinglés atteinte");
            }
            p.push(entry.clone());
        }
        self.save_pins();
        Ok(entry)
    }

    /// Retire l'épinglé `seq` — true s'il était présent.
    pub fn unpin(&self, seq: u64) -> bool {
        let removed = {
            let mut p = self.pinned.lock().unwrap();
            let n = p.len();
            p.retain(|e| e.seq != seq);
            p.len() != n
        };
        if removed {
            self.save_pins();
        }
        removed
    }

    /// Liste des épinglés, `live` recalculé à chaque appel : l'event
    /// source est encore au même seq du journal courant ET avec le ts
    /// copié (le discriminant écarte la collision d'un event du cycle
    /// suivant au même seq après /clear). Un épinglé mort garde son
    /// extrait — affiché mais non sautable.
    pub fn pins_snapshot(&self) -> Vec<Value> {
        self.pinned
            .lock()
            .unwrap()
            .iter()
            .map(|e| {
                let live = matches!(
                    self.journal.get(&self.dir, e.seq),
                    Ok(Some(ev)) if ev.ts == e.ts && ev.kind.type_tag() == e.kind
                );
                json!({
                    "seq": e.seq,
                    "ts": e.ts,
                    "type": e.kind,
                    "text": e.text,
                    "pinned_at": e.pinned_at,
                    "live": live,
                })
            })
            .collect()
    }
}
