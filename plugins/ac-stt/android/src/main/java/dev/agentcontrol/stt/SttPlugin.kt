package dev.agentcontrol.stt

import android.Manifest
import android.app.Activity
import android.content.ComponentCallbacks2
import android.content.res.Configuration
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.MediaRecorder
import android.webkit.WebView
import app.tauri.annotation.Command
import app.tauri.annotation.Permission
import app.tauri.annotation.PermissionCallback
import app.tauri.annotation.TauriPlugin
import app.tauri.PermissionState
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import com.k2fsa.sherpa.onnx.FeatureConfig
import com.k2fsa.sherpa.onnx.OnlineModelConfig
import com.k2fsa.sherpa.onnx.OnlineRecognizer
import com.k2fsa.sherpa.onnx.OnlineRecognizerConfig
import com.k2fsa.sherpa.onnx.OnlineStream
import com.k2fsa.sherpa.onnx.OnlineTransducerModelConfig
import java.io.File
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.concurrent.thread
import kotlin.math.sqrt

private const val SAMPLE_RATE = 16000

// Modèle téléchargé à la demande par le chantier « download » (item
// parallèle) dans app_data/models/stt/ = filesDir/models/stt — le core
// ne fait qu'attendre ces fichiers (4 GET HF du doc §6.1).
private const val MODEL_SUBDIR = "models/stt"
private val MODEL_FILES = arrayOf(
  "encoder.int8.onnx",
  "decoder.int8.onnx",
  "joiner.int8.onnx",
  "tokens.txt",
)

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

  @Volatile private var recognizer: OnlineRecognizer? = null
  @Volatile private var stream: OnlineStream? = null
  @Volatile private var recorder: AudioRecord? = null
  @Volatile private var recording = false
  // Thread de capture encore vivant alors que recording=false (join
  // timeout sur un read bloqué) : interdit tout release de stream/
  // recorder/recognizer tant qu'il est levé — sinon UAF JNI.
  @Volatile private var captureActive = false
  private val permissionPending = AtomicBoolean(false)
  private var recordThread: Thread? = null
  private var trimCallbacks: ComponentCallbacks2? = null

  // "auto" = auto-détect 40 locales du modèle ; forcée via stt_set_language
  // (énoncés courts, cf. doc §6.3/§9).
  @Volatile private var language: String = "auto"
  private var lastPartial: String = ""

  override fun load(webView: WebView) {
    downloader = ModelDownloader(activity.filesDir) { ev, payload ->
      trigger(ev, payload)
    }
    // Le recognizer pèse ~700 Mo de RAM — il ne vit que du 1er start à
    // un trimMemory (le chantier UX appellera stt_stop à la sortie du
    // chat ; l'unload RAM définitif est le chantier « polish »).
    val cb = object : ComponentCallbacks2 {
      override fun onConfigurationChanged(c: Configuration) {}
      override fun onLowMemory() { unloadModel() }
      override fun onTrimMemory(level: Int) {
        if (level >= ComponentCallbacks2.TRIM_MEMORY_RUNNING_LOW) unloadModel()
      }
    }
    activity.registerComponentCallbacks(cb)
    trimCallbacks = cb
  }

  @Deprecated("Deprecated in base Plugin")
  override fun onDestroy() {
    trimCallbacks?.let { activity.unregisterComponentCallbacks(it) }
    stopCapture()
    unloadModel()
  }

  // ── Commandes ──────────────────────────────────────────────────────

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
  fun sttState(invoke: Invoke) {
    val ret = JSObject()
    ret.put("platform", "android")
    ret.put("model_dir", modelDir().absolutePath)
    ret.put("model_present", modelPresent())
    ret.put("model_loaded", recognizer != null)
    ret.put("recording", recording)
    ret.put("permission", permissionLabel())
    ret.put("language", language)
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
    val m = modelArg(invoke) ?: return
    downloader.cancel(m.id)
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

  @Command
  fun sttStart(invoke: Invoke) {
    if (recording) { invoke.resolve(startResult(true)); return }
    if (!modelPresent()) { invoke.resolve(startResult(false, "model_missing")); return }
    if (getPermissionState("microphone") != PermissionState.GRANTED) {
      // Le callback est rappelé avec le même invoke une fois le grant
      // système décidé (dialog OS affiché par PermissionHelper). Le
      // slot de callback est unique → un 2e appel pendant le prompt
      // écraserait le 1er invoke : on l'écarte proprement (check+set
      // atomique — deux invokes peuvent arriver dans le même tick).
      if (!permissionPending.compareAndSet(false, true)) {
        invoke.resolve(startResult(false, "perm_pending")); return
      }
      try {
        requestPermissionForAlias("microphone", invoke, "micPermCallback")
      } catch (e: Exception) {
        permissionPending.set(false)
        invoke.resolve(startResult(false, "perm_error"))
      }
      return
    }
    doStart(invoke)
  }

  @PermissionCallback
  private fun micPermCallback(invoke: Invoke) {
    permissionPending.set(false)
    if (getPermissionState("microphone") == PermissionState.GRANTED) doStart(invoke)
    else invoke.resolve(startResult(false, "mic_denied"))
  }

  @Command
  fun sttStop(invoke: Invoke) {
    var text = ""
    synchronized(this) {
      recording = false
      // Si le join échoue (read bloqué), on laisse la boucle sortir
      // seule — elle meurt en une itération puisque recording=false —
      // et on ne release rien : un release pendant que la boucle
      // utilise encore les ptr natifs serait un JNI crash. On garde
      // recordThread (handle de re-join) et captureActive reste levé
      // → unloadModel/stopCapture/doStart refusent de toucher les
      // ressources tant que le zombie vit.
      val t = recordThread
      if (t != null) {
        t.join(2000)
        if (t.isAlive) {
          val early = JSObject()
          early.put("text", "")
          early.put("lang", language)
          invoke.resolve(early)
          return
        }
        recordThread = null
      } else if (captureActive) {
        // Zombie d'un stop précédent : mêmes raisons, mêmes refus.
        val early = JSObject()
        early.put("text", "")
        early.put("lang", language)
        invoke.resolve(early)
        return
      }
      val s = stream
      stream = null
      recorder?.let {
        try { it.stop() } catch (_: Exception) {}
        it.release()
      }
      recorder = null
      if (s != null) {
        s.inputFinished()
        val r = recognizer
        if (r != null) {
          while (r.isReady(s)) r.decode(s)
          text = r.getResult(s).text
        }
        s.release()
      }
    }
    val ret = JSObject()
    ret.put("text", text)
    ret.put("lang", language)
    invoke.resolve(ret)
  }

  @Command
  fun sttSetLanguage(invoke: Invoke) {
    // optString rend "" (pas null) pour une clé absente → ifEmpty
    // garde le repli "auto" quelle que soit la forme d'appel.
    val lang = invoke.getArgs().optString("lang").ifEmpty { "auto" }
    language = lang
    synchronized(this) {
      // Effet immédiat si une dictée est en cours (option par stream) —
      // sous le même moniteur que les release pour ne pas toucher un
      // ptr natif libéré par un stop/unload concurrent.
      stream?.setOption("language", lang)
    }
    invoke.resolve()
  }

  // ── Capture + décode ────────────────────────────────────────────────

  private fun doStart(invoke: Invoke) {
    // Le load du modèle (~700 Mo, des secondes) reste HORS du moniteur :
    // onTrimMemory → unloadModel() tourne sur le main thread et ne doit
    // pas geler l'app en attendant la fin du chargement.
    try {
      ensureRecognizer()
    } catch (e: Exception) {
      invoke.resolve(startResult(false, "engine_error"))
      return
    }
    synchronized(this) {
      if (recording) { invoke.resolve(startResult(true)); return }
      // Zombie d'un ancien stop : son recorder tient encore le micro,
      // republier stream/recorder écraserait ses ptr natifs sous lui.
      if (captureActive) { invoke.resolve(startResult(false, "busy")); return }
      // Reliquat d'un stop arrivé à timeout (zombie depuis mort) :
      // ptr natifs valides mais orphelins — on les release avant de
      // republier, sinon fuite jusqu'au prochain unloadModel.
      stream?.release()
      recorder?.let {
        try { it.stop() } catch (_: Exception) {}
        it.release()
      }
      stream = null; recorder = null
      var rec: AudioRecord? = null
      var s: OnlineStream? = null
      var started = false
      try {
        val minBuf = AudioRecord.getMinBufferSize(
          SAMPLE_RATE, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT
        )
        rec = AudioRecord(
          MediaRecorder.AudioSource.VOICE_RECOGNITION,
          SAMPLE_RATE, AudioFormat.CHANNEL_IN_MONO,
          AudioFormat.ENCODING_PCM_16BIT, maxOf(minBuf, SAMPLE_RATE / 5)
        )
        if (rec.state != AudioRecord.STATE_INITIALIZED) {
          throw IllegalStateException("AudioRecord not initialized")
        }
        s = recognizer!!.createStream()
        if (language != "auto") s.setOption("language", language)
        // Tout est alloué : publish puis capture. startRecording()
        // lève IllegalStateException si le micro est indisponible —
        // couverte par le catch (le thread n'est pas encore né →
        // release sûr).
        stream = s
        recorder = rec
        lastPartial = ""
        rec.startRecording()
        recording = true
        captureActive = true
        recordThread = thread(name = "stt-capture") { captureLoop(s!!) }
        started = true
      } catch (e: SecurityException) {
        s?.release(); rec?.release()
        stream = null; recorder = null
        recording = false; captureActive = false
        invoke.resolve(startResult(false, "mic_denied"))
      } catch (e: Exception) {
        s?.release(); rec?.release()
        stream = null; recorder = null
        recording = false; captureActive = false
        invoke.resolve(startResult(false, "audio_init"))
      }
      if (started) invoke.resolve(startResult(true))
    }
  }

  // Lecture ~100 ms par itération : vu-mètre fluide + partials denses.
  // Le chunk décode interne du modèle est 560 ms (choix du doc §7).
  private fun captureLoop(s: OnlineStream) {
    val buf = ShortArray(SAMPLE_RATE / 10)
    try {
      while (true) {
        val rec = recorder ?: break
        if (!recording) break
        val n = try { rec.read(buf, 0, buf.size) } catch (e: Exception) { break }
        if (n <= 0) continue
        emitLevel(buf, n)
        val samples = FloatArray(n) { buf[it] / 32768.0f }
        s.acceptWaveform(samples, SAMPLE_RATE)
        val r = recognizer ?: break
        while (r.isReady(s)) r.decode(s)
        if (r.isEndpoint(s)) {
          val text = r.getResult(s).text
          r.reset(s)
          if (text.isNotBlank()) emitResult(text)
          lastPartial = ""
        }
        val partial = r.getResult(s).text
        if (partial != lastPartial) {
          lastPartial = partial
          emitPartial(partial)
        }
      }
    } finally {
      // Toute sortie anormale (read cassé, recognizer perdu) doit rendre
      // l'état cohérent — sinon stt_state mentirait (« recording ») et
      // les ressources resteraient gelées pour unloadModel/doStart.
      recording = false
      captureActive = false
    }
  }

  private fun emitLevel(buf: ShortArray, n: Int) {
    var sum = 0.0
    for (i in 0 until n) { val v = buf[i].toDouble(); sum += v * v }
    val obj = JSObject()
    obj.put("level", minOf(1.0, sqrt(sum / n) / 4096.0))
    trigger("stt_level", obj)
  }

  private fun emitPartial(text: String) {
    val obj = JSObject()
    obj.put("text", text)
    trigger("stt_partial", obj)
  }

  private fun emitResult(text: String) {
    val obj = JSObject()
    obj.put("text", text)
    obj.put("lang", language)
    trigger("stt_result", obj)
  }

  private fun stopCapture() {
    recording = false
    // Join HORS moniteur (appelé depuis onDestroy/main thread : ne pas
    // bloquer les commandes JNI), release SOUS moniteur — mêmes
    // règles que sttStop/sttSetLanguage.
    val t = recordThread
    t?.join(2000)
    // Thread encore vivant (read bloqué) : on ne release rien — la
    // boucle meurt seule à l'itération suivante (recording=false),
    // captureActive reste levé et garde les ressources.
    if (t?.isAlive == true) return
    synchronized(this) {
      // Null sous moniteur : un doStart concurrent publierait sinon un
      // handle que cette ligne écraserait.
      recordThread = null
      if (captureActive) return
      recorder?.let {
        try { it.stop() } catch (_: Exception) {}
        it.release()
      }
      recorder = null
      stream?.release()
      stream = null
    }
  }

  private fun ensureRecognizer() {
    if (recognizer != null) return
    // Construction hors moniteur (charge ~700 Mo) : publication sous
    // lock — un concurrent aurait fait de même, on libère le doublon.
    val dir = modelDir()
    val r = OnlineRecognizer(
      config = OnlineRecognizerConfig(
        featConfig = FeatureConfig(sampleRate = SAMPLE_RATE, featureDim = 80),
        modelConfig = OnlineModelConfig(
          transducer = OnlineTransducerModelConfig(
            encoder = "$dir/encoder.int8.onnx",
            decoder = "$dir/decoder.int8.onnx",
            joiner = "$dir/joiner.int8.onnx",
          ),
          tokens = "$dir/tokens.txt",
          numThreads = 2,
          provider = "cpu",
          modelType = "nemo_transducer",
        ),
      )
    )
    synchronized(this) {
      if (recognizer == null) recognizer = r else r.release()
    }
  }

  private fun unloadModel() {
    synchronized(this) {
      // jamais pendant une dictée, ni sous un thread zombie (join
      // timeout) dont le read bloqué peut se réveiller sur un ptr mort.
      if (recording || captureActive) return
      stream?.release()
      stream = null
      recognizer?.release()
      recognizer = null
    }
  }

  private fun modelDir() = File(activity.filesDir, MODEL_SUBDIR)

  private fun modelPresent() = MODEL_FILES.all { File(modelDir(), it).isFile }

  // PermissionState.toString() rend déjà le label ("granted", "denied",
  // "prompt", "prompt-with-rationale") ; null = alias inconnu.
  private fun permissionLabel(): String =
    getPermissionState("microphone")?.toString() ?: "unknown"

  private fun startResult(started: Boolean, reason: String? = null): JSObject {
    val ret = JSObject()
    ret.put("started", started)
    if (reason != null) ret.put("reason", reason)
    return ret
  }
}
