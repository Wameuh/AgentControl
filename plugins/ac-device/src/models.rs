/// Verdict d'update in-place rapporté par le Kotlin :
/// - `Ok` : l'APK installé porte la signature release (certificat
///   upload keystore) → PackageManager acceptera l'APK de la release ;
/// - `Impossible` : build debuggable ou signataire ≠ certificat
///   release → seul un uninstall/update croisé est possible, avec
///   perte des données (Keystore non exportables) ;
/// - `Unknown` : certificat release inconnu (build hors CI) et
///   signature non lisible → pas de verdict, l'UI ne bloque pas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateInPlace {
    Ok,
    Impossible,
    Unknown,
}

/// Réponse de `signingInfo` — le frontend n'a besoin que du verdict,
/// les champs bruts servent au diagnostic (client.log / debug).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct SigningInfo {
    pub platform: String,
    /// `FLAG_DEBUGGABLE` de l'application — vrai sur tout build
    /// `--debug` (et sur les anciens builds debug-signés).
    pub debuggable: bool,
    /// Empreinte SHA-256 du certificat signataire de l'APK installé
    /// (`""` si illisible — API < 28 sans signatures, package absent…).
    pub signer_sha256: String,
    /// Empreinte SHA-256 attendue (upload keystore CI), injectée par
    /// `AGENTCONTROL_ANDROID_CERT_SHA256` au build — `""` hors CI.
    pub release_cert_sha256: String,
    pub update_in_place: UpdateInPlace,
}
