package dev.agentcontrol.stt

import android.Manifest
import android.app.Activity
import android.webkit.WebView
import app.tauri.annotation.Command
import app.tauri.annotation.Permission
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin

@TauriPlugin(
  permissions = [
    Permission(strings = [Manifest.permission.RECORD_AUDIO], alias = "microphone")
  ]
)
class SttPlugin(private val activity: Activity): Plugin(activity) {

  // Download des modèles à la demande (backlog #133) — events relayés à
  // la webview via le mécanisme listeners du plugin (addPluginListener
  // côté JS : stt_dl_progress / stt_dl_state).
  private lateinit var downloader: ModelDownloader

  override fun load(webView: WebView) {
    downloader = ModelDownloader(activity.filesDir) { ev, payload ->
      trigger(ev, payload)
    }
  }

  // Répond au forward Rust `run_mobile_plugin("sttPing")` de la commande
  // `stt_ping` — platform="android" prouve que l'appel aboutit ici.
  @Command
  fun sttPing(invoke: Invoke) {
    val ret = JSObject()
    ret.put("pong", true)
    ret.put("platform", "android")
    invoke.resolve(ret)
  }

  // ── Download manager modèles (settings « Reconnaissance vocale ») ────

  @Command
  fun sttDlState(invoke: Invoke) {
    val ret = JSObject()
    ret.put("platform", "android")
    ret.put("models", downloader.modelsJson())
    invoke.resolve(ret)
  }

  @Command
  fun sttDlStart(invoke: Invoke) {
    val m = modelArg(invoke) ?: return
    downloader.start(m)
    invoke.resolve()
  }

  @Command
  fun sttDlCancel(invoke: Invoke) {
    val id = invoke.getArgs().getString("model", "") ?: ""
    downloader.cancel(id)
    invoke.resolve()
  }

  @Command
  fun sttDlDelete(invoke: Invoke) {
    val m = modelArg(invoke) ?: return
    downloader.delete(m)
    invoke.resolve()
  }

  private fun modelArg(invoke: Invoke): SttModel? {
    val id = invoke.getArgs().getString("model", "") ?: ""
    val m = downloader.findModel(id)
    if (m == null) invoke.reject("modèle inconnu: $id")
    return m
  }
}
