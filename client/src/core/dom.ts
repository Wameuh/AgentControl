// ---------------------------------------------------------------------------
// Helpers DOM génériques
// ---------------------------------------------------------------------------

import { invoke } from "./bridge";

export const $ = <T extends HTMLElement>(id: string) =>
  document.getElementById(id) as T;

/** Écrit une ligne dans client.log via la commande log_write (préfixe `ui:`).
 *  Fire-and-forget : le log ne doit jamais faire échouer l'UI. */
export function logUi(level: "debug" | "info" | "warn" | "error", msg: string) {
  invoke("log_write", { level, msg }).catch(() => {});
}

export function esc(s: string): string {
  const d = document.createElement("div");
  d.textContent = s;
  return d.innerHTML;
}

// Pour les valeurs insérées dans un attribut "…" : esc() n'échappe pas '"'.
export const q = (s: string) => s.replace(/"/g, "&quot;");

// ---------------------------------------------------------------------------
// Notifications toast (coin haut-droit) : info/success/warn/error empilés.
// warn+info s'effacent seuls ; les erreurs restent jusqu'au ✕. Le survol
// suspend le timer. Les dialogues modaux (<dialog> top-layer) masquent les
// toasts — leurs erreurs passent par dlgErr() (zone interne au dialogue).
// ---------------------------------------------------------------------------
export type NoticeLevel = "info" | "success" | "warn" | "error";

const TOAST_MAX = 5;
const TOAST_TTL: Record<NoticeLevel, number> = {
  info: 5_000,
  success: 5_000,
  warn: 8_000,
  error: 0, // persistant
};
// Timer auto-dismiss par toast — annulé si le toast est expulsé par le
// cap ou fermé au ✕ (sinon il tirerait remove() sur un nœud détaché).
const toastTimers = new WeakMap<Element, ReturnType<typeof setTimeout>>();
const disarm = (el: Element) => {
  const t0 = toastTimers.get(el);
  if (t0) {
    clearTimeout(t0);
    toastTimers.delete(el);
  }
};

export function notify(level: NoticeLevel, msg: string) {
  const box = $("toasts");
  const d = document.createElement("div");
  d.className = `toast t-${level}`;
  const txt = document.createElement("span");
  txt.className = "toast-msg";
  txt.textContent = msg;
  const x = document.createElement("button");
  x.className = "toast-x";
  x.textContent = "✕";
  d.append(txt, x);
  const arm = (ms: number) => {
    if (ms > 0) toastTimers.set(d, setTimeout(() => d.remove(), ms));
  };
  arm(TOAST_TTL[level]);
  // Survol : le compte à rebours est gelé puis repart de zéro au départ.
  d.addEventListener("mouseenter", () => disarm(d));
  d.addEventListener("mouseleave", () => arm(TOAST_TTL[level]));
  x.onclick = () => {
    disarm(d);
    d.remove();
  };
  box.appendChild(d);
  // Cap : les plus anciens partent d'abord (les erreurs en dernier).
  while (box.childElementCount > TOAST_MAX) {
    const victim =
      [...box.children].find((c) => !c.classList.contains("t-error")) ??
      box.firstElementChild;
    if (!victim) break;
    disarm(victim);
    victim.remove();
  }
  logUi(level === "error" ? "error" : level === "warn" ? "warn" : "info", msg);
}

/** Erreur visible et copiable — toast persistant, aussi versé au client.log. */
export function showErr(msg: string) {
  notify("error", msg);
}

/** Erreur/statut à l'intérieur d'un dialogue modal : les toasts passent sous
 *  le backdrop du top-layer — chaque dialogue long expose sa zone .dlg-err.
 *  `null` efface la zone. */
export function dlgErr(id: string, msg: string | null) {
  const el = $(id);
  if (msg == null) {
    el.hidden = true;
    el.textContent = "";
    return;
  }
  el.textContent = msg;
  el.hidden = false;
  logUi("error", msg);
}

export function mkEv(cls: string, seq: number): HTMLElement {
  const d = document.createElement("div");
  d.className = `ev ${cls}`;
  d.dataset.seq = String(seq);
  return d;
}

export async function copyText(s: string) {
  try {
    await navigator.clipboard.writeText(s);
  } catch {
    const ta = document.createElement("textarea");
    ta.value = s;
    document.body.appendChild(ta);
    ta.select();
    document.execCommand("copy");
    ta.remove();
  }
}
