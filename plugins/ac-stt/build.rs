// `register_listener`/`remove_listener` sont les @Command héritées de la
// base Kotlin `Plugin` (events stt_* → addPluginListener JS) — elles ne
// passent pas par l'invoke_handler Rust (fallthrough mobile) mais les
// perms doivent exister pour la webview.
const COMMANDS: &[&str] = &[
    "stt_ping",
    "stt_dl_state",
    "stt_dl_start",
    "stt_dl_cancel",
    "stt_dl_delete",
    "stt_state",
    "stt_start",
    "stt_stop",
    "stt_set_language",
    "register_listener",
    "remove_listener",
];

fn main() {
    // COMMANDS alimente les permissions autogénérées (allow/deny-<cmd>).
    // `android_path` fait émettre `cargo:android_library_path` à la
    // cross-compile Android — le build.rs de l'app (tauri_build) l'agrège
    // dans tauri.settings.gradle → inclusion du module gradle ci-dessous.
    tauri_plugin::Builder::new(COMMANDS)
        .android_path("android")
        .try_build()
        .expect("failed to build tauri-plugin-ac-stt");
}
