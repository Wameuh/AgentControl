//! Logging : stderr (capturé par supervisord → `docker logs`) + fichier
//! journalier persistant `<root>/logs/daemon.log.YYYY-MM-DD` (14 conservés).
//!
//! Le fichier survit aux restarts du démon et aux recréations du conteneur
//! (root bind-mounté) — c'est lui qui permet de comprendre post-mortem
//! pourquoi le démon s'est arrêté (signal, panic, erreur fatale).

use std::path::Path;
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

/// Logge les panics (fichier + stderr via tracing) puis délègue au hook par
/// défaut qui imprime sur stderr. Sans ça, un panic dans une tâche tokio
/// tue la tâche silencieusement, et un panic dans main ne laisse qu'une
/// trace stderr volatile.
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let bt = std::backtrace::Backtrace::capture();
        tracing::error!("panic: {info}\n{bt}");
        prev(info);
    }));
}
