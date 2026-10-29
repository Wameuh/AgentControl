//! Journal append-only par session : events.jsonl + blobs/.
//! Index en mémoire seq -> offset octet (~16 o/evt) : les lectures sont en
//! seek direct, pas de scan complet. Compaction possible en gardant la queue.

use anyhow::{Context, Result};
use proto::{Event, EventKind};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing::info;

struct Inner {
    file: File,
    /// (seq, offset de début de ligne) — trié par seq croissant.
    /// Supporte les trous (après compaction) via recherche binaire.
    offsets: Vec<(u64, u64)>,
    /// Position de fin de fichier (prochain offset d'écriture).
    end: u64,
    last_seq: u64,
    /// Génération du journal : incrémentée par `reset()`. Les seqs
    /// seules ne suffisent pas à détecter un vidage (le nouveau
    /// journal peut déjà dépasser l'ancien `last_seq` vu côté
    /// subscriber) — la génération le fait sans ambiguïté. Volatile :
    /// elle n'a de sens que dans la durée de vie du démon.
    gen: u64,
}

pub struct Journal {
    inner: Mutex<Inner>,
}

/// Résultat du scan « premier post » : le journal ne garde que le
/// cycle courant, mais la compaction peut avoir coupé la tête —
/// présenter le premier `user_message` restant comme « premier »
/// serait un faux.
pub enum FirstPost {
    /// Premier `user_message` du cycle (seq/ts/texte intacts).
    Found(Event),
    /// La tête du journal a été supprimée par la compaction.
    Truncated,
    /// Pas de `user_message` dans la fenêtre scannée.
    Empty,
}

impl Journal {
    /// Ouvre (ou crée) events.jsonl : un seul scan pour bâtir l'index.
    pub fn open(dir: &Path) -> Result<Self> {
        let path = dir.join("events.jsonl");
        let mut offsets = Vec::new();
        let mut last_seq = 0u64;
        let mut end = 0u64;
        if path.exists() {
            let f = File::open(&path)?;
            let mut r = BufReader::new(f);
            let mut line = String::new();
            loop {
                let start = end;
                line.clear();
                let n = r.read_line(&mut line)?;
                if n == 0 {
                    break;
                }
                if !line.trim().is_empty() {
                    if let Ok(ev) = serde_json::from_str::<Event>(&line) {
                        offsets.push((ev.seq, start));
                        last_seq = ev.seq;
                    }
                }
                end += n as u64;
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            inner: Mutex::new(Inner {
                file,
                offsets,
                end,
                last_seq,
                gen: 0,
            }),
        })
    }

    pub fn last_seq(&self) -> u64 {
        self.inner.lock().unwrap().last_seq
    }

    /// Génération courante — voir `Inner::gen`.
    pub fn gen(&self) -> u64 {
        self.inner.lock().unwrap().gen
    }

    /// Ajoute un événement. Doit être suivi d'un broadcast.
    pub fn append(&self, kind: EventKind) -> Result<Event> {
        let mut g = self.inner.lock().unwrap();
        let ev = Event {
            seq: g.last_seq + 1,
            ts: proto::now_iso(),
            kind,
        };
        let mut line = serde_json::to_string(&ev)?;
        line.push('\n');
        let off = g.end;
        g.offsets.push((ev.seq, off));
        g.file.write_all(line.as_bytes())?;
        g.file.flush()?;
        g.end += line.len() as u64;
        g.last_seq = ev.seq;
        Ok(ev)
    }

    /// Ouvre un handle de lecture positionné à `offset` et lit les events
    /// jusqu'à `stop_before` (exclu) ou EOF.
    fn read_from(dir: &Path, offset: u64, stop_before: u64, cap: usize) -> Result<Vec<Event>> {
        let path = dir.join("events.jsonl");
        let mut f = File::open(&path)?;
        f.seek(SeekFrom::Start(offset))?;
        let mut out = Vec::new();
        for line in BufReader::new(f).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(ev) = serde_json::from_str::<Event>(&line) {
                if ev.seq >= stop_before {
                    break;
                }
                out.push(ev);
                if out.len() >= cap {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Les `limit` derniers événements avec seq < before_seq
    /// (before_seq None/0 → depuis la fin).
    pub fn read_page(&self, dir: &Path, before_seq: Option<u64>, limit: usize) -> Result<Vec<Event>> {
        let before = before_seq.filter(|b| *b > 0).unwrap_or(u64::MAX);
        let (start_off, n) = {
            let g = self.inner.lock().unwrap();
            if g.offsets.is_empty() {
                // Index mémoire vide (session chargée au boot du démon,
                // jamais ré-indexée) → scan complet du journal.
                drop(g);
                let v = Self::read_from(dir, 0, before, usize::MAX)?;
                let keep = v.len().saturating_sub(limit);
                return Ok(v.into_iter().skip(keep).collect());
            }
            // nb d'events avec seq < before
            let n = g.offsets.partition_point(|(s, _)| *s < before);
            let start = n.saturating_sub(limit);
            let off = if start < n { g.offsets[start].1 } else { return Ok(vec![]) };
            (off, n - start)
        };
        Self::read_from(dir, start_off, before, n)
    }

    /// Page ascendante : les `limit` événements avec seq > `after_seq`,
    /// en ordre croissant — pagination vers le présent depuis une vue
    /// ancrée en plein journal (saut vers un résultat de recherche).
    pub fn read_page_after(&self, dir: &Path, after_seq: u64, limit: usize) -> Result<Vec<Event>> {
        let start_off = {
            let g = self.inner.lock().unwrap();
            if g.offsets.is_empty() {
                // Index mémoire vide → scan complet, même repli que read_page.
                drop(g);
                let v = Self::read_from(dir, 0, u64::MAX, usize::MAX)?;
                return Ok(v
                    .into_iter()
                    .filter(|e| e.seq > after_seq)
                    .take(limit)
                    .collect());
            }
            let idx = g.offsets.partition_point(|(s, _)| *s <= after_seq);
            if idx >= g.offsets.len() {
                return Ok(vec![]);
            }
            g.offsets[idx].1
        };
        Self::read_from(dir, start_off, u64::MAX, limit)
    }

    /// Vide le journal et les blobs — utilisé quand la session se rebinde
    /// vers une AUTRE session native : l'historique local n'est plus valide
    /// et sera re-rempli par le replay de `session/load`.
    pub fn reset(&self, dir: &Path) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        let path = dir.join("events.jsonl");
        File::create(&path)?; // truncate
        g.file = OpenOptions::new().append(true).open(&path)?;
        g.offsets.clear();
        g.end = 0;
        g.last_seq = 0;
        g.gen += 1;
        let bdir = dir.join("blobs");
        if bdir.exists() {
            let _ = std::fs::remove_dir_all(&bdir);
        }
        std::fs::create_dir_all(&bdir)?;
        Ok(())
    }

    /// Premier `user_message` du journal (cycle courant) — scan de la
    /// tête en flux, borné par FIRST_POST_SCAN_MAX lignes : le journal
    /// peut faire des centaines de Mo, on ne le charge jamais entier.
    /// La compaction supprime la TÊTE du fichier : sans détection, le
    /// premier `user_message` restant serait un prompt de milieu de
    /// cycle présenté comme « premier post » → Truncated dans ce cas.
    pub fn first_post(&self, dir: &Path) -> Result<FirstPost> {
        /// Le prelude d'un cycle (status/system/commands post-spawn) ne
        /// fait que quelques events — la borne est très large au-delà.
        const FIRST_POST_SCAN_MAX: usize = 4096;
        // Tête perdue à la compaction ? L'index connaît le plus petit
        // seq retenu — s'il n'est pas 1, tout le début a été coupé
        // (les seqs ne sont jamais renumérotées).
        let first_seq = self.inner.lock().unwrap().offsets.first().map(|(s, _)| *s);
        match first_seq {
            Some(1) | None => {} // tête intacte, ou index vide → le scan décide
            Some(_) => return Ok(FirstPost::Truncated),
        }
        let path = dir.join("events.jsonl");
        let f = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(FirstPost::Empty),
            Err(e) => return Err(e.into()),
        };
        let mut first_seen = false;
        for (n, line) in BufReader::new(f).lines().enumerate() {
            if n >= FIRST_POST_SCAN_MAX {
                break;
            }
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let Ok(ev) = serde_json::from_str::<Event>(&line) else {
                continue;
            };
            // Repli du test d'index (index jamais peuplé) : le premier
            // event du fichier n'est pas seq 1 → la tête a été coupée.
            if !std::mem::replace(&mut first_seen, true) && ev.seq != 1 {
                return Ok(FirstPost::Truncated);
            }
            if matches!(ev.kind, EventKind::UserMessage { .. }) {
                return Ok(FirstPost::Found(ev));
            }
        }
        Ok(FirstPost::Empty)
    }

    /// Rattrapage borné : les events avec seq > after_seq, mais au plus les
    /// `cap` derniers — les plus vieux sont comptés dans `skipped` et
    /// restent atteignables via `read_page` (pagination arrière). Un
    /// subscribe sur un after_seq très ancien ne rejoue ainsi jamais un
    /// journal entier, qui peut faire des centaines de Mo.
    pub fn read_tail(&self, dir: &Path, after_seq: u64, cap: usize) -> Result<(Vec<Event>, u64)> {
        let (start_off, skipped) = {
            let g = self.inner.lock().unwrap();
            if g.offsets.is_empty() {
                // Index mémoire vide → scan complet, même repli que read_page.
                drop(g);
                let v = Self::read_from(dir, 0, u64::MAX, usize::MAX)?;
                let v: Vec<Event> = v.into_iter().filter(|e| e.seq > after_seq).collect();
                let skipped = v.len().saturating_sub(cap) as u64;
                let keep = v.len().saturating_sub(cap);
                return Ok((v.into_iter().skip(keep).collect(), skipped));
            }
            let idx = g.offsets.partition_point(|(s, _)| *s <= after_seq);
            if idx >= g.offsets.len() {
                return Ok((vec![], 0));
            }
            let remaining = g.offsets.len() - idx;
            if remaining > cap {
                let start = g.offsets.len() - cap;
                (g.offsets[start].1, (start - idx) as u64)
            } else {
                (g.offsets[idx].1, 0)
            }
        };
        Ok((Self::read_from(dir, start_off, u64::MAX, cap)?, skipped))
    }

    /// Si le fichier dépasse `max_bytes`, réécrit en gardant seulement les
    /// derniers `keep_bytes` (aligné sur une ligne) et reconstruit l'index.
    /// Retourne true si compacté.
    pub fn compact_if_needed(&self, dir: &Path, max_bytes: u64, keep_bytes: u64) -> Result<bool> {
        let mut g = self.inner.lock().unwrap();
        let len = g.file.metadata()?.len();
        if len <= max_bytes || g.offsets.is_empty() {
            return Ok(false);
        }
        let path = dir.join("events.jsonl");
        // Cherche le premier offset >= len - keep_bytes (aligné sur un event)
        let cutoff_seq_off = {
            let target = len.saturating_sub(keep_bytes);
            let i = g.offsets.partition_point(|(_, o)| *o < target);
            if i >= g.offsets.len() {
                return Ok(false); // un seul event énorme, on le garde
            }
            g.offsets[i].1
        };

        // Copie la queue dans un fichier temporaire puis rename atomique
        let tmp = dir.join("events.jsonl.tmp");
        {
            let mut src = File::open(&path)?;
            src.seek(SeekFrom::Start(cutoff_seq_off))?;
            let mut dst = File::create(&tmp)?;
            std::io::copy(&mut src, &mut dst)?;
            dst.flush()?;
        }
        std::fs::rename(&tmp, &path)?;

        // Rouvre le handle append + reconstruit l'index sans rescanner le JSON
        // (on connaît les seqs via l'ancien index).
        let mut new_offsets = Vec::new();
        let mut pos = 0u64;
        {
            let f = File::open(&path)?;
            let mut r = BufReader::new(f);
            let mut line = String::new();
            // seqs conservés : ceux dont l'offset >= cutoff_seq_off
            let kept: Vec<u64> = g
                .offsets
                .iter()
                .filter(|(_, o)| *o >= cutoff_seq_off)
                .map(|(s, _)| *s)
                .collect();
            let mut ki = kept.iter().peekable();
            loop {
                let start = pos;
                line.clear();
                let n = r.read_line(&mut line)?;
                if n == 0 {
                    break;
                }
                if !line.trim().is_empty() {
                    if let Some(&&seq) = ki.peek() {
                        new_offsets.push((seq, start));
                        ki.next();
                    } else if let Ok(ev) = serde_json::from_str::<Event>(&line) {
                        new_offsets.push((ev.seq, start));
                    }
                }
                pos += n as u64;
            }
        }
        let removed = g.offsets.len() - new_offsets.len();
        g.offsets = new_offsets;
        g.end = pos;
        g.file = OpenOptions::new().append(true).open(&path)?;
        info!("journal compacté : -{removed} events, {len} -> {pos} octets");
        Ok(true)
    }
}

/// Écrit (append) un blob de sortie volumineux. Retourne le nom relatif.
pub fn write_blob(dir: &Path, name: &str, data: &[u8]) -> Result<String> {
    let bdir = dir.join("blobs");
    std::fs::create_dir_all(&bdir)?;
    // Sanitize : pas de traversal
    let safe: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .collect();
    let path = bdir.join(&safe);
    let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
    f.write_all(data)?;
    Ok(format!("blobs/{safe}"))
}

/// Lit une tranche de blob [offset, offset+limit). Retourne (data, total_size).
pub fn read_blob(dir: &Path, rel: &str, offset: u64, limit: u64) -> Result<(Vec<u8>, u64)> {
    let path = dir.join(rel);
    let meta = std::fs::metadata(&path).with_context(|| format!("blob absent ou purgé: {rel}"))?;
    let total = meta.len();
    let mut f = File::open(&path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; limit.min(4 * 1024 * 1024) as usize];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    Ok((buf, total))
}

/// Purge les plus vieux blobs tant que le total dépasse `max_bytes`.
/// Retourne le nombre de fichiers supprimés.
pub fn purge_blobs(dir: &Path, max_bytes: u64) -> Result<usize> {
    let bdir = dir.join("blobs");
    if !bdir.exists() {
        return Ok(0);
    }
    let mut files: Vec<(u64, std::time::SystemTime, PathBuf)> = Vec::new();
    let mut total = 0u64;
    for e in std::fs::read_dir(&bdir)? {
        let e = e?;
        if let Ok(m) = e.metadata() {
            if m.is_file() {
                total += m.len();
                files.push((m.len(), m.modified().unwrap_or(std::time::UNIX_EPOCH), e.path()));
            }
        }
    }
    if total <= max_bytes {
        return Ok(0);
    }
    files.sort_by_key(|(_, t, _)| *t); // plus vieux d'abord
    let mut removed = 0;
    for (len, _, path) in files {
        if total <= max_bytes {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= len;
            removed += 1;
        }
    }
    Ok(removed)
}

pub fn session_dir(root: &Path, id: &str) -> PathBuf {
    root.join("sessions").join(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ac-journal-test-{}-{}",
            std::process::id(),
            tag
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn fill(dir: &Path, texts: &[&str]) -> Journal {
        let j = Journal::open(dir).unwrap();
        for t in texts {
            j.append(EventKind::AgentText { text: t.to_string() }).unwrap();
        }
        j
    }

    #[test]
    fn page_after() {
        let dir = tmpdir("after");
        let j = fill(&dir, &["a", "b", "c", "d", "e", "f"]);
        let page = j.read_page_after(&dir, 2, 3).unwrap();
        let seqs: Vec<u64> = page.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![3, 4, 5]);
        // après la fin → vide
        assert!(j.read_page_after(&dir, 6, 3).unwrap().is_empty());
        // après 0 = tout depuis le début, borné
        assert_eq!(j.read_page_after(&dir, 0, 2).unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn page_after_index_vide() {
        // Journal écrit, fermé, réouvert avec index reconstruit — même
        // comportement que le chemin indexé.
        let dir = tmpdir("reopen");
        let j = fill(&dir, &["x", "y", "z"]);
        drop(j);
        let j = Journal::open(&dir).unwrap();
        let page = j.read_page_after(&dir, 1, 10).unwrap();
        assert_eq!(page.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![2, 3]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn page_after_trous() {
        // Journal compacté = trou de seqs en tête : la pagination avant
        // doit sauter le trou et ne jamais rendre un seq <= borne.
        let dir = tmpdir("holes");
        let j = fill(&dir, &["a", "b", "c", "d", "e", "f", "g", "h"]);
        // max_bytes minuscule + keep_bytes ~2 events (~90 o/ligne) → la
        // tête du journal saute, les seqs 1..n-2 deviennent un trou.
        assert!(j.compact_if_needed(&dir, 1, 250).unwrap());
        j.append(EventKind::AgentText { text: "i".into() }).unwrap();
        j.append(EventKind::AgentText { text: "j".into() }).unwrap();
        // La borne peut tomber dans le trou : seuls des seqs > borne
        // doivent ressortir, dans l'ordre.
        let kept = j.read_page(&dir, None, 100).unwrap();
        let first_kept = kept[0].seq;
        assert!(first_kept > 1); // le trou existe bien
        let page = j.read_page_after(&dir, first_kept - 1, 100).unwrap();
        assert_eq!(
            page.iter().map(|e| e.seq).collect::<Vec<_>>(),
            kept.iter().map(|e| e.seq).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
