const COMMANDS: &[&str] = &["stt_ping"];

fn main() {
    // Squelette : une seule commande bidon. `android_path` fait émettre
    // `cargo:android_library_path` à la cross-compile Android — le
    // build.rs de l'app (tauri_build) l'agrège dans tauri.settings.gradle
    // → inclusion du module gradle ci-dessous.
    tauri_plugin::Builder::new(COMMANDS)
        .android_path("android")
        .try_build()
        .expect("failed to build tauri-plugin-ac-stt");
}
