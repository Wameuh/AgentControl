package dev.agentcontrol.stt

import android.Manifest
import android.annotation.SuppressLint
import android.app.Activity
import android.content.ComponentCallbacks2
import android.content.Intent
import android.content.res.Configuration
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.MediaRecorder
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.speech.RecognitionListener
import android.speech.RecognizerIntent
import android.speech.SpeechRecognizer
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
// Unload différé du recognizer (~700 Mo) : le modèle survit entre deux
// dictées proches (rechargement = des secondes) mais est rendu si plus
// rien ne l'utilise — doc §7.5/§9 (webview + 700 Mo sur entrée de gamme).
private const val IDLE_UNLOAD_MS = 120_000L

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
  // "sherpa" = modèle téléchargé + OnlineRecognizer ; "system" =
  // SpeechRecognizer on-device (API 31+, repli zéro-modèle du doc §D —
  // qualité variable hors Pixel, opt-in settings uniquement).
  @Volatile private var engine: String = "sherpa"
  // Moteur qui possède la dictée en cours — sttStop/sttState routent
  // dessus : un changement de réglage en cours de dictée ne doit pas
  // envoyer le stop au mauvais moteur (capture sherpa laissée vivante).
  @Volatile private var activeEngine: String? = null
  private val systemStt = SystemStt()
  private var lastPartial: String = ""

  private val idleHandler = Handler(Looper.getMainLooper())
  private val idleUnload = Runnable { unloadModel() }

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
    idleHandler.removeCallbacks(idleUnload)
    // release() exige le main thread (SpeechRecognizer) — no-op si aucune
    // dictée système n'a tourné.
    activity.runOnUiThread { systemStt.release() }
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
    ret.put("engine", engine)
    ret.put("system_available", systemStt.available())
    invoke.resolve(ret)
  }

  @Command
  fun sttSetEngine(invoke: Invoke) {
    // Pris en compte à la prochaine dictée (une dictée en cours n'est
    // pas basculée — les ressources diffèrent trop pour un swap live).
    val e = invoke.getArgs().optString("engine").ifEmpty { "sherpa" }
    if (e != "sherpa" && e != "system") {
      invoke.reject("moteur inconnu: $e")
      return
    }
    engine = e
    invoke.resolve()
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
    // Moteur système : zéro modèle requis — le check modèle ne s'applique
    // qu'au chemin sherpa.
    if (engine == "system") {
      if (getPermissionState("microphone") != PermissionState.GRANTED) {
        requestMicThen(invoke)
        return
      }
      doStartSystem(invoke)
      return
    }
    if (!modelPresent()) { invoke.resolve(startResult(false, "model_missing")); return }
    if (getPermissionState("microphone") != PermissionState.GRANTED) {
      requestMicThen(invoke)
      return
    }
    doStart(invoke)
  }

  /** Grant `RECORD_AUDIO` puis re-dispatch vers le start du moteur
   *  choisi. Le callback est rappelé avec le même invoke une fois le
   *  grant système décidé (dialog OS affiché par PermissionHelper). Le
   *  slot de callback est unique → un 2e appel pendant le prompt
   *  écraserait le 1er invoke : on l'écarte proprement (check+set
   *  atomique — deux invokes peuvent arriver dans le même tick). */
  private fun requestMicThen(invoke: Invoke) {
    if (!permissionPending.compareAndSet(false, true)) {
      invoke.resolve(startResult(false, "perm_pending")); return
    }
    try {
      requestPermissionForAlias("microphone", invoke, "micPermCallback")
    } catch (e: Exception) {
      permissionPending.set(false)
      invoke.resolve(startResult(false, "perm_error"))
    }
  }

  @PermissionCallback
  private fun micPermCallback(invoke: Invoke) {
    permissionPending.set(false)
    if (getPermissionState("microphone") == PermissionState.GRANTED) {
      // Même validation qu'à l'entrée de sttStart — l'utilisateur a pu
      // changer de moteur pendant le dialog (sherpa : model check).
      if (engine == "system") doStartSystem(invoke)
      else if (!modelPresent()) invoke.resolve(startResult(false, "model_missing"))
      else doStart(invoke)
    } else {
      invoke.resolve(startResult(false, "mic_denied"))
    }
  }

  @Command
  fun sttStop(invoke: Invoke) {
    // Route sur le moteur de la dictée réellement active — pas sur le
    // réglage courant (modifiable en cours de dictée via les settings).
    val active = activeEngine ?: engine
    if (active == "system") { systemStt.stop(invoke); return }
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
    activeEngine = null
    // Dictée terminée : le recognizer (~700 Mo) est rendu après un délai
    // — une dictée suivante proche le réutilise encore chaud (doStart
    // annule le timer), l'idle le libère sinon (trimMemory reste la
    // pression d'urgence).
    idleHandler.postDelayed(idleUnload, IDLE_UNLOAD_MS)
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
    // Nouvelle dictée sherpa : l'unload idle post-stop ne doit pas libérer
    // le modèle sous elle.
    idleHandler.removeCallbacks(idleUnload)
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
        activeEngine = "sherpa"
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
    activeEngine = null
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
      // Jamais sous un thread de capture zombie (join timeout) dont le
      // read bloqué peut se réveiller sur un ptr mort. Le garde est
      // engine-aware : pendant une dictée SYSTÈME (recording=true sans
      // capture sherpa), les ressources sherpa ne sont pas en usage —
      // le trim doit pouvoir libérer les ~700 Mo quand même.
      if (captureActive) return
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

  // ── Moteur système (repli zéro-modèle, doc §D/§7.5) ─────────────────
  //
  // SpeechRecognizer on-device (API 31+, Android System Intelligence) :
  // partials natifs via EXTRA_PARTIAL_RESULTS, dictée continue par
  // relance à chaque onResults. Tout l'objet vit sur le main thread —
  // SpeechRecognizer l'exige (Looper), les @Command y sont relayées via
  // runOnUiThread. Pas d'AudioRecord : le niveau vient de onRmsChanged.

  private fun doStartSystem(invoke: Invoke) {
    if (!systemStt.available()) {
      invoke.resolve(startResult(false, "engine_error"))
      return
    }
    idleHandler.removeCallbacks(idleUnload)
    // recording/activeEngine avancés AVANT le post : un stt_stop qui
    // passerait entre le dispatch et l'exécution du runnable se route
    // déjà « system » — et le runnable le détecte (activeEngine effacé
    // par le stop) → release immédiat au lieu de ressusciter l'état.
    recording = true
    activeEngine = "system"
    activity.runOnUiThread {
      val reason = systemStt.start()
      if (reason == null) {
        if (activeEngine == "system") invoke.resolve(startResult(true))
        else {
          systemStt.release()
          invoke.resolve(startResult(false, "busy"))
        }
      } else {
        recording = false
        activeEngine = null
        invoke.resolve(startResult(false, reason))
      }
    }
  }

  @SuppressLint("NewApi") // createOnDeviceSpeechRecognizer : API 31+, gaté par available()
  private inner class SystemStt {
    // @Volatile : lu par stt_stop depuis le command-thread (dictée
    // réellement active ?) — écrit exclusivement sur le main thread.
    @Volatile private var sr: SpeechRecognizer? = null
    // Dernier partial émis — stt_stop le renvoie comme « dernier
    // segment » (même sémantique que le getResult final du chemin sherpa).
    @Volatile private var lastText: String = ""

    fun available(): Boolean =
      Build.VERSION.SDK_INT >= 31 &&
        try { SpeechRecognizer.isOnDeviceRecognitionAvailable(activity) }
        catch (e: Exception) { false }

    /** Main thread uniquement. null = démarré, sinon reason d'échec. */
    fun start(): String? {
      if (sr != null) return null // déjà en cours
      val r = try {
        SpeechRecognizer.createOnDeviceSpeechRecognizer(activity)
      } catch (e: Exception) { return "engine_error" }
      r.setRecognitionListener(Listener())
      sr = r
      try {
        r.startListening(intentOf())
      } catch (e: Exception) {
        r.destroy()
        sr = null
        return "audio_init"
      }
      lastText = ""
      return null
    }

    fun stop(invoke: Invoke) {
      recording = false
      activeEngine = null
      // « Dernier segment » seulement si une dictée tournait vraiment —
      // sinon le dernier partial périmé (dictée morte via un onError
      // fatal, ou jamais lancée) serait inséré en fantôme, là où le
      // chemin sherpa renvoie "" (stream == null → texte vide).
      val text = if (sr != null) lastText else ""
      activity.runOnUiThread { release() }
      val ret = JSObject()
      ret.put("text", text)
      ret.put("lang", language)
      invoke.resolve(ret)
      // Même politique qu'au stop sherpa : le modèle éventuellement
      // résident est rendu après délai — l'idle couvre les deux moteurs.
      idleHandler.postDelayed(idleUnload, IDLE_UNLOAD_MS)
    }

    /** Main thread. */
    fun release() {
      try { sr?.stopListening() } catch (_: Exception) {}
      try { sr?.destroy() } catch (_: Exception) {}
      sr = null
      lastText = ""
    }

    private fun firstResult(b: Bundle?): String? =
      b?.getStringArrayList(SpeechRecognizer.RESULTS_RECOGNITION)
        ?.firstOrNull { it.isNotBlank() }

    private inner class Listener : RecognitionListener {
      override fun onReadyForSpeech(p: Bundle?) {}
      override fun onBeginningOfSpeech() {}
      override fun onRmsChanged(rmsdB: Float) {
        // ~[-2..10] dB → [0..1], même échelle que emitLevel sherpa.
        val obj = JSObject()
        obj.put("level", minOf(1f, maxOf(0f, (rmsdB + 2f) / 12f)))
        trigger("stt_level", obj)
      }
      override fun onBufferReceived(b: ByteArray?) {}
      override fun onEndOfSpeech() {}
      override fun onPartialResults(p: Bundle?) {
        val t = firstResult(p) ?: return
        lastText = t
        emitPartial(t)
      }
      override fun onResults(p: Bundle?) {
        val t = firstResult(p)
        if (t != null) {
          lastText = t
          emitResult(t)
        }
        // Le recognizer système est one-shot par startListening : on le
        // relance tant que la dictée n'a pas été stoppée.
        if (recording) sr?.let { try { it.startListening(intentOf()) } catch (_: Exception) {} }
      }
      override fun onError(code: Int) {
        when (code) {
          // Erreurs transitoires usuelles en dictée : la boucle reprend
          // (BUSY = cas typique stop→start rapide, le recognizer reprend).
          SpeechRecognizer.ERROR_NO_MATCH,
          SpeechRecognizer.ERROR_SPEECH_TIMEOUT,
          SpeechRecognizer.ERROR_RECOGNIZER_BUSY -> {
            if (recording) sr?.let { try { it.startListening(intentOf()) } catch (_: Exception) {} }
          }
          else -> {
            // Fatale : la dictée meurt — stt_state verra recording=false
            // au prochain probe (le resync UI de l'UX composer le couvre).
            recording = false
            activeEngine = null
            release()
            idleHandler.postDelayed(idleUnload, IDLE_UNLOAD_MS)
          }
        }
      }
      override fun onEvent(t: Int, b: Bundle?) {}
    }

    private fun intentOf(): Intent {
      val i = Intent(RecognizerIntent.ACTION_RECOGNIZE_SPEECH)
        .putExtra(RecognizerIntent.EXTRA_LANGUAGE_MODEL, RecognizerIntent.LANGUAGE_MODEL_FREE_FORM)
        .putExtra(RecognizerIntent.EXTRA_PARTIAL_RESULTS, true)
        .putExtra(RecognizerIntent.EXTRA_MAX_RESULTS, 1)
        .putExtra("android.speech.extra.DICTATION_MODE", true)
      if (language != "auto") i.putExtra(RecognizerIntent.EXTRA_LANGUAGE, language)
      return i
    }
  }
}
