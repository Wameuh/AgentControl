// ---------------------------------------------------------------------------
// Dock de permission (S3) : carte ancrée au-dessus du composer tant qu'une
// permission ou une question attend une réponse — plus besoin de chercher
// la carte dans le fil. Les boutons du dock délèguent aux boutons réels
// de la carte (le RPC respond reste dans renderPermission/renderQuestion).
// ---------------------------------------------------------------------------

import { $, esc } from "../core/dom";
import { isAndroid } from "../core/chrome";
import { app } from "../core/state";
import { t } from "../i18n";
import { icon } from "../ui/icons";
import { findPending } from "./pending";

type JumpFn = (seq: number) => Promise<boolean>;

let jumpToSeq: JumpFn;
let idx = 0;
let lastCount = 0;
let lastSid: string | null = null;

export function initPermDock(jump: JumpFn) {
  if (!isAndroid()) return;
  jumpToSeq = jump;
  // Les cartes perm vivent dans #events et passent .done à la résolution —
  // observer enfants + changements de classe, regroupés par rAF.
  let queued = false;
  const mo = new MutationObserver(() => {
    if (queued) return;
    queued = true;
    requestAnimationFrame(() => { queued = false; sync(); });
  });
  mo.observe($("events"), {
    childList: true,
    subtree: true,
    attributes: true,
    attributeFilter: ["class"],
  });
}

function dock() { return $("perm-dock"); }

function pendingCards(): HTMLElement[] {
  return [...$("events").querySelectorAll<HTMLElement>(".ev.perm[data-req]:not(.done)")];
}

function sync() {
  const d = dock();
  const cards = pendingCards();
  if (!cards.length) {
    // Aucune carte dans le DOM mais l'agent attend une saisie : la demande
    // est plus haut dans l'historique (paginée) → ligne « Afficher ».
    if (app.current?.state === "waiting_input") {
      idx = 0;
      d.innerHTML =
        `<div class="pd-off">${icon("question")}<span>${esc(t("perm_dock_offscreen"))}</span>` +
        `<button type="button" class="pd-show">${esc(t("perm_dock_show"))}</button></div>`;
      d.querySelector<HTMLElement>(".pd-show")!.onclick = () => void showOffscreen();
      d.hidden = false;
      return;
    }
    d.hidden = true;
    return;
  }
  // Une nouvelle demande remonte en tête (ordre DOM = chronologique) ;
  // la navigation ‹/› de l'utilisateur tient tant que le compte est stable.
  const sid = app.current?.id ?? null;
  if (sid !== lastSid || idx >= cards.length || cards.length > lastCount)
    idx = cards.length - 1;
  lastSid = sid;
  lastCount = cards.length;
  const card = cards[idx];
  d.hidden = false;
  d.innerHTML = "";
  d.appendChild(
    card.classList.contains("question") ? renderQuestion(card, cards.length) : renderPerm(card, cards.length));
}

/** La demande est hors DOM : remonter à la carte via les derniers events. */
async function showOffscreen() {
  const s = app.current;
  if (!s) return;
  const reqs = await findPending(s);
  const target = reqs[reqs.length - 1];
  if (target) void jumpToSeq(target.seq);
}

// ── Rendu ─────────────────────────────────────────────────────────────────

function head(card: HTMLElement, count: number, icn: "lock" | "question") {
  const h = document.createElement("div");
  h.className = "pd-head";
  const title = card.querySelector(".perm-title")?.textContent?.trim() ?? "";
  const kind = card.querySelector(".perm-kind")?.textContent?.trim() ?? "";
  h.innerHTML = `${icon(icn)}<span class="pd-title">${esc(title)}</span>` +
    (kind ? `<span class="pd-kind">${esc(kind)}</span>` : "");
  if (count > 1) {
    const navd = document.createElement("span");
    navd.className = "pd-nav";
    navd.innerHTML =
      `<button type="button" class="pd-arrow" data-d="-1" aria-label="‹">${icon("chev-down", "pd-rot")}</button>` +
      `<span class="pd-count">${esc(t("perm_dock_count", { i: idx + 1, n: count }))}</span>` +
      `<button type="button" class="pd-arrow" data-d="1" aria-label="›">${icon("chev-down")}</button>`;
    for (const a of navd.querySelectorAll<HTMLButtonElement>(".pd-arrow")) {
      a.onclick = () => { idx = (idx + Number(a.dataset.d) + count) % count; sync(); };
    }
    h.appendChild(navd);
  }
  // Tap sur le titre → remonter à la carte dans le fil.
  h.querySelector<HTMLElement>(".pd-title")!.onclick = () =>
    card.scrollIntoView({ block: "center" });
  return h;
}

function renderPerm(card: HTMLElement, count: number): HTMLElement {
  const w = document.createElement("div");
  w.className = "pd-card";
  w.appendChild(head(card, count, "lock"));
  const detail = card.querySelector(".perm-detail")?.textContent;
  if (detail) {
    const det = document.createElement("details");
    det.className = "pd-detail";
    det.innerHTML = `<summary>${esc(detail.split("\n")[0].slice(0, 80))}</summary><pre>${esc(detail)}</pre>`;
    w.appendChild(det);
  }
  const opts = document.createElement("div");
  opts.className = "pd-opts";
  // Ordre visuel du dock : refus d'abord (rouge), puis les autorisations —
  // la première .btn-allow de la carte est l'action principale (accent).
  const orig = [...card.querySelectorAll<HTMLButtonElement>(".perm-opts button")];
  const firstAllow = card.querySelector<HTMLButtonElement>(".perm-opts .btn-allow");
  const ordered = [
    ...orig.filter((o) => o.classList.contains("btn-reject")),
    ...orig.filter((o) => !o.classList.contains("btn-reject")),
  ];
  for (const o of ordered) {
    const b = document.createElement("button");
    b.type = "button";
    b.textContent = o.textContent;
    b.className = o.classList.contains("btn-reject") ? "btn-reject" : "btn-allow";
    if (o === firstAllow) b.classList.add("pd-primary");
    b.onclick = () => o.click(); // le RPC respond reste côté carte
    opts.appendChild(b);
  }
  w.appendChild(opts);
  return w;
}

function renderQuestion(card: HTMLElement, count: number): HTMLElement {
  const w = document.createElement("div");
  w.className = "pd-card pd-question";
  w.appendChild(head(card, count, "question"));
  const row = document.createElement("div");
  row.className = "pd-qrow";
  const origInput = card.querySelector<HTMLInputElement>(".q-input")!;
  const inp = document.createElement("input");
  inp.placeholder = origInput.placeholder;
  const ans = document.createElement("button");
  ans.type = "button";
  ans.className = "btn-allow pd-primary";
  ans.textContent = card.querySelector(".btn-allow")?.textContent ?? "";
  ans.onclick = () => {
    origInput.value = inp.value;
    card.querySelector<HTMLButtonElement>(".btn-allow")?.click();
  };
  inp.addEventListener("keydown", (e) => {
    if (e.key === "Enter") ans.click();
  });
  const ign = document.createElement("button");
  ign.type = "button";
  ign.className = "btn-reject";
  ign.textContent = card.querySelector(".btn-reject")?.textContent ?? "";
  ign.onclick = () => card.querySelector<HTMLButtonElement>(".btn-reject")?.click();
  row.append(inp, ign, ans);
  w.appendChild(row);
  return w;
}
