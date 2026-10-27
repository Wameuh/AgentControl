// ---------------------------------------------------------------------------
// Sélecteurs mode/modèle/effort du header : helpers de décomposition des
// identifiants de modèles (effort suffixe ou [params]) et wiring des
// <select>. Le dialogue Fusion reste dans fusion-dialog.ts.
// ---------------------------------------------------------------------------

import { $, showErr } from "../core/dom";
import { call } from "../core/rpc";
import { app } from "../core/state";
import type { CmdInfo } from "../core/types";

export const cmdName = (c: CmdInfo) => (typeof c === "string" ? c.replace(/^\//, "") : c.name);
export const cmdDesc = (c: CmdInfo) => (typeof c === "string" ? "" : (c.description ?? ""));
export const cmdHint = (c: CmdInfo) => (typeof c === "string" ? "" : (c.hint ?? ""));

// Devin : les modèles portent l'effort en suffixe (-medium/-high/...) ;
// Cursor/Codex : variantes `base[param=v,…]` (ex. gpt-6.1-sol[medium]).
const EFF_RE = /-(low|minimal|medium|high|max|lightning)$/;
const BRACKET_RE = /(\[[^\]]*\])$/;
export const splitModel = (v: string): [string, string] => {
  const b = BRACKET_RE.exec(v);
  if (b) return [v.slice(0, -b[0].length), b[1]];
  const m = EFF_RE.exec(v);
  return m ? [v.slice(0, -m[0].length), m[1]] : [v, ""];
};
// base + variante : "-" pour les efforts suffixe, concat pour les [params]
export const joinModel = (base: string, eff: string) =>
  !eff ? base : eff.startsWith("[") ? base + eff : `${base}-${eff}`;

/** Décompose "fusion-<prim>-<eff>-sidekick-<side>-<eff>" → [pb, pe, sb, se]. */
export function fusionParts(v: string): [string, string, string, string] | null {
  if (!v.startsWith("fusion-")) return null;
  const rest = v.slice("fusion-".length);
  const i = rest.indexOf("-sidekick-");
  if (i < 0) return null;
  const [pb, pe] = splitModel(rest.slice(0, i));
  const [sb, se] = splitModel(rest.slice(i + "-sidekick-".length));
  return [pb, pe, sb, se];
}

/** Remplit le select d'effort pour une base de modèle donnée. */
export function effortsFor(base: string): string[] {
  const opts: { v: string }[] = JSON.parse(($("model-select") as HTMLSelectElement).dataset.opts || "[]");
  const effs = new Set<string>();
  for (const o of opts) {
    if (o.v === base || o.v.startsWith(base + "-") || o.v.startsWith(base + "[")) {
      const e = splitModel(o.v)[1];
      if (e) effs.add(e);
    }
  }
  return [...effs];
}

export function fillEfforts(base: string, cur: string) {
  const es = $("effort-select") as HTMLSelectElement;
  const effs = effortsFor(base);
  es.innerHTML = "";
  if (effs.length <= 1) return;
  for (const e of effs) {
    const o = document.createElement("option");
    o.value = e;
    // "[context=…,effort=…]" → contenu lisible sans les crochets
    // ("[]" de cursor = variante par défaut → garder "[]" plutôt que vide)
    o.textContent = e.startsWith("[") ? e.slice(1, -1) || e : e;
    es.appendChild(o);
  }
  if (effs.includes(cur)) es.value = cur;
}

export function applyModelValue(v: string) {
  if (!app.current) return;
  const md = $("model-select") as HTMLSelectElement;
  const req = md.dataset.cfgid
    ? call("set_config_option", { session: app.current.id, config_id: md.dataset.cfgid, value: v })
    : call("set_model", { session: app.current.id, model: v });
  req.catch((er) => showErr(String(er)));
}

/** Combinaisons Fusion réellement annoncées par l'agent (liste fermée :
 *  composer librement fusion-A-eff-sidekick-B-eff renvoie Invalid params). */
export function fusionTable() {
  const opts: { v: string; label: string }[] = JSON.parse(
    ($("model-select") as HTMLSelectElement).dataset.opts || "[]",
  );
  const valid = new Set<string>();
  const prims = new Map<string, Set<string>>();               // base primaire → efforts
  const sides = new Map<string, Map<string, Set<string>>>(); // "pb|pe" → sidekick → efforts
  for (const o of opts) {
    const p = fusionParts(o.v);
    if (!p) continue;
    valid.add(o.v);
    const [pb, pe, sb, se] = p;
    if (!prims.has(pb)) prims.set(pb, new Set());
    prims.get(pb)!.add(pe);
    const key = `${pb}|${pe}`;
    if (!sides.has(key)) sides.set(key, new Map());
    const m = sides.get(key)!;
    if (!m.has(sb)) m.set(sb, new Set());
    m.get(sb)!.add(se);
  }
  return { valid, prims, sides };
}

export function fillSel(s: HTMLSelectElement, items: [string, string][]) {
  s.innerHTML = "";
  for (const [v, label] of items) {
    const o = document.createElement("option");
    o.value = v;
    o.textContent = label;
    s.appendChild(o);
  }
}

/** Wiring des <select> du header ; `openFusion` injecté (fusion-dialog). */
export function initModelBar(openFusion: () => void) {
  ($("mode-select") as HTMLSelectElement).onchange = (e) => {
    if (!app.current) return;
    const sel = e.target as HTMLSelectElement;
    const req = sel.dataset.cfgid
      ? call("set_config_option", { session: app.current.id, config_id: sel.dataset.cfgid, value: sel.value })
      : call("set_mode", { session: app.current.id, mode: sel.value });
    req.catch((er) => showErr(String(er)));
  };

  ($("model-select") as HTMLSelectElement).onchange = (e) => {
    const sel = e.target as HTMLSelectElement;
    if (sel.value === "__fusion") {
      openFusion();
      return;
    }
    fillEfforts(sel.value, "");
    const eff = ($("effort-select") as HTMLSelectElement).value;
    applyModelValue(joinModel(sel.value, eff));
  };

  ($("effort-select") as HTMLSelectElement).onchange = (e) => {
    const base = ($("model-select") as HTMLSelectElement).value;
    const eff = (e.target as HTMLSelectElement).value;
    if (base && base !== "__fusion") applyModelValue(joinModel(base, eff));
  };

  ($("model-free") as HTMLInputElement).addEventListener("keydown", async (e) => {
    if (e.key !== "Enter" || !app.current) return;
    const v = (e.target as HTMLInputElement).value.trim();
    if (v) await call("set_model", { session: app.current.id, model: v }).catch((er) => showErr(String(er)));
  });

  $("btn-fusion-edit").onclick = () => openFusion();
}
