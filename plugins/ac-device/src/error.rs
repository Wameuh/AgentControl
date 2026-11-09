use serde::{Serialize, ser::Serializer};

/// Alias pour `std::result::Result` avec l'`Error` du plugin.
pub type Result<T> = std::result::Result<T, Error>;

/// Erreurs du plugin ac-device — sérialisées en string à la
/// frontière IPC.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Échec d'appel du plugin mobile (Kotlin sur Android).
    #[cfg(mobile)]
    #[error(transparent)]
    PluginInvoke(#[from] tauri::plugin::mobile::PluginInvokeError),
    /// Réponse Kotlin inattendue.
    #[error("réponse ac-device invalide : {0}")]
    BadPayload(#[from] serde_json::Error),
    /// Commande réservée à Android appelée sur une autre plateforme.
    #[error("ac-device : commande disponible uniquement sur Android")]
    AndroidOnly,
}

impl Serialize for Error {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.to_string().as_ref())
    }
}
