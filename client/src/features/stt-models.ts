// ---------------------------------------------------------------------------
// Section « Reconnaissance vocale » des settings : selects moteur/langue
// persistés en localStorage + liste des modèles STT téléchargeables
// (DlModelsPanel — descripteurs `stt_dl_state` du plugin Kotlin ac-stt).
// Android only : init/refresh no-op partout ailleurs (section masquée).
// ---------------------------------------------------------------------------

import { invoke, isWeb } from "../core/bridge";
import { isAndroid } from "../core/chrome";
import { $ } from "../core/dom";
import { DlModelsPanel } from "./dl-models";
import {
  sttEngine,
  sttLang,
  setSttEngine,
  setSttLang,
  type SttLang,
} from "./stt-prefs";
import type { Stager } from "./settings";

const CMD = (c: string) => `plugin:ac-stt|${c}`;

const supported = () => isAndroid() && !isWeb;

/** `stt_state` — champs utiles au panneau (le mic garde sa propre vue). */
interface SttEngineState {
  engine: string;
  language: string;
  system_available: boolean;
}

export class SttModelsPanel {
  /** Liste data-driven des modèles — partagée avec la section TTS. */
  private dl = new DlModelsPanel({
    kind: "stt",
    hostId: "s-stt-models",
    secStateId: "s-st-stt",
  });

  init(stage: Stager) {
    if (!supported()) return;
    this.dl.init();
    // Selects moteur/langue : le localStorage fait foi (le Kotlin perd
    // son état volatile au reload webview). Stagé (#190) : le changement
    // est poussé au plugin au clic « Enregistrer » — de toute façon les
    // prefs sont ré-appliquées avant chaque dictée par le mic.
    const engSel = $("s-stt-engine") as HTMLSelectElement;
    const langSel = $("s-stt-lang") as HTMLSelectElement;
    engSel.value = sttEngine();
    langSel.value = sttLang();
    stage("s-stt-engine", {
      apply: () => {
        const e = engSel.value === "system" ? "system" : "sherpa";
        setSttEngine(e);
        void invoke(CMD("stt_set_engine"), { engine: e }).catch(() => {});
      },
      reset: () => {
        engSel.value = sttEngine();
      },
    });
    stage("s-stt-lang", {
      apply: () => {
        const l = langSel.value as SttLang;
        setSttLang(l);
        void invoke(CMD("stt_set_language"), { lang: l }).catch(() => {});
      },
      reset: () => {
        langSel.value = sttLang();
      },
    });
  }

  /** Relecture complète de l'état (ouverture des settings). */
  async refresh() {
    if (!supported()) return;
    await this.dl.refresh();
    void this.syncEngine();
  }

  /** Le SpeechRecognizer on-device (API 31+, appareil dépendant) gate
   *  l'option « system » : absente → option grisée (elle reste lisible —
   *  la raison est dans le desc, pas cachée). Les valeurs affichées sont
   *  gérées par init/reset du staging (#190) — ne pas les réécrire ici,
   *  un refresh asynchrone écraserait une édition non enregistrée. */
  private async syncEngine() {
    const st = await invoke<SttEngineState>(CMD("stt_state")).catch(
      () => null,
    );
    const engSel = $("s-stt-engine") as HTMLSelectElement;
    if (!st) return;
    const sysOpt = engSel.querySelector<HTMLOptionElement>(
      'option[value="system"]',
    );
    if (sysOpt) sysOpt.disabled = !st.system_available;
  }
}
