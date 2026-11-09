fn main() {
    // Identité rc compilée : sur un tag vX.Y.Z-rc.K le workflow pose
    // AGENTCONTROL_RC=K → le binaire s'identifie « X.Y.Z-rc.K » pour
    // l'updater SANS toucher la version bundle (MSI/CFBundleVersion
    // exigent du numérique — l'injection fichier est impossible).
    println!("cargo:rerun-if-env-changed=AGENTCONTROL_RC");
    if let Ok(k) = std::env::var("AGENTCONTROL_RC") {
        if !k.is_empty() && k.bytes().all(|b| b.is_ascii_digit()) {
            println!("cargo:rustc-env=AGENTCONTROL_RC={k}");
        }
    }
    // Empreinte SHA-256 du certificat signataire de la release Android
    // (upload keystore CI — calculée par build.yml depuis le keystore
    // décodé). Injectée dans le binaire → `app_signing` la transmet au
    // plugin ac-device qui la compare au signataire de l'APK installé :
    // verdict update_in_place. Re-validée ici (hexadecimal) — jamais
    // d'env ambiante non contrôlée dans le binaire.
    println!("cargo:rerun-if-env-changed=AGENTCONTROL_ANDROID_CERT_SHA256");
    if let Ok(v) = std::env::var("AGENTCONTROL_ANDROID_CERT_SHA256") {
        let hex = v.trim().replace(':', "");
        if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            println!("cargo:rustc-env=AGENTCONTROL_ANDROID_CERT_SHA256={hex}");
        }
    }
    tauri_build::build()
}
