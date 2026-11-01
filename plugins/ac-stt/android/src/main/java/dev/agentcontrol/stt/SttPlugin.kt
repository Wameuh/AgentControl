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
class SttPlugin(activity: Activity): Plugin(activity) {

  override fun load(webView: WebView) {
    // Spike squelette : rien à initialiser — la capture arrive après.
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
}
