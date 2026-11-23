package dev.agentcontrol.device

import android.app.Activity
import android.content.ActivityNotFoundException
import android.content.Intent
import android.content.pm.ApplicationInfo
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.provider.Settings
import androidx.core.content.FileProvider
import androidx.core.view.WindowCompat
import app.tauri.annotation.Command
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.io.File
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
class DevicePlugin(private val activity: Activity): Plugin(activity) {

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
    // Signature (pas X509Certificate) : toByteArray() = DER du
    // certificat — mêmes octets que keytool/apksigner → même empreinte.
    val md = MessageDigest.getInstance("SHA-256").digest(cert.toByteArray())
    return md.joinToString("") { "%02x".format(it) }
  }

  private fun verdict(debuggable: Boolean, signer: String?, expected: String): String {
    // Tout build --debug est de toute façon incompatible avec les APK
    // release (signature debug ≠ upload keystore) — verdict direct.
    if (debuggable) return "impossible"
    if (expected.isBlank() || signer == null) return "unknown"
    return if (signer.equals(expected, ignoreCase = true)) "ok" else "impossible"
  }

  // Barres système lisibles dans les deux thèmes (Y4) : `light` (arg
  // JSON) vient d'`applyTheme` côté webview — true = icônes sombres sur
  // barre d'état ET barre de navigation (thème clair), false = claires.
  // WindowInsetsControllerCompat exige le thread UI.
  @Command
  fun setSystemBars(invoke: Invoke) {
    try {
      val light = invoke.getArgs().optBoolean("light", false)
      activity.runOnUiThread {
        val c = WindowCompat.getInsetsController(
          activity.window, activity.window.decorView)
        c.isAppearanceLightStatusBars = light
        c.isAppearanceLightNavigationBars = light
      }
      invoke.resolve()
    } catch (e: Exception) {
      invoke.reject("setSystemBars: ${e.message}")
    }
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

  // Update APK : `path` = APK déjà téléchargé dans le cache de l'app
  // et vérifié côté Rust (SHA256SUMS signé minisign + sha256, I1 —
  // update_client). Android 8+ exige la permission « installer des
  // applications inconnues » : absente, on ouvre les réglages de
  // l'app et on rend "permission_required" — l'utilisateur accorde
  // puis relance la mise à jour (l'APK est retéléchargé/vérifié,
  // cache purgé à chaque tentative). Accordée → intent VIEW sur
  // l'URI content:// du FileProvider (racine cache-path
  // « agentcontrol-update/ » dans ac_device_paths.xml) ; l'installeur
  // système prend le relais et le process peut être tué par le
  // remplacement — d'où la résolution immédiate, sans attendre la
  // fin de l'install.
  @Command
  fun installApk(invoke: Invoke) {
    try {
      val path = invoke.getArgs().optString("path", "")
      val f = File(path)
      if (path.isEmpty() || !f.isFile ||
          !f.name.endsWith(".apk", ignoreCase = true)) {
        invoke.reject("installApk: invalid apk path")
        return
      }
      if (Build.VERSION.SDK_INT >= 26 &&
          !activity.packageManager.canRequestPackageInstalls()) {
        val i = Intent(
          Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES,
          Uri.parse("package:${activity.packageName}"))
        i.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        activity.startActivity(i)
        val ret = JSObject()
        ret.put("status", "permission_required")
        invoke.resolve(ret)
        return
      }
      val uri = FileProvider.getUriForFile(
        activity, "${activity.packageName}.acdevice", f)
      val i = Intent(Intent.ACTION_VIEW).apply {
        setDataAndType(uri, "application/vnd.android.package-archive")
        addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or
          Intent.FLAG_ACTIVITY_NEW_TASK)
      }
      activity.startActivity(i)
      val ret = JSObject()
      ret.put("status", "launched")
      invoke.resolve(ret)
    } catch (e: ActivityNotFoundException) {
      invoke.reject("installApk: no package installer found — ${e.message}")
    } catch (e: Exception) {
      invoke.reject("installApk: ${e.message}")
    }
  }
}
