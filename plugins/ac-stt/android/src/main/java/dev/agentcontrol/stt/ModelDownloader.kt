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
import org.apache.commons.compress.archivers.tar.TarArchiveInputStream
import org.apache.commons.compress.compressors.bzip2.BZip2CompressorInputStream
import org.json.JSONArray

// ── Download manager des modèles voix (STT #133 + TTS #138, doc §6.1) ───────
//
// Rien n'est embarqué dans l'APK : les modèles sont récupérés à la demande
// depuis les settings. Deux formes de descripteur data-driven :
//  - fichiers individuels (STT) : GET HTTPS directs sur HuggingFace
//    `resolve/main/<fichier>` — pas d'archive à décompresser ;
//  - archive tar.bz2 unique (TTS) : la release GitHub k2-fsa/sherpa-onnx tag
//    `tts-models` — un seul GET (~130-150 Mo) puis extraction (espeak-ng-data
//    compte ~350 petits fichiers, un GET chacun serait lent et fragile).
// Commun : reprise sur interruption (Range sur le `.part`), retry, vérif
// taille + sha256 avant de promouvoir, erreur persistée dans `.dlerr`.

/** Un fichier du modèle : nom, taille exacte, sha256 du contenu. Pour un
 *  modèle `archive`, l'unique entrée est le .tar.bz2 à extraire. */
data class SttModelFile(val name: String, val size: Long, val sha256: String)

/** Descripteur data-driven d'un modèle téléchargeable — ajouter un modèle =
 *  une entrée de liste de plus (parakeet 2e STT, autre voix TTS…). */
data class SttModel(
  val id: String,
  val label: String,
  /** "stt" | "tts" — dossier racine filesDir/models/<kind> et préfixe des
   *  events de progression (`<kind>_dl_progress` / `<kind>_dl_state`). */
  val kind: String,
  val baseUrl: String,
  /** Sous-dossier sous filesDir/models/<kind> ; "" = fichiers à plat à la
   *  racine du kind (layout STT du core recognizer). */
  val subdir: String,
  /** true → `files` décrit une archive .tar.bz2 extraite sous modelDir à la
   *  fin du téléchargement (premier composant de chemin ignoré, `.ok`
   *  marqueur de complétion). */
  val archive: Boolean = false,
  val files: List<SttModelFile>,
) {
  val totalBytes: Long get() = files.sumOf { it.size }
}

private const val GH_TTS_RELEASE =
  "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models"

val STT_MODELS = listOf(
  SttModel(
    id = "nemotron-3.5-asr-streaming-0.6b-int8-560ms",
    label = "Nemotron 3.5 ASR streaming 0.6B int8 (560 ms)",
    kind = "stt",
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

// Voix TTS on-device (backlog #138) : kokoro multi-lang v1_0 en priorité —
// voix neuronales naturelles, 53 speakers dont le français `ff_siwis`
// (sid 30). NB : la v1_1 n'embarque QUE zh+en (103 speakers zf/zm/af/bf) —
// c'est bien la v1_0 qui couvre fr/es/it/pt/ja/hi. Repli léger : piper
// fr_FR siwis medium (voix plus robotique mais ~21 Mo contre ~132).
val TTS_MODELS = listOf(
  SttModel(
    id = "kokoro-int8-multi-lang-v1_0",
    label = "Kokoro 82M multi-lang int8 — voix naturelles (fr/es/it/pt/ja/en…)",
    kind = "tts",
    baseUrl = GH_TTS_RELEASE,
    subdir = "kokoro-int8-multi-lang-v1_0",
    archive = true,
    files = listOf(
      SttModelFile("kokoro-int8-multi-lang-v1_0.tar.bz2", 132_353_410,
        "72c347230107ef8a31098ec4d468a605244beef5f1ed00db97efa13518800653"),
    ),
  ),
  SttModel(
    id = "vits-piper-fr_FR-siwis-medium-int8",
    label = "Piper fr_FR siwis medium int8 — léger, français uniquement",
    kind = "tts",
    baseUrl = GH_TTS_RELEASE,
    subdir = "vits-piper-fr_FR-siwis-medium-int8",
    archive = true,
    files = listOf(
      SttModelFile("vits-piper-fr_FR-siwis-medium-int8.tar.bz2", 20_914_888,
        "3909cff9b3cfd4820c66aa13bf554315c82e34899c161f0b446ece372bc4b5ec"),
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

  private fun modelRoot(m: SttModel) = File(filesDir, "models/${m.kind}")

  fun modelDir(m: SttModel): File =
    if (m.subdir.isEmpty()) modelRoot(m) else File(modelRoot(m), m.subdir)

  private fun dstFile(m: SttModel, f: SttModelFile) = File(modelDir(m), f.name)
  private fun partFile(m: SttModel, f: SttModelFile) =
    File(modelDir(m), "${f.name}.part")
  /** Erreur persistée par modèle — deux modèles déclarés dans le même
   *  `subdir` (ex. à plat) ne partagent pas leur fichier d'erreur. */
  private fun errFile(m: SttModel) = File(modelDir(m), ".${m.id}.dlerr")
  /** Marqueur posé une fois l'archive intégralement extraite — seul
   *  témoin fiable de « present » pour un modèle `archive` (l'archive
   *  elle-même est supprimée après extraction). */
  private fun okFile(m: SttModel) = File(modelDir(m), ".ok")
  /** Extraction en cours dans un dir à part, renommé en fin — une
   *  extraction interrompue ne laisse pas un demi-arbre dans modelDir. */
  private fun stagingDir(m: SttModel) = File(modelRoot(m), ".${m.id}.x")

  /** `kind` restreint la recherche à une famille ("stt"/"tts") ; null =
   *  les deux (diagnostic). */
  fun findModel(id: String, kind: String? = null): SttModel? =
    (STT_MODELS + TTS_MODELS).find { it.id == id && (kind == null || it.kind == kind) }

  // ── État ─────────────────────────────────────────────────────────────

  /** Octets téléchargés du modèle : fichiers finis (taille conforme)
   *  + `.part` en cours ; modèle installé = total affiché. */
  private fun downloadedBytes(m: SttModel): Long {
    if (m.archive && okFile(m).isFile) return m.totalBytes
    return m.files.sumOf { f ->
      val dst = dstFile(m, f)
      if (dst.isFile && dst.length() == f.size) f.size
      else partFile(m, f).takeIf { it.isFile }?.length() ?: 0L
    }
  }

  private fun filesPresent(m: SttModel): Boolean {
    if (m.archive) return okFile(m).isFile
    return m.files.all { dstFile(m, it).let { d -> d.isFile && d.length() == it.size } }
  }

  private fun hasLeftovers(m: SttModel): Boolean {
    if (m.archive) {
      return modelDir(m).isDirectory || stagingDir(m).isDirectory ||
        m.files.any { partFile(m, it).isFile || dstFile(m, it).isFile }
    }
    return m.files.any { dstFile(m, it).isFile || partFile(m, it).isFile }
  }

  fun stateOf(m: SttModel): String = when {
    jobs.containsKey(m.id) -> "downloading"
    filesPresent(m) -> "present"
    errFile(m).isFile -> "error"
    hasLeftovers(m) -> "partial"
    else -> "absent"
  }

  /** Réponse de `<kind>_dl_state` : descripteurs + état disque + jobs
   *  actifs, auto-descriptif (le front n'a rien à deviner). */
  fun modelsJson(kind: String): JSONArray {
    val arr = JSONArray()
    for (m in STT_MODELS + TTS_MODELS) {
      if (m.kind != kind) continue
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
    job.thread = thread(name = "${m.kind}-dl-${m.id.take(16)}") { runJob(job) }
  }

  fun cancel(id: String) {
    jobs[id]?.let {
      it.cancelled.set(true)
      it.thread?.interrupt()
    }
  }

  /** Supprime les fichiers du modèle : finaux + .part + .dlerr pour un
   *  modèle fichiers (le parent peut être partagé) ; dossier entier
   *  (extraction comprise) pour un modèle archive — son subdir est privé. */
  fun delete(m: SttModel) {
    val job = jobs[m.id]
    if (job != null) {
      job.cancelled.set(true)
      job.thread?.interrupt()
      job.thread?.join(3_000)
      jobs.remove(m.id)
    }
    if (m.archive) {
      stagingDir(m).deleteRecursively()
      modelDir(m).deleteRecursively()
    } else {
      for (f in m.files) {
        dstFile(m, f).delete()
        partFile(m, f).delete()
      }
      errFile(m).delete()
    }
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
      for (f in m.files) {
        downloadFile(job, f)
        checkCancelled(job)
      }
      if (m.archive) extractArchives(job)
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

  /** Extraction des archives téléchargées (chaque `files` d'un modèle
   *  archive = un .tar.bz2) : stream dans le staging dir, premier
   *  composant de chemin ignoré (dir racine du tarball), garde anti
   *  zip-slip, marqueur `.ok` puis bascule atomique par renommage. */
  private fun extractArchives(job: Job) {
    val m = job.model
    val staging = stagingDir(m)
    staging.deleteRecursively()
    staging.mkdirs()
    for (f in m.files) {
      val archive = dstFile(m, f)
      if (!archive.isFile) throw IOException("${f.name}: archive absente")
      BZip2CompressorInputStream(archive.inputStream().buffered()).use { bz ->
        TarArchiveInputStream(bz).use { tar ->
          var e = tar.nextEntry
          while (e != null) {
            checkCancelled(job)
            val rel = e.name.substringAfter('/', "")
            if (rel.isNotEmpty() && !e.isDirectory) {
              val out = File(staging, rel)
              // startsWith(path + separator) : sans séparateur,
              // `staging.evil/…` passerait pour un chemin interne.
              if (!out.canonicalPath.startsWith(staging.canonicalPath + File.separator)) {
                throw Fatal("archive: chemin refusé ${e.name}")
              }
              out.parentFile?.mkdirs()
              out.outputStream().use { o -> tar.copyTo(o) }
            }
            e = tar.nextEntry
          }
        }
      }
    }
    // Complétion AVANT la bascule : un .ok présent garantit un arbre
    // intégral, même si le process meurt entre rename et fin de job.
    File(staging, ".ok").writeText("ok\n")
    modelDir(m).deleteRecursively()
    if (!staging.renameTo(modelDir(m))) {
      staging.copyRecursively(modelDir(m), true)
      staging.deleteRecursively()
    }
    // L'archive extraite ne sert plus — on libère les ~150 Mo.
    for (f in m.files) dstFile(m, f).delete()
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
      // `.part` déjà complet (kill/cancel entre le dernier octet et la
      // vérif, ou IOException en toute fin de stream) : PAS de GET —
      // `Range: bytes=<size>-` renverrait 416 en boucle. On vérifie le
      // sha directement ; corrompu → re-départ à zéro.
      if (resumeAt == f.size && part.isFile) {
        if (sha256Of(part) == f.sha256) {
          dst.delete()
          if (!part.renameTo(dst)) { part.copyTo(dst, true); part.delete() }
          return
        }
        part.delete()
        resumeAt = 0
      }
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
          if (code == 429 || code == 416 || code >= 500) {
            throw IOException("${f.name}: http $code")
          }
          // 4xx hors 429/416 = faute côté client — retrier ne sert à rien.
          throw Fatal("${f.name}: http $code")
        }
        // 200 sur un resume = Range ignoré → on repart de zéro.
        val append = code == HttpURLConnection.HTTP_PARTIAL && resumeAt > 0
        var have = if (append) resumeAt else 0L
        var lastEmit = System.nanoTime()
        conn.inputStream.use { inp ->
          // Le cancel peut avoir été demandé pendant responseCode
          // (connect timeout) — vérifié avant de rouvrir le .part.
          checkCancelled(job)
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
    emit("${m.kind}_dl_progress", o)
  }

  private fun emitState(m: SttModel) {
    val o = JSObject()
    o.put("model", m.id)
    o.put("state", stateOf(m))
    emit("${m.kind}_dl_state", o)
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
