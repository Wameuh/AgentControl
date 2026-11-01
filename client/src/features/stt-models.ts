// ---------------------------------------------------------------------------
// Section « Reconnaissance vocale » des settings : liste data-driven des
// modèles STT téléchargeables — les descripteurs viennent du plugin Kotlin
// ac-stt (`stt_dl_state`), aucune donnée modèle n'est codée en dur ici.
// Actions : télécharger / reprendre, annuler, supprimer, re-télécharger.
// Progression live via les events plugin `stt_dl_progress` / `stt_dl_state`.
// Android only : init/refresh no-op partout ailleurs (section masquée).
// ---------------------------------------------------------------------------

import { invoke, listenPlugin, isWeb } from "../core/bridge";
import { isAndroid } from "../core/chrome";
import { $, notify } from "../core/dom";
import { fmtSize } from "../core/format";
import { t, type Key } from "../i18n";

interface SttFileState {
  name: string;
  size: number;
  have: number;
}

type SttModelStatus = "absent" | "partial" | "downloading" | "present" | "error";

interface SttModelState {
  id: string;
  label: string;
  size_bytes: number;
  dir: string;
  state: SttModelStatus;
  downloaded: number;
  error?: string;
  files: SttFileState[];
}

interface SttDlState {
  platform: string;
  models: SttModelState[];
}

const CMD = (c: string) => `plugin:ac-stt|${c}`;

const STATE_KEY: Record<SttModelStatus, Key> = {
  absent: "stt_st_absent",
  partial: "stt_st_partial",
  downloading: "stt_st_downloading",
  present: "stt_st_present",
  error: "stt_st_error",
};

const supported = () => isAndroid() && !isWeb;

export class SttModelsPanel {
  private models: SttModelState[] = [];

  /** Listeners plugin enregistrés une fois au boot — les trigger Kotlin
   *  sont des no-op sans listener, garder le canal vivant est sans coût. */
  init() {
    if (!supported()) return;
    void listenPlugin<{ model: string; bytes: number; total: number }>(
      "ac-stt",
      "stt_dl_progress",
      (p) => this.onProgress(p),
    );
    void listenPlugin<{ model: string; state: string }>(
      "ac-stt",
      "stt_dl_state",
      () => void this.refresh(),
    );
  }

  /** Relecture complète de l'état (ouverture des settings, transition
   *  d'état signalée par `stt_dl_state`). */
  async refresh() {
    if (!supported()) return;
    const st = await invoke<SttDlState>(CMD("stt_dl_state")).catch(
      () => null,
    );
    this.models = st?.models ?? [];
    this.render();
  }

  private onProgress(p: { model: string; bytes: number; total: number }) {
    const m = this.models.find((x) => x.id === p.model);
    if (m) m.downloaded = p.bytes;
    const row = $(`stt-m-${cssId(p.model)}`);
    if (!row) return;
    const fill = row.querySelector<HTMLElement>(".stt-fill");
    if (fill) fill.style.width = `${(100 * p.bytes) / Math.max(1, p.total)}%`;
    const nb = row.querySelector<HTMLElement>(".stt-num");
    if (nb) nb.textContent = `${fmtSize(p.bytes)} / ${fmtSize(p.total)}`;
  }

  /** Action utilisateur → commande plugin, puis refresh de l'état.
   *  Le Kotlin émet en parallèle `stt_dl_state` — le refresh ici rend
   *  l'UI instantanée même si l'event arrive en premier. */
  private async act(m: SttModelState, cmd: string) {
    try {
      await invoke(CMD(cmd), { model: m.id });
      await this.refresh();
    } catch (e) {
      notify("error", String(e));
    }
  }

  private async redownload(m: SttModelState) {
    try {
      await invoke(CMD("stt_dl_delete"), { model: m.id });
      await invoke(CMD("stt_dl_start"), { model: m.id });
      await this.refresh();
    } catch (e) {
      notify("error", String(e));
    }
  }

  private render() {
    const host = $("s-stt-models");
    host.innerHTML = "";
    for (const m of this.models) {
      host.append(this.renderModel(m));
    }
    this.syncSecState();
  }

  private renderModel(m: SttModelState): HTMLElement {
    const row = document.createElement("div");
    row.className = "stt-model";
    row.id = `stt-m-${cssId(m.id)}`;

    const head = document.createElement("div");
    head.className = "s-row stt-head";
    const txt = document.createElement("span");
    txt.className = "s-txt";
    const name = document.createElement("span");
    name.textContent = m.label;
    const meta = document.createElement("small");
    meta.className = "s-desc";
    meta.textContent = `${fmtSize(m.size_bytes)} · ${t(STATE_KEY[m.state])}`;
    if (m.state === "downloading") {
      meta.textContent = `${fmtSize(m.size_bytes)} · `;
      const nb = document.createElement("span");
      nb.className = "stt-num";
      nb.textContent = `${fmtSize(m.downloaded)} / ${fmtSize(m.size_bytes)}`;
      meta.append(nb);
    }
    txt.append(name, meta);
    head.append(txt);

    const actions = document.createElement("span");
    actions.className = "stt-actions";
    const btn = (key: Key, fn: () => void) => {
      const b = document.createElement("button");
      b.type = "button";
      b.className = "s-test";
      b.textContent = t(key);
      b.dataset.i18n = key;
      b.onclick = fn;
      actions.append(b);
    };
    switch (m.state) {
      case "absent":
        btn("stt_download", () => void this.act(m, "stt_dl_start"));
        break;
      case "partial":
        btn("stt_resume", () => void this.act(m, "stt_dl_start"));
        btn("delete", () => void this.act(m, "stt_dl_delete"));
        break;
      case "downloading":
        btn("cancel", () => void this.act(m, "stt_dl_cancel"));
        break;
      case "present":
        btn("stt_redownload", () => void this.redownload(m));
        btn("delete", () => void this.act(m, "stt_dl_delete"));
        break;
      case "error":
        btn("stt_redownload", () => void this.act(m, "stt_dl_start"));
        btn("delete", () => void this.act(m, "stt_dl_delete"));
        break;
    }
    head.append(actions);
    row.append(head);

    if (m.state === "downloading" || m.state === "partial") {
      const bar = document.createElement("div");
      bar.className = "stt-bar";
      const fill = document.createElement("div");
      fill.className = "stt-fill";
      fill.style.width = `${(100 * m.downloaded) / Math.max(1, m.size_bytes)}%`;
      bar.append(fill);
      row.append(bar);
      if (m.state === "partial") {
        const num = document.createElement("small");
        num.className = "s-desc stt-num";
        num.textContent = `${fmtSize(m.downloaded)} / ${fmtSize(m.size_bytes)}`;
        row.append(num);
      }
    }
    if (m.state === "error" && m.error) {
      const err = document.createElement("small");
      err.className = "s-desc stt-err";
      err.textContent = m.error;
      row.append(err);
    }
    return row;
  }

  /** Résumé à droite du titre de section — l'état le plus « actionnable »
   *  des modèles déclarés (error > downloading > partial > absent >
   *  present) reste lisible section repliée. */
  private syncSecState() {
    const el = $("s-st-stt");
    const ms = this.models;
    const dl = ms.find((m) => m.state === "downloading");
    el.textContent = dl
      ? `${Math.round((100 * dl.downloaded) / Math.max(1, dl.size_bytes))}%`
      : ms.some((m) => m.state === "error")
        ? t("stt_st_error")
        : ms.some((m) => m.state === "partial")
          ? t("stt_st_partial")
          : ms.some((m) => m.state === "absent")
            ? t("stt_st_absent")
            : ms.length
              ? t("stt_st_present")
              : "";
  }
}

/** ids de modèle → suffixe d'id DOM sans caractères spéciaux. */
const cssId = (s: string) => s.replace(/[^a-zA-Z0-9_-]/g, "_");
