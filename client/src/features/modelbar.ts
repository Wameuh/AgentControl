// ---------------------------------------------------------------------------
// Sélecteurs mode/modèle/effort du header : helpers de décomposition des
// identifiants de modèles (effort suffixe ou [params]) et wiring des
// <select>. Le dialogue Fusion reste dans fusion-dialog.ts.
// ---------------------------------------------------------------------------

import { $, showErr } from "../core/dom";
import { call } from "../core/rpc";
import { app } from "../core/state";
import type { CmdInfo, ConfigOption, SessionMeta } from "../core/types";
import { costFor } from "./modelcosts";

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

const modelOpts = (): { v: string }[] => JSON.parse(($("model-select") as HTMLSelectElement).dataset.opts || "[]");

/** Remplit le select d'effort pour une base de modèle donnée. */
export function effortsFor(base: string): string[] {
  const effs = new Set<string>();
  for (const o of modelOpts()) {
    const [b, e] = splitModel(o.v);
    if (b === base && e) effs.add(e);
  }
  return [...effs];
}

/** Valeur complète à envoyer pour `base` : seulement une option annoncée par l'agent
 *  (l'agent refuse une base nue quand elle n'existe qu'avec un suffixe d'effort). */
export function resolveModel(base: string, eff: string): string {
  const effs = effortsFor(base);
  if (eff && effs.includes(eff)) return joinModel(base, eff);
  if (modelOpts().some((o) => o.v === base)) return base;
  return effs.length ? joinModel(base, effs[0]) : base;
}

export function fillEfforts(base: string, cur: string) {
  const es = $("effort-select") as HTMLSelectElement;
  const effs = effortsFor(base);
  es.innerHTML = "";
  for (const e of effs) {
    const o = document.createElement("option");
    o.value = e;
    // "[context=…,effort=…]" → contenu lisible sans les crochets
    // ("[]" de cursor = variante par défaut → garder "[]" plutôt que vide)
    const label = e.startsWith("[") ? e.slice(1, -1) || e : e;
    // Coût catalogue du modèle complet (base + effort) quand dispo —
    // suffixe indicatif, absent si le provider n'expose rien.
    const cost = costFor(joinModel(base, e));
    o.textContent = label + (cost ? ` · ${cost.label}` : "");
    if (cost?.detail) o.title = cost.detail;
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

// ---------------------------------------------------------------------------
// thought_level : effort de raisonnement annoncé comme config_option à part
// entière (Devin — « Thinking », valeurs propres à chaque modèle). Quand il
// est annoncé, effort-select se rebinde dessus ; sinon repli sur l'heuristique
// de suffixe des ids de modèles (comportement historique).
// ---------------------------------------------------------------------------

/** config_option « effort de raisonnement » annoncée par l'agent, s'il y en a une. */
export function thoughtLevelOpt(copts: ConfigOption[] | undefined): ConfigOption | undefined {
  return copts?.find((o) => o.id === "thought_level" || o.category === "thought_level");
}

/** Remplit effort-select depuis les options annoncées d'une config_option
 *  (thought_level) — la valeur courante rapportée par l'agent fait foi. */
export function fillThoughtLevels(sel: HTMLSelectElement, opt: ConfigOption) {
  sel.innerHTML = "";
  for (const o of opt.options) {
    const el = document.createElement("option");
    el.value = o.value;
    el.textContent = o.name || o.value;
    sel.appendChild(el);
  }
  sel.value = opt.currentValue ?? "";
}

// Effort souhaité mémorisé par (session, modèle) : thought_level est PAR
// MODÈLE — les options et le défaut varient d'un modèle à l'autre, donc un
// choix ne se ré-applique qu'au retour sur le modèle où il a été fait.
// Mémoire UI (volatil) : le replay respawn reste au démon (desired_config,
// dernière valeur posée — le client re-pousse au besoin via la transition).
const effortMem = new Map<string, Map<string, string>>();
const lastModelSeen = new Map<string, string>();

/** Vrai quand `model` diffère du dernier modèle vu pour cette session —
 *  déclencheur unique de la ré-application d'effort (une transition, pas
 *  un état : updateHeader tourne à chaque event méta). */
export function modelTransition(sid: string, model: string): boolean {
  const prev = lastModelSeen.get(sid);
  if (prev === model) return false;
  lastModelSeen.set(sid, model);
  return true;
}

export function rememberEffort(sid: string, model: string, effort: string) {
  let m = effortMem.get(sid);
  if (!m) effortMem.set(sid, (m = new Map()));
  m.set(model, effort);
}

export function rememberedEffort(sid: string, model: string): string | undefined {
  return effortMem.get(sid)?.get(model);
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

/** Wiring des <select> du header ; `openFusion` injecté (fusion-dialog),
 *  `metaChanged` re-rend le header après une meta rafraîchie. */
export function initModelBar(openFusion: () => void, metaChanged: () => void = () => {}) {
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
    // thought_level annoncé : l'effort est orthogonal au modèle — envoyer
    // la variante annoncée sans joindre de suffixe d'effort (le niveau se
    // règle via effort-select, et le agent ré-annonce SES options pour le
    // nouveau modèle — sa valeur courante fera foi, cf. updateHeader).
    if (thoughtLevelOpt(app.current?.config_options)) {
      applyModelValue(resolveModel(sel.value, ""));
      return;
    }
    fillEfforts(sel.value, "");
    applyModelValue(resolveModel(sel.value, ($("effort-select") as HTMLSelectElement).value));
  };

  ($("effort-select") as HTMLSelectElement).onchange = (e) => {
    const base = ($("model-select") as HTMLSelectElement).value;
    const eff = (e.target as HTMLSelectElement).value;
    const sid = app.current?.id;
    const thought = thoughtLevelOpt(app.current?.config_options);
    // Config_option réelle : set_config_option dedicated, effort mémorisé
    // pour CE modèle (ré-appliqué au prochain retour dessus).
    if (thought && sid && app.current?.model) {
      call("set_config_option", {
        session: sid,
        config_id: thought.id,
        value: eff,
      }).catch((er) => showErr(String(er)));
      rememberEffort(sid, app.current.model, eff);
      return;
    }
    if (base && base !== "__fusion") applyModelValue(resolveModel(base, eff));
  };

  ($("model-free") as HTMLInputElement).addEventListener("keydown", async (e) => {
    if (e.key !== "Enter" || !app.current) return;
    const v = (e.target as HTMLInputElement).value.trim();
    if (v) await call("set_model", { session: app.current.id, model: v }).catch((er) => showErr(String(er)));
  });

  // ⟳ : probe jetable côté démon (spawn session/new dans un cwd scratch)
  // → cache du profil + meta des sessions du profil rafraîchis (listes
  // mode/modèle/config sans recréer la session — ex. nouveau modèle
  // provider). session_info relit les listes ; l'event config_options
  // poussé en parallèle mène au même rendu (idempotent).
  ($("btn-caps-refresh") as HTMLButtonElement).onclick = async (e) => {
    const s = app.current;
    if (!s) return;
    const btn = e.currentTarget as HTMLButtonElement;
    btn.disabled = true;
    btn.classList.add("spin");
    try {
      await call("refresh_agent_caps", { agent: s.agent, conn: s.conn });
      const info = await call<{ meta: SessionMeta }>("session_info", {
        session: s.id,
        conn: s.conn,
      });
      s.available_modes = info.meta.available_modes;
      s.available_models = info.meta.available_models;
      s.config_options = info.meta.config_options;
      s.mode = info.meta.mode;
      s.model = info.meta.model;
      metaChanged();
    } catch (er) {
      showErr(String(er));
    } finally {
      btn.disabled = false;
      btn.classList.remove("spin");
    }
  };

  $("btn-fusion-edit").onclick = () => openFusion();
}
