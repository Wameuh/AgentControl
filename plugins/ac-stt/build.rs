const COMMANDS: &[&str] = &[
    "stt_ping",
    "stt_dl_state",
    "stt_dl_start",
    "stt_dl_cancel",
    "stt_dl_delete",
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
