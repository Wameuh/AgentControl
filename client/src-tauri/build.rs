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
    tauri_build::build()
}
