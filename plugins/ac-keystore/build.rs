// Aucune commande webview : les @Command Kotlin (seal/open) ne sont
// appelées que depuis le Rust via `run_mobile_plugin` — jamais
// d'invoke JS → pas de permissions à générer.
const COMMANDS: &[&str] = &[];

fn main() {
    // `android_path` fait émettre `cargo:android_library_path` à la
    // cross-compile Android — agrégé par tauri_build dans
    // tauri.settings.gradle → inclusion du module gradle ci-dessous.
    tauri_plugin::Builder::new(COMMANDS)
        .android_path("android")
        .try_build()
        .expect("failed to build tauri-plugin-ac-keystore");
}
