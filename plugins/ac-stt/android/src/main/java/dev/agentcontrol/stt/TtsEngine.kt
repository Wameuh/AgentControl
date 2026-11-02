package dev.agentcontrol.stt

import android.app.Activity
import android.content.Context
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioManager
import android.media.AudioTrack
import android.os.Bundle
import android.os.Looper
import android.speech.tts.TextToSpeech
import android.speech.tts.UtteranceProgressListener
import app.tauri.plugin.JSObject
import com.k2fsa.sherpa.onnx.GenerationConfig
import com.k2fsa.sherpa.onnx.OfflineTts
import com.k2fsa.sherpa.onnx.OfflineTtsConfig
import com.k2fsa.sherpa.onnx.OfflineTtsKokoroModelConfig
import com.k2fsa.sherpa.onnx.OfflineTtsModelConfig
import com.k2fsa.sherpa.onnx.OfflineTtsVitsModelConfig
import java.io.File
import java.util.Locale
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger
import kotlin.concurrent.thread
import org.json.JSONArray

// ── Moteur TTS on-device (backlog #138) ─────────────────────────────────────
//
// Deux moteurs derrière le même contrat d'events (tts_started / tts_done /
// tts_error) :
//  - "sherpa" : OfflineTts de l'AAR sherpa-onnx déjà embarquée pour le STT —
//    modèles kokoro/vits téléchargés à la demande (TTS_MODELS, descripteurs
//    archive du ModelDownloader), synthèse chunkée par phrase streamée dans
//    un AudioTrack PCM float → la voix démarre dès la 1re phrase, pas après
//    la synthèse complète du message.
//  - "system" : android.speech.tts.TextToSpeech — zéro download, qualité
//    variable selon l'appareil ; repli quand aucun modèle n'est installé
//    (engine "auto") ou choix explicite de l'utilisateur.
// Une lecture à la fois : speak() préempte l'utterance en cours ; la file
// éventuelle vit côté client.

/** Voix exposée au front : id logique (args `voice` de tts_speak), sid
 *  sherpa, voice espeak-ng pour la phonémisation kokoro, label settings. */
data class TtsVoice(val id: String, val sid: Int, val lang: String, val label: String)

/** Catalogue d'un modèle TTS : type de config OfflineTts + voix proposées.
 *  Kokoro v1_0 = 53 speakers multi-lang (la v1_1 = zh/en only — sans voix
 *  française ; ff_siwis n'existe qu'en v1_0). */
data class TtsModelMeta(val engine: String, val voices: List<TtsVoice>)

val TTS_META = mapOf(
  "kokoro-int8-multi-lang-v1_0" to TtsModelMeta(
    engine = "kokoro",
    voices = listOf(
      TtsVoice("ff_siwis", 30, "fr", "Siwis — Français (F)"),
      TtsVoice("af_heart", 3, "en-us", "Heart — English US (F)"),
      TtsVoice("af_bella", 2, "en-us", "Bella — English US (F)"),
      TtsVoice("af_sarah", 9, "en-us", "Sarah — English US (F)"),
      TtsVoice("am_adam", 11, "en-us", "Adam — English US (M)"),
      TtsVoice("am_michael", 16, "en-us", "Michael — English US (M)"),
      TtsVoice("bf_emma", 21, "en-gb", "Emma — English UK (F)"),
      TtsVoice("bm_george", 26, "en-gb", "George — English UK (M)"),
      TtsVoice("ef_dora", 28, "es", "Dora — Español (F)"),
      TtsVoice("em_alex", 29, "es", "Alex — Español (M)"),
      TtsVoice("if_sara", 35, "it", "Sara — Italiano (F)"),
      TtsVoice("pf_dora", 42, "pt", "Dora — Português (F)"),
      TtsVoice("pm_alex", 43, "pt", "Alex — Português (M)"),
      TtsVoice("hf_alpha", 31, "hi", "Alpha — हिन्दी (F)"),
      TtsVoice("jf_alpha", 37, "ja", "Alpha — 日本語 (F)"),
      TtsVoice("jm_kumo", 41, "ja", "Kumo — 日本語 (M)"),
      TtsVoice("zf_xiaoxiao", 47, "cmn", "Xiaoxiao — 中文 (F)"),
      TtsVoice("zm_yunxi", 50, "cmn", "Yunxi — 中文 (M)"),
    ),
  ),
  "vits-piper-fr_FR-siwis-medium-int8" to TtsModelMeta(
    engine = "vits",
    voices = listOf(TtsVoice("siwis", 0, "fr", "Siwis — Français")),
  ),
)

private const val SPEED_MIN = 0.5f
private const val SPEED_MAX = 2.0f

class TtsEngine(
  private val activity: Activity,
  private val emit: (event: String, payload: JSObject) -> Unit,
) {
  @Volatile private var tts: OfflineTts? = null
  /** Modèle dont `tts` est l'instance — un changement de modèle impose
   *  une reconstruction (la config porte les chemins du modèle). */
  @Volatile private var ttsModel: SttModel? = null
  @Volatile private var sysTts: TextToSpeech? = null
  /** null = jamais initialisé ; tri-état pour tts_state. */
  @Volatile private var sysTtsOk: Boolean? = null
  @Volatile private var speaking = false
  @Volatile private var currentTag = ""
  @Volatile private var speakThread: Thread? = null
  /** Sérialise la section critique de speak() (stop → publication du ctl
   *  → lancement du thread) : sans elle deux tts_speak concurrents
   *  peuvent publier deux ctl et rendre le premier in-abortable. */
  private val speakLock = Any()
  /** Sérialise l'init du TextToSpeech système — deux runSystem
   *  concurrents (zombie + courant) créeraient sinon deux instances
   *  dont l'une fuirait sans shutdown(). */
  private val sysLock = Any()
  /** Abort PAR lecture : un flag partagé serait réarmé par le speak
   *  suivant et un zombie survivrait. stop() lève le ctl courant ;
   *  speak() en publie un neuf. */
  private class SpeakCtl { val abort = AtomicBoolean(false) }
  @Volatile private var speakCtl = SpeakCtl()
  /** Threads de synthèse vivants — un zombie peut survivre au join à
   *  timeout de stop() ET être éjecté du handle speakThread par le
   *  speak suivant : seul un compteur garantit qu'aucun generate()
   *  natif ne tourne avant un release() (UAF JNI sinon). */
  private val activeSpeaks = AtomicInteger(0)
  /** Instances OfflineTts remplacées dont la release est différée —
   *  balayées par sweepRetired() quand plus aucun thread ne vit. */
  private val retired = mutableListOf<OfflineTts>()
  private var track: AudioTrack? = null
  private val audioManager =
    activity.getSystemService(Context.AUDIO_SERVICE) as AudioManager
  // Appel entrant ou autre app prenant le focus → stop propre (canal
  // media, gain transitoire — jamais de lecture qui couvre un appel).
  private val focusListener = AudioManager.OnAudioFocusChangeListener { c ->
    if (c == AudioManager.AUDIOFOCUS_LOSS ||
        c == AudioManager.AUDIOFOCUS_LOSS_TRANSIENT) {
      stop()
    }
  }

  // ── État ─────────────────────────────────────────────────────────────

  fun state(downloader: ModelDownloader): JSObject {
    val voices = JSONArray()
    for (m in TTS_MODELS) {
      if (downloader.stateOf(m) != "present") continue
      for (v in TTS_META[m.id]?.voices ?: emptyList()) {
        val o = JSObject()
        o.put("id", v.id)
        o.put("label", v.label)
        o.put("model", m.id)
        o.put("lang", v.lang)
        voices.put(o)
      }
    }
    val o = JSObject()
    o.put("platform", "android")
    o.put("sherpa_ready", voices.length() > 0)
    o.put("model_loaded", tts != null)
    o.put("system_ready", sysTtsOk != false)
    o.put("speaking", speaking)
    o.put("voices", voices)
    return o
  }

  // ── Commandes ────────────────────────────────────────────────────────

  /** Résolution du modèle installé : `modelId` explicite sinon le premier
   *  présent dans l'ordre TTS_MODELS (kokoro prioritaire). null = rien
   *  d'installé. */
  private fun resolveModel(downloader: ModelDownloader, modelId: String): SttModel? {
    if (modelId.isNotEmpty()) {
      return downloader.findModel(modelId, "tts")
        ?.takeIf { downloader.stateOf(it) == "present" }
    }
    return TTS_MODELS.firstOrNull { downloader.stateOf(it) == "present" }
  }

  fun speak(
    downloader: ModelDownloader,
    text: String,
    tag: String,
    engineReq: String,
    modelId: String,
    voiceId: String,
    lang: String,
    speed: Float,
  ): JSObject {
    val m = resolveModel(downloader, modelId)
    val eng = when (engineReq) {
      "sherpa" -> "sherpa"
      "system" -> "system"
      else -> if (m != null) "sherpa" else "system" // "auto"
    }
    if (eng == "sherpa" && m == null) return speakResult(false, "model_missing")
    if (text.isBlank()) return speakResult(false, "empty")
    val clamped = speed.coerceIn(SPEED_MIN, SPEED_MAX)
    val v = m?.let { meta ->
      val vs = TTS_META[meta.id]?.voices ?: emptyList()
      vs.find { it.id == voiceId } ?: vs.firstOrNull()
    }
    if (eng == "sherpa" && v == null) return speakResult(false, "engine_error")
    val useLang = lang.ifEmpty { v?.lang ?: "" }

    // Une lecture à la fois : préemption propre de l'utterance courante —
    // APRÈS toutes les validations (un speak refusé ne tue pas l'écoute).
    // stop() + publication + lancement dans la même section critique :
    // deux speak concurrents ne peuvent plus s'entrelacer (le premier
    // ctl publié resterait sinon éjecté → lecture in-abortable).
    synchronized(speakLock) {
      stop()
      speaking = true
      currentTag = tag
      val ctl = SpeakCtl()
      speakCtl = ctl
      // Les DEUX moteurs partent sur le speakThread : le chemin system
      // bloque aussi (init TextToSpeech par latch) — jamais sur le
      // thread de la commande (peut être le main thread → ANR/deadlock).
      val dir = m?.let { downloader.modelDir(it) }
      speakThread = thread(name = "tts-speak") {
        activeSpeaks.incrementAndGet()
        try {
          if (eng == "sherpa") {
            runSherpa(m!!, dir!!, v!!, useLang, clamped, text, tag, ctl)
          } else {
            runSystem(useLang, clamped, text, tag, ctl)
          }
        } finally {
          activeSpeaks.decrementAndGet()
          if (speakThread === Thread.currentThread()) speakThread = null
          sweepRetired()
        }
      }
    }
    val ret = speakResult(true, null)
    ret.put("engine", eng)
    return ret
  }

  fun stop() {
    val ctl = speakCtl
    ctl.abort.set(true)
    val t = speakThread
    if (t?.isAlive == true && t !== Thread.currentThread()) t.join(2_000)
    // Le handle est conservé (pas de null) : un thread encore vivant
    // après le join reste visible pour unload() — release() d'un
    // OfflineTts pendant un generate() natif = UAF JNI.
    synchronized(this) {
      track?.let {
        try { it.pause(); it.flush(); it.stop() } catch (_: Exception) {}
        it.release()
      }
      track = null
    }
    synchronized(sysLock) { sysTts?.stop() }
    audioManager.abandonAudioFocus(focusListener)
    sweepRetired()
    // Ne pas effacer l'état d'un speak plus récent : si un nouveau ctl a
    // été publié pendant le join, `speaking`/`currentTag` lui
    // appartiennent désormais.
    if (speakCtl === ctl) {
      speaking = false
      currentTag = ""
    }
  }

  /** Libération RAM (trimMemory/onDestroy/ttsDlDelete) : le moteur
   *  sherpa pèse ~150-200 Mo — jamais pendant une lecture en cours ni
   *  tant qu'un thread de synthèse est vivant (il pourrait être encore
   *  dans generate() après un join à timeout dans stop()). */
  fun unload() {
    if (speaking || activeSpeaks.get() > 0) return
    synchronized(this) {
      if (activeSpeaks.get() > 0) return
      for (r in retired) r.release()
      retired.clear()
      tts?.release()
      tts = null
      ttsModel = null
      synchronized(sysLock) {
        sysTts?.shutdown()
        sysTts = null
        sysTtsOk = null
      }
    }
  }

  // ── Sherpa (sherpa-onnx OfflineTts → AudioTrack) ──────────────────────

  private fun ensureTts(m: SttModel, dir: File) {
    if (tts != null && ttsModel?.id == m.id) return
    val meta = TTS_META[m.id] ?: throw IllegalStateException("modèle inconnu")
    // API Kotlin de l'AAR : data classes (pas les builders Java).
    val modelCfg = when (meta.engine) {
      "kokoro" -> OfflineTtsModelConfig(
        kokoro = OfflineTtsKokoroModelConfig(
          model = "$dir/model.int8.onnx",
          voices = "$dir/voices.bin",
          tokens = "$dir/tokens.txt",
          dataDir = "$dir/espeak-ng-data",
          // dict/ = hétéronymes anglais ; *-zh.fst = normalisation zh
          // (dates/nombres) — tous embarqués dans l'archive v1_0.
          dictDir = "$dir/dict",
          lexicon =
            "$dir/lexicon-us-en.txt,$dir/lexicon-gb-en.txt,$dir/lexicon-zh.txt",
          ruleFsts = "$dir/date-zh.fst,$dir/phone-zh.fst,$dir/number-zh.fst",
        ),
        numThreads = 4, // défaut upstream kokoro (latence synthèse)
        provider = "cpu",
      )
      else -> OfflineTtsModelConfig(
        vits = OfflineTtsVitsModelConfig(
          model = "$dir/fr_FR-siwis-medium.onnx",
          tokens = "$dir/tokens.txt",
          dataDir = "$dir/espeak-ng-data",
        ),
        numThreads = 2,
        provider = "cpu",
      )
    }
    val engine = OfflineTts(config = OfflineTtsConfig(model = modelCfg))
    synchronized(this) {
      if (tts != null && ttsModel?.id == m.id) { engine.release(); return }
      // Release différée : un speakThread zombie peut encore utiliser
      // l'ancienne instance (cf. champ `retired`).
      tts?.let { retired.add(it) }
      tts = engine
      ttsModel = m
    }
  }

  /** Libère les moteurs retirés — seulement quand aucun thread de
   *  synthèse n'est vivant (appelée en fin de runSherpa, stop, unload). */
  private fun sweepRetired() {
    if (activeSpeaks.get() > 0) return
    synchronized(this) {
      if (activeSpeaks.get() > 0) return
      for (r in retired) r.release()
      retired.clear()
    }
  }

  private fun runSherpa(
    m: SttModel, dir: File, v: TtsVoice, lang: String, speed: Float,
    text: String, tag: String, ctl: SpeakCtl,
  ) {
    var tr: AudioTrack? = null
    var frames = 0L
    try {
      ensureTts(m, dir)
      if (ctl.abort.get()) return
      val engine = tts ?: throw IllegalStateException("engine gone")
      val rate = engine.sampleRate()
      val minBuf = AudioTrack.getMinBufferSize(
        rate, AudioFormat.CHANNEL_OUT_MONO, AudioFormat.ENCODING_PCM_FLOAT
      )
      tr = AudioTrack(
        AudioAttributes.Builder()
          .setUsage(AudioAttributes.USAGE_MEDIA)
          .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH)
          .build(),
        AudioFormat.Builder()
          .setSampleRate(rate)
          .setChannelMask(AudioFormat.CHANNEL_OUT_MONO)
          .setEncoding(AudioFormat.ENCODING_PCM_FLOAT)
          .build(),
        maxOf(minBuf, rate), // ~1 s de tampon
        AudioTrack.MODE_STREAM,
        AudioManager.AUDIO_SESSION_ID_GENERATE,
      )
      synchronized(this) {
        if (ctl.abort.get()) { tr.release(); return }
        track = tr
      }
      requestFocus()
      tr.play()
      emitEvent("tts_started", tag)

      val cfg = GenerationConfig()
      cfg.sid = v.sid
      cfg.speed = speed
      // Langue de phonémisation espeak-ng par appel (kokoro ≥1.0 —
      // extra["lang"] pris en charge par le JNI, cf. impl C++).
      if (lang.isNotEmpty()) cfg.extra = mapOf("lang" to lang)
      // Le callback écrit dans SON track local `tr` — jamais le champ
      // `track` partagé (une lecture plus récente a pu le republier).
      val callback: (FloatArray) -> Int = cb@{ samples ->
        if (ctl.abort.get()) return@cb 0
        var off = 0
        // WRITE_BLOCKING : backpressure naturelle de la lecture.
        while (off < samples.size && !ctl.abort.get()) {
          val n = try {
            tr?.write(samples, off, samples.size - off,
                      AudioTrack.WRITE_BLOCKING) ?: -1
          } catch (_: Exception) { -1 }
          if (n <= 0) break
          off += n
          frames += n
        }
        if (ctl.abort.get()) 0 else 1
      }
      try {
        engine.generateWithConfigAndCallback(text, cfg, callback)
      } catch (_: UnsatisfiedLinkError) {
        // Vieil AAR sans generateWithConfigAndCallback : synthèse d'un
        // bloc puis écriture — la 1re parole tarde mais reste correcte.
        val audio = engine.generate(text, v.sid, speed)
        val s = audio.samples
        var off = 0
        while (off < s.size && !ctl.abort.get()) {
          val n = try {
            tr?.write(s, off, s.size - off, AudioTrack.WRITE_BLOCKING) ?: -1
          } catch (_: Exception) { -1 }
          if (n <= 0) break
          off += n
          frames += n
        }
      }
      // Drain : le dernier chunk continue de jouer — attendre la tête de
      // lecture réelle avant d'émettre tts_done.
      while (!ctl.abort.get()) {
        val head = try {
          (tr?.playbackHeadPosition ?: 0).toLong() and 0xffffffffL
        } catch (_: Exception) { frames }
        if (head >= frames) break
        Thread.sleep(40)
      }
      emitEvent("tts_done", tag)
    } catch (e: Exception) {
      if (!ctl.abort.get()) emitError(tag, e)
    } finally {
      synchronized(this) {
        // N'agir que sur SON track — une lecture plus récente a pu
        // publier le sien (stop → speak immédiat).
        if (track === tr) {
          track?.let {
            try { it.pause(); it.flush(); it.stop() } catch (_: Exception) {}
            it.release()
          }
          track = null
        }
      }
      audioManager.abandonAudioFocus(focusListener)
      // speaking/currentTag ne sont effacés que si ce thread est la
      // lecture courante — le handle a pu être repris par un speak
      // plus récent (compteur/sweep dans le wrapper du thread).
      if (speakThread === Thread.currentThread()) {
        speaking = false
        if (currentTag == tag) currentTag = ""
      }
    }
  }

  @Suppress("DEPRECATION") // requestAudioFocus(listener,…) couvre l'API 24 —
                         // AudioFocusRequest exigerait API 26.
  private fun requestFocus() {
    audioManager.requestAudioFocus(
      focusListener, AudioManager.STREAM_MUSIC,
      AudioManager.AUDIOFOCUS_GAIN_TRANSIENT_MAY_DUCK,
    )
  }

  // ── Android system TTS ───────────────────────────────────────────────

  private fun ensureSysTts(): Boolean {
    // sysLock : deux runSystem concurrents (zombie + courant) ne
    // créent qu'une instance — le perdant n'a plus de fuite à gérer.
    synchronized(sysLock) {
      sysTts?.let { return sysTtsOk == true }
      var created: TextToSpeech? = null
      val latch = CountDownLatch(1)
      val mk = Runnable {
        created = TextToSpeech(activity) { status ->
          sysTtsOk = status == TextToSpeech.SUCCESS
          created?.setOnUtteranceProgressListener(utterListener)
          latch.countDown()
        }
      }
      if (Looper.myLooper() == Looper.getMainLooper()) mk.run()
      else activity.runOnUiThread(mk)
      latch.await(10, TimeUnit.SECONDS)
      sysTts = created
      return sysTtsOk == true && sysTts != null
    }
  }

  private val utterListener = object : UtteranceProgressListener() {
    override fun onStart(id: String) { if (id == currentTag) emitEvent("tts_started", id) }
    override fun onDone(id: String) { if (id == currentTag) finishSystem(id) }
    override fun onStop(id: String, interrupted: Boolean) {
      if (id == currentTag) finishSystem(id)
    }
    @Deprecated("Deprecated in Java")
    override fun onError(id: String) { if (id == currentTag) errorSystem(id) }
  }

  private fun finishSystem(id: String) {
    emitEvent("tts_done", id)
    speaking = false
    if (currentTag == id) currentTag = ""
  }

  private fun errorSystem(id: String) {
    val o = JSObject()
    o.put("tag", id)
    o.put("error", "engine_error")
    emit("tts_error", o)
    speaking = false
    if (currentTag == id) currentTag = ""
  }

  private fun runSystem(
    lang: String, speed: Float, text: String, tag: String, ctl: SpeakCtl,
  ) {
    val t = if (ensureSysTts()) sysTts else null
    if (t == null) { if (currentTag == tag) errorSystem(tag); return }
    if (ctl.abort.get()) return
    if (lang.isNotEmpty()) {
      val avail = t.setLanguage(Locale.forLanguageTag(lang))
      if (avail == TextToSpeech.LANG_MISSING_DATA ||
          avail == TextToSpeech.LANG_NOT_SUPPORTED) {
        if (currentTag == tag) {
          val o = JSObject()
          o.put("tag", tag)
          o.put("error", "lang_unavailable")
          emit("tts_error", o)
          speaking = false
          currentTag = ""
        }
        return
      }
    }
    t.setSpeechRate(speed)
    // Check + enqueue atomiques sous sysLock (stop() y fait son
    // sysTts.stop()) : un stop pendant setLanguage/setSpeechRate ne peut
    // plus laisser passer une utterance fantôme — soit le zombie voit
    // l'abort avant d'enqueuer, soit son utterance est flushée par le
    // stop() qui prend le verrou ensuite.
    synchronized(sysLock) {
      if (ctl.abort.get()) return
      t.speak(text, TextToSpeech.QUEUE_FLUSH, Bundle(), tag)
    }
    // tts_started/done viennent des callbacks utterance.
  }

  // ── Events / helpers ─────────────────────────────────────────────────

  private fun emitEvent(ev: String, tag: String) {
    val o = JSObject()
    o.put("tag", tag)
    emit(ev, o)
  }

  private fun emitError(tag: String, e: Exception) {
    val o = JSObject()
    o.put("tag", tag)
    o.put("error", e.message ?: e.javaClass.simpleName)
    emit("tts_error", o)
  }

  private fun speakResult(started: Boolean, reason: String?): JSObject {
    val ret = JSObject()
    ret.put("started", started)
    if (reason != null) ret.put("reason", reason)
    return ret
  }
}
