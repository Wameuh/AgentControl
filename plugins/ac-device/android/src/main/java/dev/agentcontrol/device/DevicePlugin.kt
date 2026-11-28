package dev.agentcontrol.device

import android.app.Activity
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.ActivityNotFoundException
import android.content.Context
import android.content.Intent
import android.content.pm.ApplicationInfo
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.provider.Settings
import androidx.appcompat.app.AppCompatActivity
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
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

  companion object {
    private const val UPDATE_PREFS = "agentcontrol_update"
    private const val NOTIFICATION_ID = 3751
    private const val NOTIFICATION_CHANNEL = "agentcontrol-updates"
    private const val PENDING_INTENT_REQUEST = 3751
    private const val PENDING_TTL_MS = 24L * 60 * 60 * 1000
    private const val LAUNCH_CHECK_MS = 4000L
  }

  // État du cycle d'install : l'APK reste dans le cache privé entre les
  // tentatives (le chemin est persisté dans SharedPreferences pour
  // survivre à un kill du process pendant que l'installeur est ouvert).
  @Volatile private var resumed = false
  @Volatile private var installLaunched = false
  @Volatile private var installSawPause = false
  private val mainHandler = Handler(Looper.getMainLooper())
  private val launchCheck = Runnable {
    if (installLaunched && resumed && !installSawPause) {
      clearPending()
      triggerState("launch_blocked")
    }
  }

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

  // Update APK : `path` = APK déjà téléchargé dans
  // <cache>/agentcontrol-update/ et vérifié côté Rust (SHA256SUMS signé
  // minisign + sha256, I1 — update_client). L'installeur n'est lancé que
  // si l'activité est resumed : un startActivity depuis l'arrière-plan
  // est silencieusement bloqué par Android 10+. Sinon l'APK est armé en
  // SharedPreferences + notification PendingIntent ; onResume reprend
  // (permission accordée → installeur, sinon réglages une fois). Le
  // retour de l'installeur est détecté par comparaison du versionCode —
  // "unchanged" = install annulée/installeur jamais affiché.
  @Command
  fun installApk(invoke: Invoke) {
    try {
      val args = invoke.getArgs()
      val path = args.optString("path", "")
      val title = args.optString("notify_title", "")
      val text = args.optString("notify_text", "")
      val file = validApkFile(path)
      if (file == null) {
        invoke.reject("installApk: invalid apk path")
        return
      }
      armPending(file)
      if (!canInstallPackages()) {
        if (isForeground()) {
          updatePrefs().edit().putBoolean("permission_prompted", true).apply()
          openUnknownSourcesSettings()
        } else {
          postUpdateNotification(title, text)
        }
        invoke.resolve(statusObject("permission_required"))
        return
      }
      if (!isForeground()) {
        postUpdateNotification(title, text)
        invoke.resolve(statusObject("pending"))
        return
      }
      startInstall(file) { err ->
        if (err == null) {
          markLaunched()
          invoke.resolve(statusObject("launched"))
        } else {
          clearPending()
          invoke.reject("installApk: $err")
        }
      }
    } catch (e: Exception) {
      invoke.reject("installApk: ${e.message}")
    }
  }

  override fun onResume(appCompatActivity: AppCompatActivity) {
    super.onResume(appCompatActivity)
    resumed = true
    try {
      handlePendingResume()
    } catch (e: Exception) {
      triggerState("error", "resume: ${e.message}")
    }
  }

  override fun onPause(appCompatActivity: AppCompatActivity) {
    super.onPause(appCompatActivity)
    resumed = false
    if (installLaunched) installSawPause = true
  }

  // Reprise au premier plan : install déjà lancée → le versionCode dit si
  // l'APK a été remplacé ; install armée mais pas lancée → permission ou
  // installeur selon l'état courant.
  private fun handlePendingResume() {
    val prefs = updatePrefs()
    val apkPath = prefs.getString("apk_path", "").orEmpty()
    if (apkPath.isEmpty()) return
    // Install déjà lancée : le versionCode tranche — pas besoin du
    // fichier (ni du TTL) pour savoir si l'APK a été remplacé.
    if (prefs.getBoolean("launched", false)) {
      val baseline = prefs.getLong("baseline_version_code", -1)
      val current = currentVersionCode()
      clearPending()
      if (baseline >= 0 && current != baseline) {
        validApkFile(apkPath)?.delete()
        triggerState("installed")
      } else {
        triggerState("unchanged")
      }
      return
    }
    val file = pendingFileOrNull()
    if (file == null) {
      clearPending()
      triggerState("missing")
      return
    }
    if (!canInstallPackages()) {
      if (!prefs.getBoolean("permission_prompted", false)) {
        prefs.edit().putBoolean("permission_prompted", true).apply()
        openUnknownSourcesSettings()
      }
      if (!prefs.getBoolean("resume_notified", false)) {
        prefs.edit().putBoolean("resume_notified", true).apply()
        triggerState("permission_required")
      }
      return
    }
    launchPendingInstall(file)
  }

  private fun updatePrefs() =
    activity.getSharedPreferences(UPDATE_PREFS, Context.MODE_PRIVATE)

  private fun isUpdateFile(f: File): Boolean {
    return try {
      val root = File(activity.cacheDir, "agentcontrol-update")
        .canonicalFile.toPath()
      f.canonicalFile.toPath().startsWith(root)
    } catch (e: Exception) {
      false
    }
  }

  private fun validApkFile(path: String): File? {
    if (path.isEmpty()) return null
    val f = File(path)
    if (!f.isFile || !f.name.endsWith(".apk", ignoreCase = true) ||
        !isUpdateFile(f)) {
      return null
    }
    return f
  }

  private fun pendingFileOrNull(): File? {
    val prefs = updatePrefs()
    val created = prefs.getLong("created_at", 0)
    if (created > 0 &&
        System.currentTimeMillis() - created > PENDING_TTL_MS) {
      clearPending()
      return null
    }
    return validApkFile(prefs.getString("apk_path", "").orEmpty())
  }

  private fun armPending(file: File) {
    cancelUpdateNotification()
    updatePrefs().edit()
      .putString("apk_path", file.absolutePath)
      .putLong("baseline_version_code", currentVersionCode())
      .putLong("created_at", System.currentTimeMillis())
      .putBoolean("launched", false)
      .putBoolean("permission_prompted", false)
      .putBoolean("resume_notified", false)
      .apply()
    installLaunched = false
    installSawPause = false
    mainHandler.removeCallbacks(launchCheck)
  }

  private fun markLaunched() {
    updatePrefs().edit()
      .putBoolean("launched", true)
      .apply()
    installLaunched = true
    installSawPause = false
    mainHandler.removeCallbacks(launchCheck)
    mainHandler.postDelayed(launchCheck, LAUNCH_CHECK_MS)
  }

  private fun clearPending() {
    updatePrefs().edit().clear().apply()
    installLaunched = false
    installSawPause = false
    mainHandler.removeCallbacks(launchCheck)
    cancelUpdateNotification()
  }

  // «Foreground» = activité resumed (onResume/onPause tenus par le
  // PluginManager) — pas hasWindowFocus(), à appeler hors thread UI.
  private fun isForeground() = resumed

  @Suppress("DEPRECATION")
  private fun currentVersionCode(): Long {
    val pi = activity.packageManager.getPackageInfo(activity.packageName, 0)
    return if (Build.VERSION.SDK_INT >= 28) {
      pi.longVersionCode
    } else {
      pi.versionCode.toLong()
    }
  }

  private fun canInstallPackages() =
    Build.VERSION.SDK_INT < 26 || activity.packageManager.canRequestPackageInstalls()

  private fun statusObject(status: String): JSObject {
    val ret = JSObject()
    ret.put("status", status)
    return ret
  }

  private fun triggerState(status: String, message: String? = null) {
    triggerStateWhenReady(status, message, 0)
  }

  // Un process relancé (install réussie → l'APK a remplacé l'app) émet
  // l'état au premier onResume, AVANT que la webview n'ait enregistré
  // son listener — on rejoue tant que personne n'écoute (~10 s max).
  private fun triggerStateWhenReady(
    status: String,
    message: String?,
    attempt: Int,
  ) {
    val ret = JSObject()
    ret.put("status", status)
    if (message != null) ret.put("message", message)
    if (hasListener("update_install_state") || attempt >= 20) {
      trigger("update_install_state", ret)
      return
    }
    mainHandler.postDelayed(
      { triggerStateWhenReady(status, message, attempt + 1) }, 500)
  }

  // startActivity exige le thread UI ; le contexte Activity n'a pas besoin
  // de FLAG_ACTIVITY_NEW_TASK (réservé aux contextes non-Activity et
  // source de tâches parasites chez certains OEM). Retourne null si
  // l'intent est parti, sinon la cause de l'échec.
  private fun launchInstallIntent(file: File): String? {
    return try {
      val uri = FileProvider.getUriForFile(
        activity, "${activity.packageName}.acdevice", file)
      val i = Intent(Intent.ACTION_VIEW).apply {
        setDataAndType(uri, "application/vnd.android.package-archive")
        addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
      }
      activity.startActivity(i)
      null
    } catch (e: ActivityNotFoundException) {
      "no package installer found — ${e.message}"
    } catch (e: Exception) {
      e.message ?: e.toString()
    }
  }

  private fun startInstall(file: File, onResult: (String?) -> Unit) {
    activity.runOnUiThread { onResult(launchInstallIntent(file)) }
  }

  private fun launchPendingInstall(file: File) {
    startInstall(file) { err ->
      if (err == null) {
        markLaunched()
        cancelUpdateNotification()
        triggerState("launched")
      } else {
        clearPending()
        triggerState("error", err)
      }
    }
  }

  private fun openUnknownSourcesSettings() {
    activity.runOnUiThread {
      try {
        val i = Intent(
          Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES,
          Uri.parse("package:${activity.packageName}"))
        activity.startActivity(i)
      } catch (e: Exception) {
        triggerState("error", "settings: ${e.message}")
      }
    }
  }

  private fun ensureNotificationChannel() {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
    val nm = activity.getSystemService(NotificationManager::class.java)
    nm.createNotificationChannel(
      NotificationChannel(
        NOTIFICATION_CHANNEL,
        activity.applicationInfo.loadLabel(activity.packageManager).toString(),
        NotificationManager.IMPORTANCE_HIGH))
  }

  // Filet pour le démarrage en arrière-plan : l'APK est déjà vérifié,
  // le PendingIntent rouvre l'app et onResume reprend le flux (permission
  // ou installeur). Sans permission notifications Android 13+, le flag
  // SharedPreferences suffit — l'utilisateur retrouve le flux au retour.
  private fun postUpdateNotification(title: String, text: String) {
    try {
      ensureNotificationChannel()
      val nm = NotificationManagerCompat.from(activity)
      if (!nm.areNotificationsEnabled()) return
      val launch = activity.packageManager
        .getLaunchIntentForPackage(activity.packageName)
        ?: Intent(activity, activity.javaClass)
      launch.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or
        Intent.FLAG_ACTIVITY_SINGLE_TOP)
      val pi = PendingIntent.getActivity(
        activity, PENDING_INTENT_REQUEST, launch,
        PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE)
      val appLabel = activity.applicationInfo
        .loadLabel(activity.packageManager).toString()
      val n = NotificationCompat.Builder(activity, NOTIFICATION_CHANNEL)
        .setSmallIcon(
          activity.applicationInfo.icon.takeIf { it != 0 }
            ?: android.R.drawable.ic_dialog_info)
        .setContentTitle(title.ifEmpty { appLabel })
        .setContentText(text.ifEmpty { "Update ready — tap to install" })
        .setContentIntent(pi)
        .setAutoCancel(true)
        .setPriority(NotificationCompat.PRIORITY_HIGH)
        .build()
      nm.notify(NOTIFICATION_ID, n)
    } catch (e: Exception) {
      // La notification est un filet : l'armement SharedPreferences reste
      // fonctionnel même si le canal/icône est indisponible.
    }
  }

  private fun cancelUpdateNotification() {
    NotificationManagerCompat.from(activity).cancel(NOTIFICATION_ID)
  }
}
