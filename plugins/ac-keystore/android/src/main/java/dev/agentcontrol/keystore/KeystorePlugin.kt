package dev.agentcontrol.keystore

import android.app.Activity
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import app.tauri.annotation.Command
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

// Scellage des secrets au repos (clé ed25519 de l'app, clés appairées,
// connections.json) sous une clé AES/GCM non exportable vivant dans
// AndroidKeyStore. Format retourné : iv(12) || ciphertext+tag(GCM 128).
@TauriPlugin
class KeystorePlugin(activity: Activity): Plugin(activity) {
  private val alias = "agentcontrol_secrets_v1"
  private val ivLen = 12
  private val tagBits = 128

  private fun key(): SecretKey {
    val ks = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
    (ks.getEntry(alias, null) as? KeyStore.SecretKeyEntry)?.let {
      return it.secretKey
    }
    val gen = KeyGenerator.getInstance(
      KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
    gen.init(
      KeyGenParameterSpec.Builder(
        alias,
        KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
        .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
        .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
        .setKeySize(256)
        .build())
    return gen.generateKey()
  }

  private fun cipherFor(mode: Int, iv: ByteArray? = null): Cipher {
    val c = Cipher.getInstance("AES/GCM/NoPadding")
    if (iv == null) c.init(mode, key())
    else c.init(mode, key(), GCMParameterSpec(tagBits, iv))
    return c
  }

  @Command
  fun seal(invoke: Invoke) {
    try {
      val data = Base64.decode(
        invoke.getArgs().optString("data", ""), Base64.NO_WRAP)
      val c = cipherFor(Cipher.ENCRYPT_MODE)
      val ct = c.doFinal(data)
      val ret = JSObject()
      ret.put("data", Base64.encodeToString(c.iv + ct, Base64.NO_WRAP))
      invoke.resolve(ret)
    } catch (e: Exception) {
      invoke.reject("seal: ${e.message}")
    }
  }

  @Command
  fun open(invoke: Invoke) {
    try {
      val blob = Base64.decode(
        invoke.getArgs().optString("data", ""), Base64.NO_WRAP)
      if (blob.size <= ivLen) {
        invoke.reject("open: blob trop court")
        return
      }
      val c = cipherFor(Cipher.DECRYPT_MODE, blob.copyOfRange(0, ivLen))
      val ret = JSObject()
      ret.put("data",
        Base64.encodeToString(c.doFinal(blob, ivLen, blob.size - ivLen),
          Base64.NO_WRAP))
      invoke.resolve(ret)
    } catch (e: Exception) {
      invoke.reject("open: ${e.message}")
    }
  }
}
