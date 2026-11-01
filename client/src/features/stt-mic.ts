// ---------------------------------------------------------------------------
// Bouton micro du composer (dictée vocale on-device) : toggle → le plugin
// Kotlin ac-stt stream AudioRecord → OnlineRecognizer ; les partials sont
// injectés en continu dans #input (segment courant remplacé à chaque update,
// segment figé à chaque stt_result/endpoint — jamais d'envoi automatique).
// Android only : sans core STT (commande absente) le bouton reste caché.
//
// Suivi du segment live : {anchor, seg} = position + texte du partial courant
// dans #input. Une frappe clavier pendant l'écoute ne doit pas être écrasée :
// le segment se ré-ancre à la position du curseur, le texte tapé est gardé.
// ---------------------------------------------------------------------------

import { invoke, listenPlugin, isWeb } from "../core/bridge";
import { isAndroid } from "../core/chrome";
import { $, notify } from "../core/dom";
import { icon } from "../ui/icons";
import { t, type Key } from "../i18n";
import { sttEngine, sttLang } from "./stt-prefs";

const CMD = (c: string) => `plugin:ac-stt|${c}`;

interface SttState {
  model_present: boolean;
  recording: boolean;
  permission: string;
}

interface SttStart {
  started: boolean;
  reason?: string;
}

const supported = () => isAndroid() && !isWeb;

export class SttMic {
  private listening = false;
  /** Texte du segment live (dernier partial reçu, "" = segment figé). */
  private seg = "";
  /** Index de début du segment live dans #input.value. */
  private anchor = 0;
  /** Dernier texte figé via stt_result — la réponse de stt_stop répète le
   *  dernier segment : sans ce garde il serait inséré une seconde fois. */
  private lastFinal = "";
  /** Écriture programmatique en cours — purement défensif : assigner
   *  `.value` ne dispatche pas d'event `input`, aucun self-write ne
   *  passe réellement par onUserInput. */
  private selfWrite = false;
  /** Composition IME en cours (autocorrect, glide typing Android) :
   *  ré-écrire `.value` committerait la composition à chaque lettre —
   *  le ré-ancrage est sauté, les partials sont différés au commit. */
  private composing = false;
  /** Events texte différés pendant une composition — file ordonnée : un
   *  endpoint suivi d'un partial dans la même fenêtre ne doit pas perdre
   *  le segment figé (un slot unique l'écraserait silencieusement). */
  private deferredSeg: { text: string; final: boolean }[] = [];
  /** Ré-écrit #input via le composer (value + fit + autocomplete). */
  private writeInput: (v: string) => void = () => {};
  /** Ouvre les settings sur la section modèles STT (injecté par main). */
  openSttSettings: () => void = () => {};

  private inp(): HTMLTextAreaElement {
    return $("input") as HTMLTextAreaElement;
  }

  private btn(): HTMLButtonElement {
    return $("btn-mic") as HTMLButtonElement;
  }

  init(hooks: { setInput: (v: string) => void }) {
    if (!supported()) return;
    this.writeInput = hooks.setInput;
    this.btn().onclick = () => void this.toggle();
    // Les listeners restent vivants en permanence : les trigger Kotlin
    // sont des no-op sans listener, garder le canal ouvert est sans coût.
    void listenPlugin<{ text: string }>("ac-stt", "stt_partial", (p) =>
      this.onPartial(p.text),
    );
    void listenPlugin<{ text: string }>("ac-stt", "stt_result", (p) =>
      this.onResult(p.text),
    );
    void listenPlugin<{ level: number }>("ac-stt", "stt_level", (p) =>
      this.onLevel(p.level),
    );
    // Modèle (re)téléchargé → l'état probe (bouton masqué, hint) change.
    void listenPlugin("ac-stt", "stt_dl_state", () => void this.probe());
    // Retour des réglages système : permission éventuellement accordée.
    document.addEventListener("visibilitychange", () => {
      if (document.visibilityState === "visible") void this.probe();
    });
    // IME : pendant une composition (glide/autocorrect), aucune écriture
    // programmatique — au commit, la frappe est traitée puis la file
    // d'events différés est rejouée dans l'ordre.
    const inp = this.inp();
    inp.addEventListener("compositionstart", () => {
      this.composing = true;
    });
    inp.addEventListener("compositionend", () => {
      this.composing = false;
      this.onUserInput();
      const queue = this.deferredSeg;
      this.deferredSeg = [];
      for (const d of queue) {
        if (!this.listening) break;
        if (d.final) this.commit(d.text);
        else this.spliceSeg(d.text, false);
      }
    });
    void this.probe();
  }

  /** État plugin → visibilité/état du bouton. La commande absente (build
   *  sans core STT) laisse le bouton caché : aucune régression UX. */
  private async probe() {
    const st = await invoke<SttState>(CMD("stt_state")).catch(() => null);
    const b = this.btn();
    if (!st) {
      b.hidden = true;
      return;
    }
    b.hidden = false;
    b.setAttribute("aria-label", b.title || t("stt_mic"));
    if (st.recording && !this.listening) {
      // Dictée encore active côté Kotlin (ex. après un reload webview) —
      // les partials repartent d'un segment neuf en fin de champ.
      this.seg = "";
      this.anchor = this.inp().value.length;
      this.setListening(true);
    } else if (!st.recording && this.listening) {
      // Capture morte côté natif (read cassé, unload, perte de focus
      // audio) : sans ce resync l'anneau pulserait indéfiniment alors
      // qu'aucun event n'arrive plus.
      this.seg = "";
      this.setListening(false);
    } else {
      b.setAttribute("aria-pressed", String(this.listening));
    }
    this.setDenied(st.permission === "denied" && !st.recording);
  }

  // ---- Toggle ----

  private async toggle() {
    if (this.listening) {
      await this.stop();
      return;
    }
    const denied = this.btn().classList.contains("stt-denied");
    if (denied) {
      this.showHint("stt_hint_denied", false);
      void this.probe(); // permission accordée entre-temps → bouton libéré
      return;
    }
    // Prefs par dictée (spec : langue forcée par stream) — le Kotlin
    // perd ses volatiles au reload webview ; best-effort : un plugin
    // plus vieux que stt_set_engine ne doit pas bloquer la dictée.
    await Promise.all([
      invoke(CMD("stt_set_engine"), { engine: sttEngine() }).catch(() => {}),
      invoke(CMD("stt_set_language"), { lang: sttLang() }).catch(() => {}),
    ]);
    const r = await invoke<SttStart>(CMD("stt_start")).catch((e) => {
      notify("error", String(e));
      return null;
    });
    if (!r) return;
    if (r.started) {
      this.beginSegment();
      this.setListening(true);
      $("mic-hint").hidden = true;
      return;
    }
    switch (r.reason) {
      case "model_missing":
        // Hint + lien direct vers la section de téléchargement (spec).
        this.showHint("stt_hint_model", true);
        break;
      case "mic_denied":
        this.setDenied(true);
        this.showHint("stt_hint_denied", false);
        break;
      case "perm_pending":
        // Dialogue système de permission déjà affiché (double-tap) —
        // laisser l'utilisateur répondre, aucun message.
        break;
      default:
        notify("error", t("stt_err"));
    }
  }

  /** Début de dictée : le segment live s'ancre au curseur (une sélection
   *  active est remplacée — convention des dictées). */
  private beginSegment() {
    const inp = this.inp();
    const s = inp.selectionStart ?? inp.value.length;
    const e = inp.selectionEnd ?? s;
    this.seg = "";
    this.anchor = s;
    this.lastFinal = "";
    this.deferredSeg = []; // résidu d'une dictée précédente non rejoué
    if (e > s) this.write(inp.value.slice(0, s) + inp.value.slice(e), s);
  }

  private async stop() {
    const r = await invoke<{ text: string }>(CMD("stt_stop")).catch((e) => {
      notify("error", String(e));
      return null;
    });
    this.setListening(false);
    // Drainer la file différée : un endpoint non rejoué n'est PAS couvert
    // par la réponse (dernier segment seulement) — le replay dans l'ordre
    // fige tout ce qui est resté en l'air, la dédup lastFinal gère la
    // suite. Le compositionend ultérieur ne rejoue rien (listening faux
    // + file vidée).
    for (const d of this.deferredSeg) {
      if (d.final) this.commit(d.text);
      else this.spliceSeg(d.text, false);
    }
    this.deferredSeg = [];
    // La réponse répète le dernier endpoint déjà émis en stt_result → ne
    // l'insérer que si ce n'est pas un doublon (partial resté en l'air).
    const fin = r?.text ?? "";
    if (fin && fin !== this.lastFinal) this.commit(fin);
    this.seg = "";
  }

  private setListening(on: boolean) {
    this.listening = on;
    if (on) this.setDenied(false); // start réussi = permission accordée
    const b = this.btn();
    b.classList.toggle("stt-on", on);
    b.style.removeProperty("--stt-lvl");
    const key: Key = on ? "stt_listening" : "stt_mic";
    b.dataset.i18nTitle = key;
    b.title = t(key);
    b.setAttribute("aria-label", b.title);
    b.setAttribute("aria-pressed", String(on));
    // Icône carré pendant l'écoute : l'état est lisible sans la couleur
    // seule (icône + anneau animé — exigence accessibilité).
    b.innerHTML = icon(on ? "stop" : "mic");
  }

  private setDenied(denied: boolean) {
    const b = this.btn();
    b.classList.toggle("stt-denied", denied);
    b.setAttribute("aria-disabled", String(denied));
    if (!denied && $("mic-hint-denied")) $("mic-hint").hidden = true;
  }

  // ---- Injection live dans #input ----

  /** stt_partial : le segment courant est REMPLACÉ (pas concaténé). */
  private onPartial(text: string) {
    if (!this.listening) return;
    if (this.composing) {
      // Partials consécutifs non figés coalescés au push : chacun remplace
      // le segment — seul le dernier compte (borne la file si le
      // compositionend ne venait jamais).
      const last = this.deferredSeg[this.deferredSeg.length - 1];
      if (last && !last.final) last.text = text;
      else this.deferredSeg.push({ text, final: false });
      return;
    }
    this.spliceSeg(text, false);
  }

  /** stt_result (endpoint) : segment figé + espace de jointure ; le
   *  prochain partial ouvrira un segment neuf juste après. */
  private onResult(text: string) {
    if (!this.listening) return;
    if (this.composing) {
      this.deferredSeg.push({ text, final: true });
      return;
    }
    this.commit(text);
  }

  private commit(text: string) {
    this.lastFinal = text;
    const fin = text && !/\s$/.test(text) ? `${text} ` : text;
    this.spliceSeg(fin, true);
  }

  /** Remplace le segment live par `text` ; `final` fige le segment
   *  (anchor avance après le texte — next partial = nouveau segment). */
  private spliceSeg(text: string, final: boolean) {
    const inp = this.inp();
    const v = inp.value;
    const a = this.locate(v);
    const next = v.slice(0, a) + text + v.slice(a + this.seg.length);
    const end = a + text.length;
    this.anchor = final ? end : a;
    this.seg = final ? "" : text;
    this.write(next, end);
  }

  /** Position actuelle du segment live : à l'ancre si intact, sinon la
   *  occurrence la plus proche (frappe avant le segment l'a décalé) ;
   *  disparu (champ vidé, session changée) → repart au curseur. */
  private locate(v: string): number {
    let a = Math.min(this.anchor, v.length);
    if (!this.seg) return a;
    if (v.slice(a, a + this.seg.length) === this.seg) return a;
    let best = -1;
    for (let p = v.indexOf(this.seg); p >= 0; p = v.indexOf(this.seg, p + 1)) {
      if (best < 0 || Math.abs(p - a) < Math.abs(best - a)) best = p;
    }
    if (best >= 0) return best;
    this.seg = "";
    return Math.min(this.inp().selectionStart ?? v.length, v.length);
  }

  /** Frappe clavier pendant l'écoute (event `input` hors selfWrite) :
   *  ne pas écraser la saisie — le segment se ré-ancre au curseur,
   *  le texte tapé est gardé (spec §7.4). Pas de ré-écriture pendant
   *  `isComposing` : ça committerait la composition IME à chaque lettre
   *  (glide typing fragmenté, autocorrect perdu) — le ré-ancrage attend
   *  le `compositionend`. */
  onUserInput(e?: InputEvent) {
    const inp = this.inp();
    if (this.selfWrite || !this.listening || e?.isComposing) return;
    const v = inp.value;
    const cur = inp.selectionStart ?? v.length;
    if (this.seg) {
      const a = this.locate(v);
      if (a >= 0 && v.slice(a, a + this.seg.length) === this.seg) {
        // Segment intact quelque part → retirer puis ré-insérer au
        // curseur (la frappe de l'utilisateur reste là où il l'a mise).
        const base = v.slice(0, a) + v.slice(a + this.seg.length);
        const na = Math.max(
          0,
          Math.min(cur - (cur > a + this.seg.length ? this.seg.length : 0), base.length),
        );
        this.anchor = na;
        this.write(base.slice(0, na) + this.seg + base.slice(na), na + this.seg.length);
        return;
      }
    }
    // Segment altéré/supprimé par la frappe : garder le document tel quel
    // — le prochain partial repart d'un segment neuf au curseur.
    this.seg = "";
    this.anchor = Math.min(cur, v.length);
  }

  /** Écriture dans #input via le hook composer (fit + refreshAc) ; le
   *  flag évite que notre propre event `input` soit pris pour une frappe. */
  private write(next: string, cursor: number) {
    this.selfWrite = true;
    this.writeInput(next);
    this.inp().setSelectionRange(cursor, cursor);
    this.selfWrite = false;
  }

  private onLevel(level: number) {
    if (!this.listening) return;
    this.btn().style.setProperty("--stt-lvl", level.toFixed(3));
  }

  // ---- Hint composer (modèle absent, micro refusé) ----

  /** Bandeau inline du composer : message + lien optionnel vers les
   *  settings + ✕. Persistant jusqu'à résolution ou fermeture — les
   *  toasts disparaissent trop vite pour un état bloquant. */
  private showHint(msgKey: Key, settingsLink: boolean) {
    const h = $("mic-hint");
    h.innerHTML = "";
    const msg = document.createElement("span");
    msg.textContent = t(msgKey);
    if (msgKey === "stt_hint_denied") msg.id = "mic-hint-denied";
    h.append(msg);
    if (settingsLink) {
      const a = document.createElement("button");
      a.type = "button";
      a.className = "mh-link";
      a.textContent = t("stt_open_settings");
      a.onclick = () => {
        h.hidden = true;
        this.openSttSettings();
      };
      h.append(a);
    }
    const x = document.createElement("button");
    x.type = "button";
    x.className = "mh-x";
    x.textContent = "✕";
    x.setAttribute("aria-label", t("close"));
    x.onclick = () => (h.hidden = true);
    h.append(x);
    h.hidden = false;
  }
}
