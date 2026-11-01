package dev.agentcontrol.stt

import app.tauri.plugin.JSObject
import java.io.File
import java.io.FileOutputStream
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.concurrent.thread
import org.json.JSONArray

// ── Download manager des modèles STT (backlog #133, doc §6.1/§7.3) ──────────
//
// Rien n'est embarqué dans l'APK (~685 Mo) : les modèles sont récupérés à la
// demande depuis les settings par GET HTTPS directs sur HuggingFace
// `resolve/main/<fichier>` — pas d'archive à décompresser. Reprise sur
// interruption (Range sur le `.part`), retry, vérif taille + sha256 avant de
// promouvoir le fichier. Source alternative documentée : la release GitHub
// k2-fsa/sherpa-onnx tag `asr-models` (tar.bz2 — exigerait bzip2, non retenu).

/** Un fichier du modèle : nom, taille exacte, sha256 du contenu. */
data class SttModelFile(val name: String, val size: Long, val sha256: String)

/** Descripteur data-driven d'un modèle téléchargeable — ajouter
 *  parakeet-tdt-0.6b-v3-int8 (2e modèle du doc §7) = une entrée de plus. */
data class SttModel(
  val id: String,
  val label: String,
  val baseUrl: String,
  /** Sous-dossier sous filesDir/models/stt ; "" = fichiers à plat — le
   *  layout que le core (#132) attend pour charger le recognizer. */
  val subdir: String,
  val files: List<SttModelFile>,
) {
  val totalBytes: Long get() = files.sumOf { it.size }
}

val STT_MODELS = listOf(
  SttModel(
    id = "nemotron-3.5-asr-streaming-0.6b-int8-560ms",
    label = "Nemotron 3.5 ASR streaming 0.6B int8 (560 ms)",
    baseUrl = "https://huggingface.co/csukuangfj2/sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-560ms-int8-2026-06-11/resolve/main",
    subdir = "",
    files = listOf(
      SttModelFile("encoder.int8.onnx", 657_601_403,
        "012e9321373af99021415e0b0eb3ec827b4be3153be6f30d9b448fe65e896e68"),
      SttModelFile("decoder.int8.onnx", 14_978_075,
        "19f9c98fc6d0a2c33a65a43b36fdb2e914c26c0aa9764be3aebc502a1e982fb0"),
      SttModelFile("joiner.int8.onnx", 9_504_438,
        "4101c7c679a0bc30483794b27a059e34e79232aa2068d78d51231a22c8b0d7ce"),
      SttModelFile("tokens.txt", 131_440,
        "729cc103155bafa785f9cd45746cd41cabe97eab7182fc04d594129587958f8a"),
    ),
  ),
)

private const val MAX_ATTEMPTS = 4
private const val CONNECT_TIMEOUT_MS = 20_000
private const val READ_TIMEOUT_MS = 30_000
private const val PROGRESS_EMIT_NS = 300_000_000L

private class Cancelled : Exception()

/** Échec non reprénable : 4xx (hors 429), sha256 mismatch — retrier ne
 *  servirait à rien. */
private class Fatal(msg: String) : Exception(msg)

/** Job de téléchargement actif d'un modèle : drapeau d'annulation +
 *  thread (interrupt() réveille les sleeps de backoff). */
private class Job(val model: SttModel) {
  val cancelled = AtomicBoolean(false)
  var thread: Thread? = null
}

/**
 * Moteur de téléchargement : un job par modèle, fichiers séquentiels,
 * `.part` reprenables, erreur persistée dans `.dlerr` (l'état « error »
 * survit au redémarrage de l'app ; `partial`/`absent`/`present` se déduisent
 * du disque — les fichiers sont l'état).
 */
class ModelDownloader(
  private val filesDir: File,
  private val emit: (event: String, payload: JSObject) -> Unit,
) {
  private val jobs = ConcurrentHashMap<String, Job>()

  // ── Chemins ──────────────────────────────────────────────────────────

  private fun modelRoot() = File(filesDir, "models/stt")

  fun modelDir(m: SttModel): File =
    if (m.subdir.isEmpty()) modelRoot() else File(modelRoot(), m.subdir)

  private fun dstFile(m: SttModel, f: SttModelFile) = File(modelDir(m), f.name)
  private fun partFile(m: SttModel, f: SttModelFile) =
    File(modelDir(m), "${f.name}.part")
  private fun errFile(m: SttModel) = File(modelDir(m), ".dlerr")

  fun findModel(id: String): SttModel? = STT_MODELS.find { it.id == id }

  // ── État ─────────────────────────────────────────────────────────────

  /** Octets téléchargés du modèle : fichiers finis (taille conforme)
   *  + `.part` en cours. */
  private fun downloadedBytes(m: SttModel): Long =
    m.files.sumOf { f ->
      val dst = dstFile(m, f)
      if (dst.isFile && dst.length() == f.size) f.size
      else partFile(m, f).takeIf { it.isFile }?.length() ?: 0L
    }

  fun stateOf(m: SttModel): String = when {
    jobs.containsKey(m.id) -> "downloading"
    m.files.all { dstFile(m, it).let { d -> d.isFile && d.length() == it.size } } ->
      "present"
    errFile(m).isFile -> "error"
    m.files.any {
      dstFile(m, it).isFile || partFile(m, it).isFile
    } -> "partial"
    else -> "absent"
  }

  /** Réponse de `stt_dl_state` : descripteur + état disque + job actif,
   *  auto-descriptif (le front n'a rien à deviner). */
  fun modelsJson(): JSONArray {
    val arr = JSONArray()
    for (m in STT_MODELS) {
      val o = JSObject()
      o.put("id", m.id)
      o.put("label", m.label)
      o.put("size_bytes", m.totalBytes)
      o.put("dir", modelDir(m).absolutePath)
      o.put("state", stateOf(m))
      o.put("downloaded", downloadedBytes(m))
      if (errFile(m).isFile) {
        o.put("error", runCatching { errFile(m).readText() }.getOrDefault(""))
      }
      val fa = JSONArray()
      for (f in m.files) {
        val fo = JSObject()
        fo.put("name", f.name)
        fo.put("size", f.size)
        val dst = dstFile(m, f)
        fo.put("have", if (dst.isFile && dst.length() == f.size) f.size
                       else partFile(m, f).takeIf { it.isFile }?.length() ?: 0L)
        fa.put(fo)
      }
      o.put("files", fa)
      arr.put(o)
    }
    return arr
  }

  // ── Actions ──────────────────────────────────────────────────────────

  fun start(m: SttModel) {
    errFile(m).delete()
    val job = Job(m)
    if (jobs.putIfAbsent(m.id, job) != null) return
    emitState(m)
    job.thread = thread(name = "stt-dl-${m.id.take(16)}") { runJob(job) }
  }

  fun cancel(id: String) {
    jobs[id]?.let {
      it.cancelled.set(true)
      it.thread?.interrupt()
    }
  }

  /** Supprime les fichiers du modèle (finaux + .part + .dlerr) — jamais
   *  le dossier entier : d'autres modèles peuvent partager le parent. */
  fun delete(m: SttModel) {
    val job = jobs[m.id]
    if (job != null) {
      job.cancelled.set(true)
      job.thread?.interrupt()
      job.thread?.join(3_000)
      jobs.remove(m.id)
    }
    for (f in m.files) {
      dstFile(m, f).delete()
      partFile(m, f).delete()
    }
    errFile(m).delete()
    emitState(m)
  }

  // ── Boucle de job ────────────────────────────────────────────────────

  private fun checkCancelled(job: Job) {
    if (job.cancelled.get() || Thread.currentThread().isInterrupted) {
      throw Cancelled()
    }
  }

  private fun runJob(job: Job) {
    val m = job.model
    try {
      modelDir(m).mkdirs()
      for (f in m.files) downloadFile(job, f)
    } catch (_: Cancelled) {
      // Les .part restent — stateOf relira "partial"/"absent" au finally.
    } catch (e: Exception) {
      // delete() a pu supprimer les fichiers pendant que ce job mourait
      // — un .dlerr écrit ensuite « ressusciterait » l'état error.
      if (!job.cancelled.get()) {
        runCatching { errFile(m).writeText(e.message ?: e.javaClass.simpleName) }
      }
    } finally {
      // Job sorti du registre AVANT d'émettre : stateOf reflète le disque
      // (present/partial/error), pas "downloading".
      jobs.remove(m.id)
      emitProgress(m)
      emitState(m)
    }
  }

  private fun downloadFile(job: Job, f: SttModelFile) {
    val m = job.model
    val dst = dstFile(m, f)
    val part = partFile(m, f)
    // Déjà complet (taille conforme → sha256 vérifié à son téléchargement).
    if (dst.isFile && dst.length() == f.size) return

    var resumeAt = if (part.isFile) part.length() else 0L
    if (resumeAt > f.size) { part.delete(); resumeAt = 0 }

    var attempt = 0
    while (true) {
      checkCancelled(job)
      try {
        val conn = URL("${m.baseUrl}/${f.name}").openConnection()
          as HttpURLConnection
        conn.connectTimeout = CONNECT_TIMEOUT_MS
        conn.readTimeout = READ_TIMEOUT_MS
        conn.instanceFollowRedirects = true
        if (resumeAt > 0) conn.setRequestProperty("Range", "bytes=$resumeAt-")
        val code = conn.responseCode
        if (code != HttpURLConnection.HTTP_OK &&
            code != HttpURLConnection.HTTP_PARTIAL) {
          if (code == 429 || code >= 500) {
            throw IOException("${f.name}: http $code")
          }
          // 4xx hors 429 = faute côté client — retrier ne sert à rien.
          throw Fatal("${f.name}: http $code")
        }
        // 200 sur un resume = Range ignoré → on repart de zéro.
        val append = code == HttpURLConnection.HTTP_PARTIAL && resumeAt > 0
        var have = if (append) resumeAt else 0L
        var lastEmit = System.nanoTime()
        conn.inputStream.use { inp ->
          FileOutputStream(part, append).use { out ->
            val buf = ByteArray(256 * 1024)
            while (true) {
              val n = inp.read(buf)
              if (n < 0) break
              out.write(buf, 0, n)
              have += n
              checkCancelled(job)
              val now = System.nanoTime()
              if (now - lastEmit > PROGRESS_EMIT_NS) {
                lastEmit = now
                emitProgress(m)
              }
            }
          }
        }
        if (have != f.size) {
          throw IOException("${f.name}: tronqué $have/${f.size}")
        }
        emitProgress(m)
        if (sha256Of(part) != f.sha256) {
          part.delete()
          throw Fatal("${f.name}: sha256 mismatch")
        }
        dst.delete()
        if (!part.renameTo(dst)) { part.copyTo(dst, true); part.delete() }
        return
      } catch (e: Fatal) {
        throw IOException(e.message)
      } catch (e: IOException) {
        // IOException = réseau/fichier : le `.part` garde les octets
        // déjà écrits → reprise au prochain essai (backoff borné).
        attempt++
        if (attempt >= MAX_ATTEMPTS) throw e
        resumeAt = part.takeIf { it.isFile }?.length() ?: 0L
        try {
          Thread.sleep(minOf(1_000L * (1 shl (attempt - 1)), 15_000L))
        } catch (_: InterruptedException) {
          throw Cancelled()
        }
      }
    }
  }

  // ── Events & utilitaires ─────────────────────────────────────────────

  private fun emitProgress(m: SttModel) {
    val o = JSObject()
    o.put("model", m.id)
    o.put("bytes", downloadedBytes(m))
    o.put("total", m.totalBytes)
    emit("stt_dl_progress", o)
  }

  private fun emitState(m: SttModel) {
    val o = JSObject()
    o.put("model", m.id)
    o.put("state", stateOf(m))
    emit("stt_dl_state", o)
  }

  private fun sha256Of(f: File): String {
    val md = MessageDigest.getInstance("SHA-256")
    f.inputStream().use { inp ->
      val buf = ByteArray(256 * 1024)
      while (true) {
        val n = inp.read(buf)
        if (n < 0) break
        md.update(buf, 0, n)
      }
    }
    return md.digest().joinToString("") { "%02x".format(it) }
  }
}
