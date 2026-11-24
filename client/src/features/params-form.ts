// ---------------------------------------------------------------------------
// Champs de paramètres pilotés par un `ParamSpec` (schéma déclaré par le
// démon) — partagés par le dialogue d'intégration du catalogue
// (components.ts) et la vue pleine page des paramètres (params-view.ts) :
// construction du contrôle selon le type, lecture normalisée (comparaison
// avec la valeur initiale ou le défaut), validation avant envoi.
// ---------------------------------------------------------------------------

import { t, type Key } from "../i18n";
import type { ParamSpec } from "../core/types";

/** Valeur chaîne d'un param `bool` (params/default sont en forme
 *  chaîne — mêmes acceptes que la validation démon). */
export function paramBool(v: string): boolean {
  return ["true", "yes", "on", "1"].includes(v.trim().toLowerCase());
}

/** Chaîne CSV d'un param `list` → entrées (inverse à la collecte :
 *  les lignes du textarea repartent jointes par ","). */
export function paramList(v: string): string[] {
  return v.split(",").map((s) => s.trim()).filter(Boolean);
}

/** Libellé de section du formulaire params — groupes connus de
 *  l'orchestrateur ; un groupe inconnu s'affiche brut (extensible). */
const SEC_KEYS: Record<string, Key> = {
  workers: "cp_sec_workers",
  pipeline: "cp_sec_pipeline",
  pr_reviews: "cp_sec_pr_reviews",
  general: "cp_sec_general",
};
export const secLabel = (g: string) => {
  const k = SEC_KEYS[g];
  return k ? t(k) : g;
};

/** Chaîne stockée d'un param `json` (forme canonique compacte) → JSON
 *  pretty-print pour le textarea ; invalide → la chaîne telle quelle. */
export function paramJson(v: string): string {
  try {
    return JSON.stringify(JSON.parse(v), null, 2);
  } catch {
    return v;
  }
}

/** Tri récursif des clés d'objet — reproduit le canon démon (BTreeMap
 *  serde_json) : comparer des canons côté client évite un patch sans
 *  changement réel (un patch = render → restart d'un process vivant). */
export function sortJson(x: unknown): unknown {
  if (Array.isArray(x)) return x.map(sortJson);
  if (x !== null && typeof x === "object") {
    const o = x as Record<string, unknown>;
    return Object.fromEntries(
      Object.keys(o).sort().map((k) => [k, sortJson(o[k])]),
    );
  }
  return x;
}

/** JSON → forme canonique compacte à clés triées ; null si invalide. */
export function canonJson(v: string): string | null {
  try {
    return JSON.stringify(sortJson(JSON.parse(v)));
  } catch {
    return null;
  }
}

/** Équipe `worker_pool` : le démon omet model/effort/mode vides
 *  (`skip_serializing_if`) alors que le widget les écrit "" — retirés
 *  ici, sinon l'équipe paraîtrait modifiée dès l'ouverture. */
function canonPool(v: string): string | null {
  try {
    const a = JSON.parse(v);
    if (!Array.isArray(a)) return null;
    const clean = a.map((e) =>
      e && typeof e === "object"
        ? Object.fromEntries(Object.entries(e as Record<string, unknown>).filter(([, x]) => x !== ""))
        : e,
    );
    return JSON.stringify(sortJson(clean));
  } catch {
    return null;
  }
}

/** Forme comparable d'une valeur : booléens en "true"/"false", JSON
 *  canonique, le reste rogné — deux saisies équivalentes sont égales. */
export function normParam(ps: ParamSpec, v: string): string {
  if (ps.kind === "bool") return paramBool(v) ? "true" : "false";
  if (ps.kind === "json") {
    const s = v.trim();
    if (!s) return "";
    if (ps.key === "worker_pool") return canonPool(s) ?? canonJson(s) ?? s;
    return canonJson(s) ?? s;
  }
  if (ps.kind === "list") return paramList(v).join(",");
  return v.trim();
}

/** Texte long (prompts) : zone multiligne plutôt qu'un champ d'une ligne. */
const isLongText = (ps: ParamSpec) =>
  ps.kind === "string" && /(^|_)prompt$/.test(ps.key);

export type ParamInput = HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement;

/** Éditeur riche d'un param (widget équipe `worker_pool`) : la valeur
 *  canonique vit dans `hidden`, qui émet `change` à chaque modification. */
export type PoolFactory = (
  cur: string,
  ps: ParamSpec,
) => { wrap: HTMLElement; hidden: HTMLInputElement };

export interface FieldOpts {
  pool?: PoolFactory;
  /** Libellé affiché d'une valeur d'enum (traductions côté client). */
  choiceLabel?: (value: string) => string | undefined;
  /** Classe des cases booléennes (interrupteur de la vue pleine page). */
  boolClass?: string;
}

/** Un champ : contrôle + valeur initiale normalisée + lecture/écriture. */
export class ParamField {
  /** Une valeur est stockée côté démon (sémantique des `secret`). */
  readonly has: boolean;
  /** Valeur effective à l'ouverture (stockée, sinon défaut), normalisée. */
  readonly initial: string;
  /** Défaut du schéma, normalisé. */
  readonly defaultValue: string;
  el!: ParamInput;
  control!: HTMLElement;
  private listeners: (() => void)[] = [];

  constructor(
    readonly ps: ParamSpec,
    current: Record<string, string>,
    private opts: FieldOpts = {},
  ) {
    const cur = current[ps.key];
    this.has = cur !== undefined && cur !== "";
    const raw = this.has ? cur : (ps.default ?? "");
    this.defaultValue = normParam(ps, ps.default ?? "");
    this.initial = ps.secret ? "" : normParam(ps, raw);
    this.build(raw);
  }

  private build(raw: string) {
    const ps = this.ps;
    let el: ParamInput;
    if (ps.kind === "bool") {
      el = document.createElement("input");
      el.type = "checkbox";
      if (this.opts.boolClass) el.className = this.opts.boolClass;
      el.checked = paramBool(raw);
      this.control = el;
    } else if (ps.key === "worker_pool" && ps.kind === "json" && this.opts.pool) {
      // « Équipe » : table profil/slots/modèle/effort/mode — la valeur
      // reste le JSON canonique dans un input masqué.
      const w = this.opts.pool(raw, ps);
      el = w.hidden;
      this.control = w.wrap;
    } else if (ps.kind === "enum") {
      const sel = document.createElement("select");
      const opts = ps.choices ?? [];
      // Valeur vide : toujours offerte en tête — requise sans défaut →
      // l'utilisateur doit trancher ; non requise → « — » = non positionné.
      if (!ps.required || (!this.has && !ps.default)) {
        const o = document.createElement("option");
        o.value = "";
        o.textContent = "—";
        sel.appendChild(o);
      }
      opts.forEach((c, i) => {
        const o = document.createElement("option");
        o.value = c;
        o.textContent = this.opts.choiceLabel?.(c) || ps.choice_labels?.[i] || c;
        sel.appendChild(o);
      });
      // Valeur stockée hors `choices` (spec éditée à la main ou liste
      // dynamique périmée) : la conserver plutôt que la retirer.
      if (this.has && raw && !opts.includes(raw)) {
        const o = document.createElement("option");
        o.value = raw;
        o.textContent = `${raw} (?)`;
        sel.appendChild(o);
      }
      sel.value = raw;
      el = sel;
      this.control = sel;
    } else if (ps.kind === "json" || isLongText(ps)) {
      const ta = document.createElement("textarea");
      ta.rows = ps.kind === "json" ? 4 : 6;
      ta.spellcheck = false;
      ta.value = ps.kind === "json" ? paramJson(raw) : raw;
      el = ta;
      this.control = ta;
    } else {
      const inp = document.createElement("input");
      inp.type = ps.secret ? "password" : ps.kind === "int" ? "number" : "text";
      inp.autocomplete = "off";
      inp.spellcheck = false;
      if (ps.secret && this.has) inp.placeholder = "••••••";
      inp.value = ps.secret ? "" : raw;
      if (ps.kind === "int") inp.step = "1";
      el = inp;
      this.control = inp;
    }
    this.el = el;
    const fire = () => this.listeners.forEach((f) => f());
    el.addEventListener("input", fire);
    el.addEventListener("change", fire);
  }

  /** Valeur saisie, normalisée (bool "true"/"false", JSON canonique). */
  read(): string {
    if (this.ps.kind === "bool")
      return (this.el as HTMLInputElement).checked ? "true" : "false";
    return normParam(this.ps, this.el.value);
  }

  /** Remet une valeur (retour au défaut, annulation d'un changement). */
  write(v: string) {
    if (this.ps.kind === "bool") {
      (this.el as HTMLInputElement).checked = paramBool(v);
    } else if (this.ps.key === "worker_pool" && this.opts.pool && this.ps.kind === "json") {
      // Le widget équipe se reconstruit : ses lignes dérivent du JSON.
      const old = this.control;
      this.build(v);
      old.replaceWith(this.control);
    } else if (this.ps.kind === "json") {
      this.el.value = paramJson(v);
    } else {
      this.el.value = v;
    }
    this.listeners.forEach((f) => f());
  }

  /** Différent de la valeur d'ouverture (un secret vide = inchangé). */
  get changed(): boolean {
    if (this.ps.secret) return this.el.value.trim() !== "";
    return this.read() !== this.initial;
  }

  onInput(f: () => void) {
    this.listeners.push(f);
  }
}

/** Contrôle d'une saisie avant envoi — message d'erreur, null si valide.
 *  `label` = libellé affiché (traduit) cité dans le message. */
export function validateParam(f: ParamField, label: string): string | null {
  const ps = f.ps;
  const v = ps.kind === "bool" ? f.read() : f.el.value.trim();
  if (!v) {
    if (ps.secret && f.has) return null;
    return ps.required ? t("comp_param_required", { k: label }) : null;
  }
  if (ps.kind === "int" && !/^-?\d+$/.test(v))
    return t("comp_param_badint", { k: label });
  if (ps.kind === "duration" && !/^(\d+(ms|min|s|m|h|d|w)\s*)+$/.test(v))
    return t("comp_param_baddur", { k: label });
  if (ps.kind === "enum" && !ps.open_enum && !(ps.choices ?? []).includes(v))
    return t("comp_param_badenum", { k: label });
  if (ps.kind === "json") {
    try {
      JSON.parse(v);
    } catch {
      return t("comp_param_badjson", { k: label });
    }
  }
  if (ps.validation) {
    try {
      if (!new RegExp(ps.validation).test(v)) return t("comp_param_badfmt", { k: label });
    } catch {
      /* regex démon-seule : le serveur revalide */
    }
  }
  return null;
}
