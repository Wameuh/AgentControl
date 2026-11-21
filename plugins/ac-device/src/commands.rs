use tauri::State;

/// Apparence des barres système Android (Y4) : `light: true` = icônes
/// sombres (thème clair de l'UI), `false` = icônes claires. Appelée par
/// `applyTheme` côté webview à chaque bascule de thème — forward au
/// `WindowCompat.getInsetsController` Kotlin via `run_mobile_plugin`.
#[tauri::command]
pub(crate) async fn set_system_bars(
    device: State<'_, crate::Device<tauri::Wry>>,
    light: bool,
) -> crate::Result<()> {
    device.set_system_bars(light)
}
