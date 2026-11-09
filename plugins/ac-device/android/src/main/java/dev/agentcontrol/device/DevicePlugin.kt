package dev.agentcontrol.device

import android.app.Activity
import android.content.pm.ApplicationInfo
import android.content.pm.PackageManager
import android.os.Build
import app.tauri.annotation.Command
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.security.MessageDigest

// Verdict « update in-place possible ? » pour l'APK installé : Android
// n'accepte un update que si la signature est identique — un appareil
// encore sur un build debug-signé (rc pré-keystore CI ou build --debug)
// ne peut PAS upgrader : la seule voie est uninstall → perte des accès
// (Keystore non exportables) → re-pairing QR. Le client interroge ce
// verdict AVANT de proposer le téléchargement de l'APK release.
//
// `expected` (arg JSON, injecté au build via AGENTCONTROL_ANDROID_CERT_SHA256
// → cargo:rustc-env → app_signing) = empreinte SHA-256 du certificat
// signataire de la release. Vide (build hors CI / dev local) → le
// verdict se limite à FLAG_DEBUGGABLE.
@TauriPlugin
class DevicePlugin(activity: Activity): Plugin(activity) {

  // SHA-256 du certificat signataire de CETTE app, hex continu
  // minuscules — même format que l'EXPECTED calculé par build.yml
  // (keytool/apksigner, sans deux-points) pour que la comparaison
  // avec `expected` soit directe. API 28+ lit
  // signingInfo.apkContentsSigners ; en dessous GET_SIGNATURES
  // (déprécié mais seul disponible — minSdk 24). L'overload int de
  // getPackageInfo est déprécié depuis API 33 (PackageInfoFlags) mais
  // reste fonctionnel — plus simple que de dupliquer par niveau.
  @Suppress("DEPRECATION")
  private fun signerSha256(): String? {
    val pm = activity.packageManager
    val pi = if (Build.VERSION.SDK_INT >= 28) {
      pm.getPackageInfo(
        activity.packageName, PackageManager.GET_SIGNING_CERTIFICATES)
    } else {
      pm.getPackageInfo(activity.packageName, PackageManager.GET_SIGNATURES)
    }
    val cert = if (Build.VERSION.SDK_INT >= 28) {
      // signingInfo couvre les schémas v2/v3 ; repli signatures (v1)
      // si le package n'expose pas de signingInfo.
      pi.signingInfo?.apkContentsSigners?.firstOrNull()
        ?: pi.signatures?.firstOrNull()
    } else {
      pi.signatures?.firstOrNull()
    } ?: return null
    val md = MessageDigest.getInstance("SHA-256").digest(cert.encoded)
    return md.joinToString("") { "%02x".format(it) }
  }

  private fun verdict(debuggable: Boolean, signer: String?, expected: String): String {
    // Tout build --debug est de toute façon incompatible avec les APK
    // release (signature debug ≠ upload keystore) — verdict direct.
    if (debuggable) return "impossible"
    if (expected.isBlank() || signer == null) return "unknown"
    return if (signer.equals(expected, ignoreCase = true)) "ok" else "impossible"
  }

  @Command
  fun signingInfo(invoke: Invoke) {
    try {
      val ret = JSObject()
      ret.put("platform", "android")
      val debuggable =
        (activity.applicationInfo.flags and ApplicationInfo.FLAG_DEBUGGABLE) != 0
      val signer = signerSha256() ?: ""
      val expected = invoke.getArgs().optString("expected", "")
      ret.put("debuggable", debuggable)
      ret.put("signer_sha256", signer)
      ret.put("release_cert_sha256", expected)
      ret.put("update_in_place", verdict(debuggable, signer.ifEmpty { null }, expected))
      invoke.resolve(ret)
    } catch (e: Exception) {
      invoke.reject("signingInfo: ${e.message}")
    }
  }
}
