//! Logging : stderr (capturé par supervisord → `docker logs`) + fichier
//! journalier persistant `<root>/logs/daemon.log.YYYY-MM-DD` (14 conservés).
//!
//! Le fichier survit aux restarts du démon et aux recréations du conteneur
//! (root bind-mounté) — c'est lui qui permet de comprendre post-mortem
//! pourquoi le démon s'est arrêté (signal, panic, erreur fatale).

use std::path::{Path, PathBuf};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter};

/// Initialise tracing. Le guard retourné flush le writer fichier non-bloquant
/// à son drop — le garder vivant jusqu'à la fin de main, sinon les derniers
/// logs d'un arrêt brutal peuvent être perdus.
pub fn init(root: &Path) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let log_dir = root.join("logs");
    let _ = std::fs::create_dir_all(&log_dir);
    // Option<Layer> : repli stderr-only si le fichier ne peut pas être créé.
    let (file_layer, guard) = match tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("daemon.log")
        .max_log_files(14)
        .build(&log_dir)
    {
        Ok(appender) => {
            let (writer, guard) = tracing_appender::non_blocking(appender);
            (
                Some(fmt::layer().with_ansi(false).with_writer(writer)),
                Some(guard),
            )
        }
        Err(e) => {
            eprintln!("agentcontrol-daemon: log file indisponible ({e}) — stderr only");
            (None, None)
        }
    };

    let stderr_layer = fmt::layer()
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(std::io::stderr);

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
        .init();

    install_panic_hook();
    guard
}

/// Fichier journal courant : le `daemon.log.<date>` au nom le plus élevé
/// (les dates ISO trient chronologiquement ; `daemon.log` nu toléré par
/// robustesse). None = répertoire absent ou vide.
pub fn current_log_file(root: &Path) -> Option<(String, PathBuf)> {
    let dir = root.join("logs");
    let mut best: Option<(String, PathBuf)> = None;
    for e in std::fs::read_dir(&dir).ok()? {
        let Ok(e) = e else { continue };
        let name = e.file_name().to_string_lossy().to_string();
        if !(name == "daemon.log" || name.starts_with("daemon.log.")) {
            continue;
        }
        if !e.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        if best
            .as_ref()
            .map(|(n, _)| name.as_str() > n.as_str())
            .unwrap_or(true)
        {
            best = Some((name, e.path()));
        }
    }
    best
}

/// Tranche de lecture pour le suivi live (RPC `daemon_log_tail`) :
/// `incremental` = le client connaît déjà `offset` dans ce fichier →
/// lit [offset, offset+limit) ; sinon (premier appel, rotation, offset
/// invalide) → dernière page de `limit` octets. Ne renvoie que des
/// lignes complètes (coupe au dernier '\n' ; une ligne partielle en
/// cours d'écriture est retenue pour le poll suivant). Renvoie
/// (données UTF-8, nouvel offset absolu dans le fichier).
pub fn tail_slice(path: &Path, incremental: bool, offset: u64, limit: u64) -> (String, u64) {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return (String::new(), offset);
    };
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    // Incrémental réel seulement si l'offset tient dans le fichier —
    // offset > taille (rotation/troncature) retombe sur la dernière page.
    let incremental = incremental && offset <= size;
    let (start, end) = if incremental {
        (offset, size.min(offset.saturating_add(limit)))
    } else {
        // Dernière page : en mode reset on purge aussi la première
        // ligne partielle (la fenêtre peut commencer au milieu d'une
        // ligne) — repérée par drop_head dans cut_lines.
        (size.saturating_sub(limit), size)
    };
    if start >= end {
        return (
            String::new(),
            if incremental { offset } else { size },
        );
    }
    let mut buf = vec![0u8; (end - start) as usize];
    if f.seek(SeekFrom::Start(start)).is_err() || f.read_exact(&mut buf).is_err() {
        return (String::new(), offset);
    }
    // drop_head seulement si la fenêtre démarre au milieu du fichier —
    // start == 0 est une vraie frontière de ligne.
    cut_lines(&buf, start, !incremental && start > 0)
}

/// Découpe `buf` (octets lus depuis `base` dans le fichier) en lignes
/// complètes : `drop_head` purge la première ligne partielle (début de
/// fenêtre de tail au milieu d'une ligne). Renvoie (texte, offset absolu
/// juste après la dernière ligne émise). Aucune ligne complète émise →
/// offset de fin de fenêtre en reset (on ne rejoue pas la même page
/// indéfiniment), offset inchangé en incrémental (attente de la fin de
/// ligne).
fn cut_lines(buf: &[u8], base: u64, drop_head: bool) -> (String, u64) {
    let mut lo = 0usize;
    if drop_head {
        match buf.iter().position(|&b| b == b'\n') {
            Some(i) => lo = i + 1,
            // Fenêtre sans aucun '\n' : rien d'exploitable — en reset on
            // avance quand même à end (le cas est pathologique : une
            // ligne > limit), en incrémental on attend sa complétion.
            None => return (String::new(), base + buf.len() as u64),
        }
    }
    match buf.iter().rposition(|&b| b == b'\n') {
        Some(i) if i >= lo => (
            String::from_utf8_lossy(&buf[lo..=i]).into_owned(),
            base + i as u64 + 1,
        ),
        _ => (String::new(), base),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn current_log_picks_latest_dated() {
        let dir = std::env::temp_dir().join(format!("ac-logtest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        write(&dir.join("logs"), "daemon.log.2026-11-08", "old\n");
        write(&dir.join("logs"), "daemon.log.2026-11-09", "new\n");
        write(&dir.join("logs"), "client.log", "ignore\n");
        let (name, _) = current_log_file(&dir).unwrap();
        assert_eq!(name, "daemon.log.2026-11-09");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_slice_incremental_and_rotation() {
        let dir = std::env::temp_dir().join(format!("ac-slicetest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 3 lignes complètes + une partielle en cours d'écriture.
        let p = write(&dir, "f.log", "l1\nl2\nl3\npartial");
        // Premier appel (non incrémental) : dernière page, tête et queue
        // partielles purgées → lignes complètes seulement.
        let (d, off) = tail_slice(&p, false, 0, 1024);
        assert_eq!(d, "l1\nl2\nl3\n");
        assert_eq!(off, 9); // après "l3\n" — "partial" retenu
        // Incrémental : rien de nouveau tant que la ligne n'est pas finie.
        let (d, off2) = tail_slice(&p, true, off, 1024);
        assert_eq!((d.as_str(), off2), ("", 9));
        // La ligne se complète → émise au poll suivant.
        std::fs::write(&p, "l1\nl2\nl3\npartial\n").unwrap();
        let (d, off3) = tail_slice(&p, true, off2, 1024);
        assert_eq!(d, "partial\n");
        assert_eq!(off3, 17);
        // Offset invalide (> taille) → retombe sur la dernière page.
        let (d, _) = tail_slice(&p, true, 999, 1024);
        assert!(d.contains("l3"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_slice_window_drops_partial_head() {
        let dir = std::env::temp_dir().join(format!("ac-win-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Fenêtre de 8 octets sur un fichier plus grand : [12..20) =
        // "cc\ndddd\n" — la ligne « cc » (queue de "cccc") est purgée.
        let p = write(&dir, "f.log", "aaaa\nbbbb\ncccc\ndddd\n");
        let (d, off) = tail_slice(&p, false, 0, 8);
        assert_eq!(d, "dddd\n");
        assert_eq!(off, 20);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Logge les panics (fichier + stderr via tracing) puis délègue au hook par
/// défaut qui imprime sur stderr. Sans ça, un panic dans une tâche tokio
/// tue la tâche silencieusement, et un panic dans main ne laisse qu'une
/// trace stderr volatile.
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // force_capture : capture() honore RUST_BACKTRACE (absente sous
        // supervisord) → backtrace vide justement au moment du crash.
        let bt = std::backtrace::Backtrace::force_capture();
        tracing::error!("panic: {info}\n{bt}");
        prev(info);
    }));
}
