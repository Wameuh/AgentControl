// ---------------------------------------------------------------------------
// Suites d'outils repliées (S4) : ≥2 .ev.tool consécutifs = une suite —
// seul le dernier reste visible, avec une pastille « +N outils » qui
// déplie les autres au tap. Les marqueurs vivent sur les .ev eux-mêmes
// (jamais de nœud frère ajouté : la pagination de chat-view insère et
// recherche par data-seq, les enfants de #events doivent rester des .ev).
// ---------------------------------------------------------------------------

import { $ } from "../core/dom";
import { isAndroid } from "../core/chrome";
import { t } from "../i18n";

/** Ids des suites dépliées par l'utilisateur — survit aux re-scans
 *  (l'observer repasse à chaque insertion d'event). */
const open = new Set<string>();

export function initToolRuns() {
  if (!isAndroid()) return;
  let queued = false;
  const mo = new MutationObserver(() => {
    if (queued) return;
    queued = true;
    requestAnimationFrame(() => { queued = false; group(); });
  });
  mo.observe($("events"), { childList: true });
  group();
}

function group() {
  const events = $("events");
  // Idempotent : retirer les marques d'un passage précédent avant de
  // recalculer les suites (le fil a pu changer — insertions, .done…).
  for (const el of events.querySelectorAll<HTMLElement>("[data-run]")) {
    delete el.dataset.run;
    el.classList.remove("run-rest", "run-last", "run-open");
  }
  for (const b of events.querySelectorAll(".run-sum")) b.remove();

  // Suites de ≥2 .ev.tool consécutifs (enfants directs de #events).
  const kids = [...events.children] as HTMLElement[];
  let run: HTMLElement[] = [];
  const flush = () => {
    if (run.length >= 2) markRun(run);
    run = [];
  };
  for (const k of kids) {
    if (k.classList.contains("ev") && k.classList.contains("tool")) run.push(k);
    else flush();
  }
  flush();
}

function markRun(members: HTMLElement[]) {
  const id = members[0].dataset.seq ?? "";
  const last = members[members.length - 1];
  for (const m of members) {
    m.dataset.run = id;
    m.classList.add(m === last ? "run-last" : "run-rest");
    if (open.has(id)) m.classList.add("run-open");
  }
  const sum = document.createElement("button");
  sum.type = "button";
  sum.className = "run-sum";
  sum.textContent = open.has(id) ? "−" : t("tool_run", { n: members.length - 1 });
  sum.addEventListener("click", (e) => {
    // stopPropagation : le clic ne doit pas déplier le corps du tool
    // (le .tool-head parent a son propre toggle).
    e.stopPropagation();
    if (open.has(id)) open.delete(id);
    else open.add(id);
    for (const m of document.querySelectorAll<HTMLElement>(`[data-run="${id}"]`))
      m.classList.toggle("run-open", open.has(id));
    sum.textContent = open.has(id) ? "−" : t("tool_run", { n: members.length - 1 });
  });
  last.querySelector(".tool-head")?.appendChild(sum);
}
