// ---------------------------------------------------------------------------
// Vue pleine page des paramètres d'un composant (« Configurer », panneau
// Composants) — remplace le dialogue de 55vh pour les gros schémas comme
// l'orchestrateur : un onglet par `group` du schéma, filtre texte,
// réglages avancés masqués par défaut, retour au défaut par champ et
// récapitulatif des changements (ancienne → nouvelle valeur) avant
// « Appliquer ». Seules les clés modifiées partent dans
// `component_set_params` (patch partiel, le démon fusionne et valide).
// ---------------------------------------------------------------------------

import { $, esc, notify } from "../core/dom";
import { call } from "../core/rpc";
import { askUnsaved } from "../core/unsaved";
import { getLang, t, type Key } from "../i18n";
import { icon } from "../ui/icons";
import type { ComponentItem, ParamSpec } from "../core/types";
import { orchParamText } from "./orch-params-i18n";
import {
  ParamField,
  paramBool,
  secLabel,
  validateParam,
  type PoolFactory,
} from "./params-form";

/** Composant interne de l'orchestrateur — libellés traduits côté client. */
export const ORCH_COMPONENT = "backlog-orchestrator";

const ADV_KINDS = new Set(["json", "path", "list"]);

/** Changements qui interrompent du travail en cours : signalés dans le
 *  récapitulatif avant application (orchestrateur uniquement). */
const WARNS: { key: string; when: (from: string, to: string) => boolean; msg: Key }[] = [
  { key: "pr_review", when: (a, b) => a === "true" && b === "false", msg: "pv_warn_pr_review" },
  { key: "plan_review", when: (a, b) => a === "true" && b === "false", msg: "pv_warn_plan_review" },
  { key: "enabled", when: (a, b) => a === "true" && b === "false", msg: "pv_warn_enabled" },
  { key: "title_prefix", when: (a, b) => a !== b, msg: "pv_warn_prefix" },
];

export interface ParamsViewDeps {
  connParam: () => string | undefined;
  /** Widget équipe (`worker_pool`) fourni par le panneau Composants. */
  pool: PoolFactory;
  /** Caps des profils pour le widget équipe (une fois par ouverture). */
  loadPool: () => Promise<void>;
  /** Ligne d'état du composant (pid, uptime, santé). */
  statusLine: (it: ComponentItem) => string;
  /** Après application : item rafraîchi (nouvelle base de comparaison). */
  reload: (name: string) => Promise<ComponentItem | null>;
  /** Retour à la liste des composants. */
  back: () => void;
}

interface Row {
  f: ParamField;
  el: HTMLElement;
  label: string;
  adv: boolean;
  group: string;
  hay: string;
}

export class ParamsView {
  private item: ComponentItem | null = null;
  private rows: Row[] = [];
  private tab = "";
  private adv = false;
  private busy = false;

  constructor(private deps: ParamsViewDeps) {}

  private host() {
    return $("comp-params");
  }

  /** Des changements attendent « Appliquer ». */
  dirty(): boolean {
    return this.rows.some((r) => r.f.changed);
  }

  /** Quitter la vue : avec des changements, Appliquer / Ignorer / Rester.
   *  Résout `true` si l'on peut partir. */
  async confirmLeave(): Promise<boolean> {
    if (!this.item || !this.dirty()) return true;
    const c = await askUnsaved(
      t("unsaved_params_msg", { name: this.item.name }),
      t("apply"),
    );
    if (c === "stay") return false;
    if (c === "save") return await this.apply();
    return true;
  }

  async open(it: ComponentItem, keepUi = false) {
    const schema = it.spec.params_schema ?? [];
    if (!keepUi) {
      this.tab = "";
      this.adv = false;
    }
    this.item = it;
    if (schema.some((p) => p.key === "worker_pool")) await this.deps.loadPool();
    this.render(schema, it.spec.params ?? {}, keepUi);
  }

  private text(ps: ParamSpec): { label: string; desc: string; adv: boolean } {
    const o = this.item?.name === ORCH_COMPONENT ? orchParamText(ps.key) : undefined;
    const tr = o?.t[getLang()];
    const adv =
      o?.adv === true ||
      (!o && ((ADV_KINDS.has(ps.kind) && ps.key !== "worker_pool") || /(^|_)prompt$/.test(ps.key)));
    return {
      label: tr?.[0] ?? ps.label,
      desc: tr?.[1] ?? ps.description ?? "",
      adv,
    };
  }

  private render(schema: ParamSpec[], current: Record<string, string>, keepUi: boolean) {
    const it = this.item!;
    const host = this.host();
    const prevFilter = keepUi
      ? (host.querySelector<HTMLInputElement>(".pv-filter input")?.value ?? "")
      : "";
    host.innerHTML = "";
    this.rows = [];
    const groups = [...new Set(schema.map((p) => p.group ?? ""))];
    if (!groups.includes(this.tab)) this.tab = groups[0] ?? "";

    // ---- En-tête ----
    const head = document.createElement("div");
    head.className = "pv-head";
    const st = it.status.state;
    head.innerHTML =
      `<button type="button" class="pv-back" title="${esc(t("comp_back"))}" aria-label="${esc(t("comp_back"))}">${icon("back")}</button>` +
      `<h4 class="pv-title">${esc(it.name === ORCH_COMPONENT ? t("orch_h") : it.name)}</h4>` +
      `<span class="pv-pill"><span class="pv-dot ${esc(st)}"></span>${esc(this.deps.statusLine(it))}</span>` +
      `<span class="pv-sp"></span>` +
      `<label class="pv-filter">${icon("search")}<input type="search" autocomplete="off" spellcheck="false" placeholder="${esc(t("pv_filter_ph", { n: schema.length }))}"></label>` +
      `<label class="pv-advtgl"><input type="checkbox" class="s-sw"${this.adv ? " checked" : ""}><span>${esc(t("pv_advanced"))}</span><span class="pv-advn"></span></label>`;
    host.appendChild(head);
    head.querySelector<HTMLButtonElement>(".pv-back")!.onclick = () => void this.leave();
    const filter = head.querySelector<HTMLInputElement>(".pv-filter input")!;
    filter.value = prevFilter;
    filter.addEventListener("input", () => this.applyFilter());
    filter.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && filter.value) {
        e.stopPropagation();
        filter.value = "";
        this.applyFilter();
      }
    });
    const advIn = head.querySelector<HTMLInputElement>(".pv-advtgl input")!;
    advIn.onchange = () => {
      this.adv = advIn.checked;
      this.applyFilter();
    };

    // ---- Onglets ----
    const tabs = document.createElement("div");
    tabs.className = "pv-tabs";
    tabs.setAttribute("role", "tablist");
    tabs.hidden = groups.length < 2;
    host.appendChild(tabs);

    // ---- Corps : formulaire + récapitulatif ----
    const body = document.createElement("div");
    body.className = "pv-body";
    const form = document.createElement("div");
    form.className = "pv-form";
    const aside = document.createElement("aside");
    aside.className = "pv-diff";
    aside.innerHTML =
      `<h5>${esc(t("pv_changes"))} <span class="pv-count"></span></h5>` +
      `<ul class="pv-list"></ul>` +
      `<ul class="pv-warns"></ul>` +
      `<p class="pv-note">${esc(t(it.name === ORCH_COMPONENT ? "pv_note_orch" : "pv_note_proc"))}</p>` +
      `<div class="pv-err dlg-err" role="alert" hidden></div>` +
      `<div class="pv-acts"><button type="button" class="pv-reset">${esc(t("pv_reset_all"))}</button>` +
      `<button type="button" class="pv-apply primary">${esc(t("apply"))}</button></div>`;
    body.append(form, aside);
    host.appendChild(body);
    aside.querySelector<HTMLButtonElement>(".pv-reset")!.onclick = () => {
      for (const r of this.rows) if (r.f.changed) r.f.write(r.f.initial);
    };
    aside.querySelector<HTMLButtonElement>(".pv-apply")!.onclick = () => void this.apply();

    for (const g of groups) {
      const pane = document.createElement("section");
      pane.className = "pv-pane";
      pane.dataset.group = g;
      if (g) {
        const h = document.createElement("h5");
        h.className = "pv-gh";
        h.textContent = secLabel(g);
        pane.appendChild(h);
      }
      form.appendChild(pane);
      const tb = document.createElement("button");
      tb.type = "button";
      tb.setAttribute("role", "tab");
      tb.dataset.group = g;
      tb.innerHTML = `<span>${esc(g ? secLabel(g) : t("cp_sec_general"))}</span><span class="pv-tc"></span>`;
      tb.onclick = () => {
        this.tab = g;
        filter.value = "";
        this.applyFilter();
      };
      tabs.appendChild(tb);
    }

    const choiceLabel = (key: string) => (v: string) => {
      const o = it.name === ORCH_COMPONENT ? orchParamText(key) : undefined;
      return o?.choices?.[v]?.[getLang()];
    };
    for (const ps of schema) {
      const f = new ParamField(ps, current, {
        pool: this.deps.pool,
        choiceLabel: choiceLabel(ps.key),
        boolClass: "s-sw",
      });
      const tx = this.text(ps);
      const wide =
        ps.kind === "json" || f.control.tagName === "TEXTAREA" || ps.key === "worker_pool";
      const row = document.createElement("div");
      row.className = `pv-row${wide ? " pv-wide" : ""}${ps.kind === "int" ? " pv-int" : ""}`;
      row.dataset.key = ps.key;
      const txt = document.createElement("div");
      txt.className = "pv-txt";
      const full = ps.description && ps.description !== tx.desc ? ps.description : "";
      txt.innerHTML =
        `<span class="pv-l">${esc(tx.label)}${ps.required ? " *" : ""}<span class="pv-mod" hidden></span></span>` +
        (tx.desc ? `<small class="pv-d">${esc(tx.desc)}</small>` : "") +
        `<span class="pv-k"><code>${esc(ps.key)}</code>` +
        (full ? `<span class="pv-info" tabindex="0" title="${esc(full)}" aria-label="${esc(full)}">${icon("info")}</span>` : "") +
        `</span>`;
      const ctl = document.createElement("div");
      ctl.className = "pv-ctl";
      ctl.appendChild(f.control);
      const rst = document.createElement("button");
      rst.type = "button";
      rst.className = "pv-rst";
      rst.title = t("pv_default");
      rst.setAttribute("aria-label", t("pv_default"));
      rst.innerHTML = icon("undo");
      rst.onclick = () => f.write(f.defaultValue);
      ctl.appendChild(rst);
      row.append(txt, ctl);
      const g = ps.group ?? "";
      form.querySelector<HTMLElement>(`.pv-pane[data-group="${CSS.escape(g)}"]`)!.appendChild(row);
      const r: Row = {
        f, el: row, label: tx.label, adv: tx.adv, group: g,
        hay: `${tx.label} ${tx.desc} ${ps.key} ${ps.label}`.toLowerCase(),
      };
      this.rows.push(r);
      f.onInput(() => this.syncRow(r));
      this.syncRow(r, false);
    }
    this.applyFilter();
    this.syncDiff();
  }

  /** Pastille « modifié » + bouton retour au défaut d'une ligne. */
  private syncRow(r: Row, diff = true) {
    const ps = r.f.ps;
    r.el.querySelector<HTMLElement>(".pv-mod")!.hidden = !r.f.changed;
    const rst = r.el.querySelector<HTMLButtonElement>(".pv-rst")!;
    rst.hidden = ps.secret === true || ps.default == null || r.f.read() === r.f.defaultValue;
    if (diff) this.syncDiff();
  }

  private applyFilter() {
    const host = this.host();
    const q = host.querySelector<HTMLInputElement>(".pv-filter input")!.value.trim().toLowerCase();
    host.classList.toggle("pv-filtering", !!q);
    const counts = new Map<string, number>();
    for (const r of this.rows) {
      const show = q
        ? r.hay.includes(q)
        : r.group === this.tab && (this.adv || !r.adv || r.f.changed);
      r.el.hidden = !show;
      if (!r.adv || this.adv) counts.set(r.group, (counts.get(r.group) ?? 0) + 1);
    }
    for (const p of host.querySelectorAll<HTMLElement>(".pv-pane")) {
      const g = p.dataset.group ?? "";
      p.hidden = q
        ? !this.rows.some((r) => r.group === g && !r.el.hidden)
        : g !== this.tab;
    }
    for (const b of host.querySelectorAll<HTMLButtonElement>(".pv-tabs button")) {
      const g = b.dataset.group ?? "";
      b.setAttribute("aria-selected", !q && g === this.tab ? "true" : "false");
      b.querySelector(".pv-tc")!.textContent = String(counts.get(g) ?? 0);
    }
    // Réglages avancés personnalisés (≠ défaut) masqués : le dire.
    const hiddenCustom = this.adv
      ? 0
      : this.rows.filter((r) => r.adv && r.f.initial !== r.f.defaultValue && !r.f.ps.secret).length;
    const advn = host.querySelector<HTMLElement>(".pv-advn")!;
    advn.textContent = hiddenCustom ? String(hiddenCustom) : "";
    advn.title = hiddenCustom ? t("pv_adv_custom_title") : "";
  }

  /** Valeur affichée dans le récapitulatif. */
  private show(r: Row, v: string): string {
    const ps = r.f.ps;
    if (ps.kind === "bool") return paramBool(v) ? t("st_on") : t("st_off");
    if (ps.secret) return "••••••";
    if (!v) return "—";
    if (ps.kind === "json" || r.f.control.tagName === "TEXTAREA") return t("pv_edited");
    if (ps.kind === "enum") {
      const o = this.item?.name === ORCH_COMPONENT ? orchParamText(ps.key) : undefined;
      v = o?.choices?.[v]?.[getLang()] ?? v;
    }
    return v.length > 34 ? `${v.slice(0, 33)}…` : v;
  }

  private syncDiff() {
    const host = this.host();
    const list = host.querySelector(".pv-list");
    if (!list) return;
    const changed = this.rows.filter((r) => r.f.changed);
    list.innerHTML = "";
    for (const r of changed) {
      const li = document.createElement("li");
      const long = r.f.ps.kind === "json" || r.f.control.tagName === "TEXTAREA";
      li.innerHTML =
        `<span class="pv-dn">${esc(r.label)}</span>` +
        `<span class="pv-dv">${long ? esc(this.show(r, r.f.read())) : `${esc(this.show(r, r.f.initial))} → <b>${esc(this.show(r, r.f.read()))}</b>`}</span>` +
        `<button type="button" title="${esc(t("pv_undo"))}" aria-label="${esc(t("pv_undo"))}">${icon("undo")}</button>`;
      li.querySelector("button")!.onclick = () => r.f.write(r.f.initial);
      list.appendChild(li);
    }
    if (!changed.length) list.innerHTML = `<li class="pv-empty">${esc(t("pv_none"))}</li>`;
    const warns = host.querySelector(".pv-warns")!;
    warns.innerHTML = "";
    if (this.item?.name === ORCH_COMPONENT) {
      for (const w of WARNS) {
        const r = changed.find((x) => x.f.ps.key === w.key);
        if (r && w.when(r.f.initial, r.f.read())) {
          const li = document.createElement("li");
          li.innerHTML = `${icon("warn")}<span>${esc(t(w.msg))}</span>`;
          warns.appendChild(li);
        }
      }
    }
    host.querySelector(".pv-count")!.textContent = changed.length ? String(changed.length) : "";
    (host.querySelector(".pv-apply") as HTMLButtonElement).disabled = !changed.length || this.busy;
    (host.querySelector(".pv-reset") as HTMLButtonElement).disabled = !changed.length || this.busy;
    host.querySelector<HTMLElement>(".pv-err")!.hidden = true;
  }

  private showErr(msg: string) {
    const e = this.host().querySelector<HTMLElement>(".pv-err");
    if (!e) return;
    e.textContent = msg;
    e.hidden = false;
  }

  /** Valide puis envoie les seules clés modifiées. `true` = appliqué. */
  async apply(): Promise<boolean> {
    const it = this.item;
    if (!it || this.busy) return false;
    const changed = this.rows.filter((r) => r.f.changed);
    if (!changed.length) return true;
    for (const r of this.rows) {
      if (!r.f.changed && !(r.f.ps.required && !r.f.has)) continue;
      const err = validateParam(r.f, r.label);
      if (err) {
        this.showErr(err);
        this.tab = r.group;
        this.applyFilter();
        r.f.el.focus();
        return false;
      }
    }
    const params: Record<string, string> = {};
    for (const r of changed) params[r.f.ps.key] = r.f.read();
    this.busy = true;
    this.syncDiff();
    try {
      const res = await call<{ config_required?: string[] }>("component_set_params", {
        name: it.name,
        params,
        conn: this.deps.connParam(),
      });
      if (res.config_required?.length)
        notify("warn", t("comp_cfg_missing", { l: res.config_required.join(", ") }));
      else notify("success", t("comp_params_done", { name: it.name }));
      const fresh = await this.deps.reload(it.name);
      this.busy = false;
      if (fresh && this.item === it) await this.open(fresh, true);
      else this.syncDiff();
      return true;
    } catch (err) {
      this.busy = false;
      this.syncDiff();
      this.showErr(String(err));
      return false;
    }
  }

  private async leave() {
    if (await this.confirmLeave()) {
      this.item = null;
      this.deps.back();
    }
  }

  /** Vue refermée par le panneau (fermeture / autre sous-vue). */
  reset() {
    this.item = null;
    this.rows = [];
    this.host().innerHTML = "";
  }
}
