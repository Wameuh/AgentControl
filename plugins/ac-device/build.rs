// Commande webview : `set_system_bars` (barres d'état/navigation lisibles
// dans les deux thèmes, Y4). `signingInfo` reste Rust-only via
// `run_mobile_plugin` — jamais d'invoke JS.
const COMMANDS: &[&str] = &["set_system_bars"];

fn main() {
    // `android_path` fait émettre `cargo:android_library_path` à la
    // cross-compile Android — agrégé par tauri_build dans
    // tauri.settings.gradle → inclusion du module gradle ci-dessous.
    tauri_plugin::Builder::new(COMMANDS)
        .android_path("android")
        .try_build()
        .expect("failed to build tauri-plugin-ac-device");
}
