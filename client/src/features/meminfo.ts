// ---------------------------------------------------------------------------
// Dialogue « Memory » — répartition de la mémoire client, calculée À
// L'OUVERTURE (clic sur le segment `mem` de #sb-stats — aucun polling,
// le refresh 3 s de la barre est inchangé) :
//   1. processus — commande Tauri `mem_breakdown` : RSS du process hôte +
//      enfants classés par rôle (web content / network / gpu / autres),
//      smaps_rollup joint quand lisible (Linux). LE web process est
//      typiquement le plus gros — c'est la réponse au « 400 MB ».
//   2. JS heap — performance.memory.usedJSHeapSize (Chrome-family ;
//      absent sous WebKitGTK/Safari → section omise).
//   3. comptage DOM — engine-agnostic, à l'ouverture.
// Chaque section est feature-détectée : absent = omise, jamais
// d'exception ; « non mesurable sur ce moteur » plutôt qu'une valeur
// inventée.
// ---------------------------------------------------------------------------

import { invoke, isWeb } from "../core/bridge";
import { $ } from "../core/dom";
import { t, type Key } from "../i18n";

interface Smaps {
  rss_mb?: number | null;
  pss_mb?: number | null;
  anon_mb?: number | null;
  shared_mb?: number | null;
  private_mb?: number | null;
  swap_mb?: number | null;
}

interface ProcMember {
  pid: number;
  name: string;
  rss_mb: number;
}

interface ProcGroup {
  role: string;
  count: number;
  /** Somme brute des RSS des membres (pages partagées comptées en
   *  double entre process). */
  rss_mb: number;
  /** Total comptable (Pss — pages partagées réparties) quand le
   *  démon l'a mesuré ; prioritaire sur rss_mb à l'affichage. */
  pss_mb?: number | null;
  name?: string;
  smaps?: Smaps | null;
  members?: ProcMember[];
}

interface MemBreakdown {
  procs?: ProcGroup[];
}

const ROLE_KEY: Record<string, Key> = {
  app: "mem_role_app",
  web: "mem_role_web",
  network: "mem_role_network",
  gpu: "mem_role_gpu",
  other: "mem_role_other",
};

function mb(v: number): string {
  return `${Math.round(v)} MB`;
}

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  cls?: string,
  text?: string,
): HTMLElementTagNameMap[K] {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

function section(body: HTMLElement, titleKey: Key) {
  const sec = el("div", "mem-sec");
  sec.appendChild(el("div", "mem-sec-h", t(titleKey)));
  body.appendChild(sec);
  return sec;
}

function row(parent: HTMLElement, label: string, value: string, dim?: string) {
  const r = el("div", "mem-row");
  const l = el("span", "mem-role", label);
  const v = el("span", "mem-val", value);
  r.append(l, v);
  if (dim) {
    const d = el("div", "mem-sub", dim);
    r.appendChild(d);
  }
  parent.appendChild(r);
}

/** Lignes smaps d'un process : seuls les champs présents sont affichés
 *  (le noyau expose un sous-ensemble variable). */
function smapsLine(s: Smaps): string {
  const parts: string[] = [];
  const push = (labelKey: Key, v?: number | null) => {
    if (typeof v === "number") parts.push(`${t(labelKey)} ${mb(v)}`);
  };
  push("mem_smaps_pss", s.pss_mb);
  push("mem_smaps_anon", s.anon_mb);
  push("mem_smaps_priv", s.private_mb);
  push("mem_smaps_shared", s.shared_mb);
  push("mem_smaps_swap", s.swap_mb);
  return parts.join(" · ");
}

function renderProcs(body: HTMLElement, data: MemBreakdown | null) {
  const procs = data?.procs;
  if (!procs?.length) {
    // Commande absente (vieux binaire) ou webview sans Tauri : la
    // section est omise plutôt qu'afficher un faux « 0 ».
    if (!isWeb) {
      const sec = section(body, "mem_proc_h");
      sec.appendChild(el("div", "mem-note", t("mem_na")));
    }
    return;
  }
  const sec = section(body, "mem_proc_h");
  for (const p of procs) {
    const roleKey = ROLE_KEY[p.role] ?? "mem_role_other";
    const label = t(roleKey);
    const dimParts: string[] = [];
    if (p.count > 1) dimParts.push(`×${p.count}`);
    if (p.name) dimParts.push(p.name);
    // pss_mb = total sans double comptage des pages partagées ; la
    // somme RSS brute reste visible en sous-ligne pour le détail.
    const shown = p.pss_mb ?? p.rss_mb;
    if (p.pss_mb != null && p.pss_mb !== p.rss_mb) {
      dimParts.push(`RSS ${mb(p.rss_mb)}`);
    }
    row(sec, label, mb(shown), dimParts.join(" · ") || undefined);
    if (p.smaps) {
      const line = smapsLine(p.smaps);
      if (line) {
        const sub = el(
          "div",
          "mem-sub mem-smaps",
          `${t("mem_smaps_of", { what: label })} — ${line}`,
        );
        sec.appendChild(sub);
      }
    }
    // Détail par process quand le groupe en contient plusieurs — le
    // coup d'œil « qui mange quoi » reste possible sans alourdir la
    // ligne principale.
    if ((p.members?.length ?? 0) > 1) {
      for (const m of p.members!.slice(0, 6)) {
        const r = el("div", "mem-row mem-member");
        r.append(
          el("span", "mem-role", m.name),
          el("span", "mem-val", mb(m.rss_mb)),
        );
        sec.appendChild(r);
      }
      if (p.members!.length > 6) {
        sec.appendChild(
          el("div", "mem-sub", `+${p.members!.length - 6}`),
        );
      }
    }
  }
}

function renderJsHeap(body: HTMLElement) {
  // performance.memory = Chrome-only (absent WebKitGTK/Safari) —
  // measureUserAgentSpecificMemory volontairement évité : il exige
  // cross-origin isolation.
  const mem = (performance as { memory?: { usedJSHeapSize?: number } })
    .memory;
  const used = mem?.usedJSHeapSize;
  if (typeof used !== "number") return;
  const sec = section(body, "mem_js_h");
  row(sec, t("mem_js_used"), mb(used / 1_048_576));
}

function renderDom(body: HTMLElement) {
  const total = document.getElementsByTagName("*").length;
  const events = $("events")?.childElementCount ?? 0;
  const sessions = document.querySelectorAll("#session-list .sess").length;
  const sec = section(body, "mem_dom_h");
  row(sec, t("mem_dom_total"), String(total));
  if (events > 0) row(sec, t("mem_dom_events"), String(events));
  if (sessions > 0) row(sec, t("mem_dom_sess"), String(sessions));
}

function open() {
  const dlg = $("dlg-mem") as HTMLDialogElement;
  const body = $("mem-body");
  body.textContent = "";
  body.appendChild(el("div", "mem-note", t("mem_loading")));
  if (!dlg.open) dlg.showModal();

  // Toutes les mesures partent en parallèle — chacune dégradée seule.
  const procsP = invoke<MemBreakdown>("mem_breakdown").catch(
    () => null as MemBreakdown | null,
  );
  void Promise.all([procsP]).then(([procs]) => {
    body.textContent = "";
    renderProcs(body, procs);
    renderJsHeap(body);
    renderDom(body);
    if (!body.childElementCount) {
      body.appendChild(el("div", "mem-note", t("mem_na")));
    }
  });
}

export function initMemDialog() {
  const stats = $("sb-stats");
  stats.addEventListener("click", (e) => {
    // Desktop : seul le segment `mem` est cliquable. Web : pas de
    // segment (la barre n'affiche que le trafic) — le clic sur
    // l'indicateur ouvre quand même le dialogue (sections dégradées).
    if (!isWeb && !(e.target as HTMLElement).closest("#sb-mem")) return;
    open();
  });
  // En web, l'indicateur entier devient la cible — curseur main.
  if (isWeb) stats.classList.add("clk");
}
