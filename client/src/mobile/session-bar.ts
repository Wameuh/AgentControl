// ---------------------------------------------------------------------------
// Barre de session mobile (S1) : en session, remplace #titlebar +
// #chat-header (body.m-in-session) — ☰ drawer à gauche, gemme d'état,
// titre + sous-titre (tap → feuille Session), Stop à droite quand l'agent
// est occupé. Synchronisée par le hook afterHeader de ChatView (fin
// d'updateHeader/setChatTitle).
// ---------------------------------------------------------------------------

import { app } from "../core/state";
import { $, esc } from "../core/dom";
import { isAndroid } from "../core/chrome";
import { nav } from "../ui/nav";
import { icon } from "../ui/icons";
import { t } from "../i18n";
import { dotCls, dotIcon } from "../features/status";
import { joinModel } from "../features/modelbar";
import { refreshSessionSheet } from "./session-sheet";

let gemCls = "";

/** Câble les trois zones de la barre — Android uniquement (le reste de
 *  l'app garde titlebar + chat-header). Le ☰ reste un menu en phase 2 ;
 *  la phase 3 le transformera en retour ‹. */
export function initSessionBar(openSheet: () => void) {
  if (!isAndroid()) return;
  $("m-sbar-nav").onclick = () => nav.openDrawer();
  $("m-sbar-title").onclick = openSheet;
  $("m-sbar-title").setAttribute("aria-label", t("ss_open"));
  $("m-sbar-stop").onclick = () => ($("btn-cancel") as HTMLButtonElement).click();
}

/** Resynchronise la barre depuis app.current + le DOM du header (source
 *  unique : badge d'état, selects, jauge contexte, bouton Stop). Sans
 *  session ouverte, m-in-session retombe → titlebar + header reviennent
 *  (écran d'accueil inchangé). */
export function renderSessionBar() {
  if (!isAndroid()) return;
  const s = app.current;
  document.body.classList.toggle("m-in-session", !!s);
  if (!s) return;
  const st = s.state ?? "dead";
  const cls = dotCls(st, s.last_error);
  const gem = $("m-sbar-gem");
  if (cls !== gemCls) {
    gemCls = cls;
    gem.innerHTML = dotIcon(st, s.last_error);
  }
  gem.classList.toggle("spin", st === "working" || st === "compacting");
  $("m-sbar-name").textContent = s.title;
  // « agent · modèle » + jauge contexte + état terminal — sous-titre
  // relu depuis le DOM du header masqué (même source d'information).
  const sub = [`${s.agent}`];
  const ms = $("model-select") as HTMLSelectElement;
  const es = $("effort-select") as HTMLSelectElement;
  const mf = $("model-free") as HTMLInputElement;
  if (!ms.hidden && ms.value === "__fusion") sub.push(s.model ?? "");
  else if (!ms.hidden && ms.value)
    sub.push(!es.hidden && es.value ? joinModel(ms.value, es.value) : ms.value);
  else if (!mf.hidden && mf.value) sub.push(mf.value);
  else if (s.model) sub.push(s.model);
  const ctx = $("chat-ctx");
  if (!ctx.hidden && ctx.textContent) sub.push(ctx.textContent);
  if (st === "dead" || st === "failed" || st === "interrupted") {
    const badge = $("chat-state");
    if (badge.textContent) sub.push(badge.textContent);
  }
  const subEl = $("m-sbar-sub");
  // Chevron = affordance « tap → feuille » (mockup : sous-titre ⌄).
  const html = `<span class="m-sub-t">${esc(sub.join(" · "))}</span>${icon("chev-down")}`;
  if (subEl.innerHTML !== html) subEl.innerHTML = html;
  $("m-sbar-stop").hidden = ($("btn-cancel") as HTMLElement).hidden;
  refreshSessionSheet();
}
