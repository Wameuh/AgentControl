// ---------------------------------------------------------------------------
// Lecture vocale des messages (TTS on-device, backlog #138) : bouton
// haut-parleur sur chaque message agent, lecture auto optionnelle (off
// par défaut), une utterance à la fois — file FIFO bornée côté client,
// préemption côté plugin. Moteurs : sherpa OfflineTts (modèle téléchargé
// via la section « Synthèse vocale ») ou TextToSpeech système Android ;
// choix "auto" = sherpa si un modèle est installé, système sinon.
// Android only : no-op sur desktop/web.
// ---------------------------------------------------------------------------

import { invoke, listenPlugin, isWeb } from "../core/bridge";
import { isAndroid } from "../core/chrome";
import { $, notify } from "../core/dom";
import { speechText } from "../core/format";
import type { AcEvent } from "../core/types";
import { t } from "../i18n";
import { icon } from "../ui/icons";

const CMD = (c: string) => `plugin:ac-stt|${c}`;

interface TtsVoice {
  id: string;
  label: string;
  model: string;
  lang: string;
}

interface TtsState {
  platform: string;
  sherpa_ready: boolean;
  system_ready: boolean;
  speaking: boolean;
  voices: TtsVoice[];
}

interface TtsSpeak {
  started: boolean;
  reason?: string;
  engine?: string;
}

export type TtsEngine = "auto" | "sherpa" | "system";

// Préférences persistées (localStorage) — même motif que stt-prefs :
// la webview peut perdre son état au reload, le fichier fait foi.
export const ttsEnginePref = (): TtsEngine => {
  const v = localStorage.getItem("ac-tts-engine");
  return v === "sherpa" || v === "system" ? v : "auto";
};
export const ttsAutoread = () =>
  localStorage.getItem("ac-tts-autoread") === "1";
const ttsVoicePref = () => localStorage.getItem("ac-tts-voice") ?? "";
const ttsSpeedPref = () =>
  Number(localStorage.getItem("ac-tts-speed") || "1") || 1;

const supported = () => isAndroid() && !isWeb;

/** File d'attente de la lecture auto — bornée : un débordement éjecte
 *  les plus anciens (la voix rattrape le présent, pas l'historique). */
const AUTO_QUEUE_MAX = 3;

export class Tts {
  private voices: TtsVoice[] = [];
  /** sherpa ou système utilisable — gate de la lecture auto. */
  private available = false;
  /** Utterance en cours : tag + bouton DOM à resynchroniser. */
  private playing: { tag: string; btn: HTMLButtonElement | null } | null =
    null;
  private queue: { tag: string; text: string }[] = [];
  /** « modèle absent » → ouvre les settings TTS (injecté par le bootstrap). */
  openSettings: () => void = () => {};

  init() {
    if (!supported()) return;
    void listenPlugin<{ tag: string }>(
      "ac-stt",
      "tts_started",
      (p) => this.onStarted(p.tag),
    );
    void listenPlugin<{ tag: string }>("ac-stt", "tts_done", (p) =>
      this.onEnded(p.tag),
    );
    void listenPlugin<{ tag: string; error?: string }>(
      "ac-stt",
      "tts_error",
      (p) => this.onErr(p.tag, p.error),
    );
    // Modèle installé/supprimé → voix dispo + gate de la lecture auto.
    void listenPlugin<{ model: string; state: string }>(
      "ac-stt",
      "tts_dl_state",
      () => void this.refreshState(),
    );
    void this.refreshState();
  }

  // ── État moteur / voix ────────────────────────────────────────────

  /** tts_state : voix des modèles installés + dispo des moteurs. */
  async refreshState() {
    if (!supported()) return;
    const st = await invoke<TtsState>(CMD("tts_state")).catch(() => null);
    if (!st) return;
    this.voices = st.voices;
    this.available = st.sherpa_ready || st.system_ready;
    this.syncVoiceSelect();
  }

  // ── Bouton par message agent ──────────────────────────────────────

  /** Haut-parleur attaché par le renderer à chaque agent_text — live
   *  comme historique (relire un vieux message est voulu). */
  attachMsg(el: HTMLElement, ev: AcEvent) {
    if (!supported()) return;
    const md = ev.text as string;
    if (!speechText(md)) return;
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "msg-tts";
    const tag = `ev-${ev.seq}`;
    this.renderBtn(btn, this.playing?.tag === tag);
    btn.onclick = () => void this.toggle(tag, md, btn);
    el.appendChild(btn);
  }

  private renderBtn(btn: HTMLButtonElement, on: boolean) {
    btn.innerHTML = icon(on ? "stop" : "speaker");
    btn.title = t(on ? "tts_stop" : "tts_speak");
    btn.classList.toggle("tts-on", on);
  }

  /** Clic haut-parleur : toggle (re-clic = stop) ou lecture. Une
   *  action manuelle purge la file auto — l'utilisateur reprend la
   *  main, la lecture auto repartira sur les prochains messages. */
  private async toggle(tag: string, md: string, btn: HTMLButtonElement) {
    if (this.playing?.tag === tag) {
      this.stop();
      return;
    }
    this.queue = [];
    const text = speechText(md);
    if (!text) return;
    await this.speakNow(tag, text, btn);
  }

  private async speakNow(
    tag: string,
    text: string,
    btn: HTMLButtonElement | null,
  ) {
    const voice = ttsVoicePref();
    // Le modèle de la voix préférée : sans lui resolveModel("") prend le
    // premier installé (kokoro) et la voix vits choisie serait ignorée.
    const model = this.voices.find((v) => v.id === voice)?.model ?? "";
    const r = await invoke<TtsSpeak>(CMD("tts_speak"), {
      text,
      tag,
      engine: ttsEnginePref(),
      model,
      voice,
      lang: "",
      speed: ttsSpeedPref(),
    }).catch((e) => ({ started: false, reason: String(e) }) as TtsSpeak);
    if (!r.started) {
      if (r.reason === "model_missing") this.openSettings();
      else
        notify(
          "error",
          `${t("tts_err")}${r.reason ? ` — ${r.reason}` : ""}`,
        );
      return;
    }
    this.playing?.btn?.isConnected && this.renderBtn(this.playing.btn, false);
    this.playing = { tag, btn };
    if (btn) this.renderBtn(btn, true);
  }

  stop() {
    this.queue = [];
    if (this.playing) {
      const b = this.playing.btn;
      if (b?.isConnected) this.renderBtn(b, false);
      this.playing = null;
    }
    void invoke(CMD("tts_stop")).catch(() => {});
  }

  /** Changement de session affichée / navigation : coupe la lecture en
   *  cours — une voix qui commente un journal qu'on a quitté est un
   *  bug UX, pas une feature. */
  sessionChanged() {
    this.stop();
  }

  // ── Lecture auto ─────────────────────────────────────────────────

  /** agent_text COMPLET rendu en live (l'appelant filtre live+queue —
   *  jamais de replay). Lecture auto on : en cours → file FIFO bornée ;
   *  idle → direct. */
  onAgentText(ev: AcEvent) {
    if (!ttsAutoread() || !this.available) return;
    const text = speechText(ev.text as string);
    if (!text) return;
    const tag = `ev-${ev.seq}`;
    if (this.playing) {
      this.queue.push({ tag, text });
      if (this.queue.length > AUTO_QUEUE_MAX) this.queue.shift();
      return;
    }
    void this.speakNow(tag, text, null);
  }

  // ── Events plugin ────────────────────────────────────────────────

  private onStarted(_tag: string) {
    // Le bouton est déjà marqué au speak — rien à faire : l'event sert
    // surtout à garder le plugin et le client cohérents en cas de
    // préemption native (nouveau speak → le tag courant change).
  }

  private onEnded(tag: string) {
    // Le Kotlin émet tts_done même pour une utterance abortée : ne
    // dépiler QUE si c'était la lecture courante — sinon un done
    // tardif d'un tag périmé préempterait la lecture en cours.
    if (this.clearIf(tag)) this.dequeue();
  }

  private onErr(tag: string, err?: string) {
    const was = this.clearIf(tag);
    if (was) this.dequeue();
    if (was || !err?.includes("model_missing"))
      notify("error", `${t("tts_err")}${err ? ` — ${err}` : ""}`);
  }

  private clearIf(tag: string): boolean {
    if (this.playing?.tag !== tag) return false;
    const b = this.playing.btn;
    if (b?.isConnected) this.renderBtn(b, false);
    this.playing = null;
    return true;
  }

  private dequeue() {
    const next = this.queue.shift();
    if (next) void this.speakNow(next.tag, next.text, null);
  }

  // ── Settings « Synthèse vocale » ─────────────────────────────────

  /** Selects engine/voice/speed + lecture auto + aperçu — bind une fois
   *  au boot (les éléments existent toujours dans le markup, la section
   *  est simplement cachée hors Android). */
  bindSettings() {
    if (!supported()) return;
    const engSel = $("s-tts-engine") as HTMLSelectElement;
    const voiceSel = $("s-tts-voice") as HTMLSelectElement;
    const speedSel = $("s-tts-speed") as HTMLSelectElement;
    const autoChk = $("s-tts-autoread") as HTMLInputElement;
    engSel.value = ttsEnginePref();
    speedSel.value = String(ttsSpeedPref());
    autoChk.checked = ttsAutoread();
    engSel.onchange = () =>
      localStorage.setItem("ac-tts-engine", engSel.value);
    voiceSel.onchange = () =>
      localStorage.setItem("ac-tts-voice", voiceSel.value);
    speedSel.onchange = () =>
      localStorage.setItem("ac-tts-speed", speedSel.value);
    autoChk.onchange = () =>
      localStorage.setItem("ac-tts-autoread", autoChk.checked ? "1" : "0");
    ($("s-tts-preview") as HTMLButtonElement).onclick = () => {
      this.queue = [];
      void this.speakNow("preview", t("tts_preview_text"), null);
    };
    this.syncVoiceSelect();
  }

  /** Re-sync à l'ouverture des settings (valeurs persistées + voix). */
  syncSettings() {
    if (!supported()) return;
    ($("s-tts-engine") as HTMLSelectElement).value = ttsEnginePref();
    ($("s-tts-speed") as HTMLSelectElement).value = String(ttsSpeedPref());
    ($("s-tts-autoread") as HTMLInputElement).checked = ttsAutoread();
    this.syncVoiceSelect();
    void this.refreshState();
  }

  /** Options du select voix = catalogue `tts_state` (data-driven — les
   *  modèles installés offrent leurs voix ; « auto » = défaut du modèle).
   *  La préférence est conservée si la voix reste proposée. */
  private syncVoiceSelect() {
    const sel = $("s-tts-voice") as HTMLSelectElement | null;
    if (!sel) return;
    const cur = ttsVoicePref();
    sel.innerHTML = "";
    const auto = document.createElement("option");
    auto.value = "";
    auto.dataset.i18n = "tts_voice_auto";
    auto.textContent = t("tts_voice_auto");
    sel.append(auto);
    for (const v of this.voices) {
      const o = document.createElement("option");
      o.value = v.id;
      o.textContent = v.label;
      sel.append(o);
    }
    sel.value = this.voices.some((v) => v.id === cur) ? cur : "";
  }
}
