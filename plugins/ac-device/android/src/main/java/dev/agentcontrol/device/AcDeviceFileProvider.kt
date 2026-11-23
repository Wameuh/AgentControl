package dev.agentcontrol.device

import androidx.core.content.FileProvider

/// Provider FileProvider dédié au plugin ac-device (update APK).
/// Sous-classe nommée (et non `androidx.core.content.FileProvider`
/// directement) : le template Gradle de l'app hôte déclare déjà un
/// provider du même nom — deux providers homonymes feraient échouer
/// le manifest merger.
class AcDeviceFileProvider : FileProvider()
