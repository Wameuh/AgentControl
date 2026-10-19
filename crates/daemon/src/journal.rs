//! Journal append-only par session : events.jsonl + blobs/.
//! Index en mémoire seq -> offset octet (~16 o/evt) : les lectures sont en
//! seek direct, pas de scan complet. Compaction possible en gardant la queue.

use anyhow::{Context, Result};
use proto::{Event, EventKind};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

struct Inner {
    file: File,
    /// (seq, offset de début de ligne) — trié par seq croissant.
    /// Supporte les trous (après compaction) via recherche binaire.
    offsets: Vec<(u64, u64)>,
    /// Position de fin de fichier (prochain offset d'écriture).
    end: u64,
    last_seq: u64,
}

pub struct Journal {
    inner: Mutex<Inner>,
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
            }),
        })
    }

    pub fn last_seq(&self) -> u64 {
        self.inner.lock().unwrap().last_seq
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
            // nb d'events avec seq < before
            let n = g.offsets.partition_point(|(s, _)| *s < before);
            let start = n.saturating_sub(limit);
            let off = if start < n { g.offsets[start].1 } else { return Ok(vec![]) };
            (off, n - start)
        };
        Self::read_from(dir, start_off, before, n)
    }

    /// Tous les événements avec seq > after_seq (rattrapage subscribe).
    pub fn read_after(&self, dir: &Path, after_seq: u64) -> Result<Vec<Event>> {
        let start_off = {
            let g = self.inner.lock().unwrap();
            let i = g.offsets.partition_point(|(s, _)| *s <= after_seq);
            if i >= g.offsets.len() {
                return Ok(vec![]);
            }
            g.offsets[i].1
        };
        Self::read_from(dir, start_off, u64::MAX, usize::MAX)
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
        eprintln!("journal compacté : -{removed} events, {len} -> {pos} octets");
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
