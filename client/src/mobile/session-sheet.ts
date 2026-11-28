// ---------------------------------------------------------------------------
// Feuille « Session » (S2) : bottom-sheet des réglages et actions de la
// session ouverte — remplace le menu ⋮ du header masqué sous m-in-session.
// Les <select> réels vivent dans #chat-opts (câblés par modelbar.ts) : la
// feuille ne duplique pas la logique, elle reflète leur état et délègue —
// un choix = value + dispatch "change" vers le handler d'origine. Les
// entrées d'application vivent dans le ⋮ de l'accueil (phase 3).
// ---------------------------------------------------------------------------

import { $, esc, showErr } from "../core/dom";
import { isAndroid } from "../core/chrome";
import { app } from "../core/state";
import { t } from "../i18n";
import { call } from "../core/rpc";
import { connLabel } from "../core/conn";
import type { SessionMeta } from "../core/types";
import { dotIcon } from "../features/status";
import { icon } from "../ui/icons";
import type { SheetItem } from "../ui/nav";

export interface SessionSheetDeps {
  /** Recherche dans le journal (SessionSearch.open). */
  openSearch: () => void;
  /** « Ouvrir un chemin » (QuickOpen, mode path). */
  openPath: () => void;
  /** Terminal supporté par la conn de la session (TermPanel.supported). */
  termSupported: () => boolean;
  firstPostCapable: (conn: string) => boolean;
  pinCapable: (conn: string) => boolean;
  stepsCapable: (conn: string) => boolean;
  /** Titre renommé → resync barre session + liste sidebar. */
  onRenamed: () => void;
  /** kill_session appliqué → refresh de la liste (Sidebar.refresh). */
  refreshSidebar: () => void;
}

let deps: SessionSheetDeps;

export function initSessionSheet(d: SessionSheetDeps) {
  deps = d;
}

const dlg = () => $("dlg-session") as HTMLDialogElement;

/** Arme du bouton « Arrêter l'agent » : survit aux re-rendus live de la
 *  feuille (état session) — réarmé à chaque nouvelle ouverture. */
let killArmed = false;

/** Ouvre la feuille — rendue à l'ouverture puis rafraîchie par
 *  renderSessionBar tant qu'elle reste affichée. */
export function openSessionSheet() {
  if (!isAndroid() || !app.current || dlg().open) return;
  killArmed = false;
  renderSheet();
  dlg().showModal();
}

/** Re-rend la feuille si elle est ouverte (états/options/visibilités des
 *  actions suivent la meta live). Un champ en cours de saisie bloque le
 *  re-rendu — sinon le focus et le texte tapé seraient perdus. */
export function refreshSessionSheet() {
  if (!dlg().open) return;
  // Session disparue (kill depuis ailleurs, conn perdue) : une feuille
  // qui délègue à des contrôles d'une session morte n'a plus de sens.
  if (!app.current) { closeSheet(); return; }
  const ae = document.activeElement as HTMLElement | null;
  if (ae && dlg().contains(ae) && (ae.tagName === "INPUT" || ae.tagName === "TEXTAREA")) return;
  renderSheet();
}

function closeSheet() {
  if (dlg().open) dlg().close();
}

function renderSheet() {
  const s = app.current;
  if (!s) return;
  renderHead(s);
  renderOpts();
  renderActs(s);
}

// ── En-tête : gemme + titre renommable + sous-ligne agent·conn·cwd ───────

function renderHead(s: SessionMeta) {
  const head = $("ss-head");
  const spin = s.state === "working" || s.state === "compacting";
  head.innerHTML =
    `<span class="ss-gem${spin ? " spin" : ""}">${dotIcon(s.state, s.last_error)}</span>` +
    `<div id="ss-titles"><div id="ss-title-row">` +
    `<span id="ss-title">${esc(s.title)}</span>` +
    `<button id="ss-rename" type="button" title="${esc(t("ctx_rename"))}" aria-label="${esc(t("ctx_rename"))}">${icon("edit")}</button>` +
    `</div><div id="ss-sub">${esc(subLine(s))}</div></div>`;
  $("ss-rename").onclick = () => renameTitle(s);
}

function subLine(s: SessionMeta): string {
  const parts = [s.agent];
  if (s.conn) parts.push(connLabel(s.conn));
  if (s.cwd) parts.push(s.cwd);
  const ctx = $("chat-ctx");
  if (!ctx.hidden && ctx.textContent) parts.push(ctx.textContent);
  return parts.join(" · ");
}

/** Renommage inline du titre — même flux que le dblclick de #chat-title
 *  et la ligne sidebar : input → Enter/blur sauvegarde via
 *  rename_session, Escape annule. */
function renameTitle(s: SessionMeta) {
  const tl = $("ss-title");
  const inp = document.createElement("input");
  inp.id = "ss-title-edit";
  inp.value = s.title;
  tl.replaceWith(inp);
  inp.focus();
  inp.select();
  let finished = false;
  const done = (save: boolean) => {
    if (finished) return;
    finished = true;
    const v = inp.value.trim();
    inp.replaceWith(tl);
    if (!save || !v || v === s.title) return;
    void call("rename_session", { session: s.id, title: v, conn: s.conn })
      .then(() => {
        s.title = v;
        if (app.current?.id === s.id) app.current.title = v;
        deps.onRenamed();
        refreshSessionSheet();
      })
      .catch((e) => showErr(String(e)));
  };
  inp.addEventListener("keydown", (e) => {
    if (e.key === "Enter") done(true);
    if (e.key === "Escape") done(false);
  });
  inp.addEventListener("blur", () => done(true));
}

// ── Options : miroir des contrôles de #chat-opts, dans leur ordre ───────

const OPT_LABELS: Record<string, Parameters<typeof t>[0]> = {
  "mode-select": "ss_mode",
  "model-select": "ss_model",
  "effort-select": "ss_effort",
  "advisor-select": "ss_advisor",
};

function renderOpts() {
  const host = $("ss-opts");
  host.innerHTML = "";
  for (const el of $("chat-opts").children) {
    const h = el as HTMLElement;
    if (h.hidden) continue;
    if (h instanceof HTMLSelectElement) {
      host.appendChild(optSection(
        OPT_LABELS[h.id] ? t(OPT_LABELS[h.id]) : h.title || h.id,
        h, h.id === "model-select"));
    } else if (h.id === "model-free") {
      host.appendChild(freeSection(h as HTMLInputElement));
    } else if (h.id === "dyn-opts") {
      // Les selects enfants peuvent être masqués un à un (option sans
      // valeur pour l'agent courant) — même filtre que le parent.
      for (const sel of h.querySelectorAll<HTMLSelectElement>("select")) {
        if (sel.hidden) continue;
        host.appendChild(optSection(sel.title || sel.id, sel));
      }
    }
    // #btn-fusion-edit / #btn-caps-refresh : rendus dans la section Modèle.
  }
}

/** Libellé de section d'options ; `refresh` ajoute le bouton « re-sonder
 *  les caps » — pertinent seulement sur la section Modèle (il re-scrute
 *  modes/modèles, pas chaque select). */
function optLabel(labelText: string, refresh = false): HTMLElement {
  const lab = document.createElement("div");
  lab.className = "ss-label";
  const txt = document.createElement("span");
  txt.textContent = labelText;
  lab.appendChild(txt);
  const caps = $("btn-caps-refresh") as HTMLButtonElement;
  if (refresh && !caps.hidden) {
    const r = document.createElement("button");
    r.type = "button";
    r.className = "ss-refresh";
    r.title = caps.title;
    r.setAttribute("aria-label", caps.title);
    r.innerHTML = icon("sync");
    r.onclick = () => caps.click();
    lab.appendChild(r);
  }
  return lab;
}

/** Section d'un <select> : segmenté si ≤4 options courtes, sinon liste
 *  radio. Le choix délègue au select réel (value + "change"). */
function optSection(
  labelText: string,
  sel: HTMLSelectElement,
  refresh = false,
): HTMLElement {
  const sec = document.createElement("div");
  sec.className = "ss-sec";
  sec.appendChild(optLabel(labelText, refresh));
  const items = [...sel.options].map((o) => {
    const txt = o.textContent ?? o.value;
    const i = txt.indexOf(" · ");
    return { v: o.value, name: i < 0 ? txt : txt.slice(0, i), note: i < 0 ? "" : txt.slice(i + 3) };
  });
  const pick = (v: string) => {
    // Fusion : le dialogue s'ouvre par le handler du select — la feuille
    // doit être refermée AVANT (deux modales ne se recouvrent pas).
    if (v === "__fusion") closeSheet();
    sel.value = v;
    sel.dispatchEvent(new Event("change", { bubbles: true }));
    refreshSessionSheet();
  };
  const seg = items.length <= 4 && items.every((i) => i.name.length <= 12);
  const body = document.createElement("div");
  body.className = seg ? "ss-seg" : "ss-radios";
  for (const it of items) {
    const b = document.createElement("button");
    b.type = "button";
    if (seg) {
      b.className = "ss-seg-item" + (it.v === sel.value ? " on" : "");
      b.textContent = it.name;
    } else {
      b.className = "ss-radio" + (it.v === sel.value ? " on" : "");
      b.innerHTML = `<span class="ss-dot"></span><span class="ss-rn">${esc(it.name)}</span>` +
        (it.note ? `<span class="ss-note">${esc(it.note)}</span>` : "");
    }
    b.onclick = () => pick(it.v);
    body.appendChild(b);
  }
  sec.appendChild(body);
  // Fusion déjà active : l'option du select ne re-tire pas « change » —
  // ligne dédiée qui délègue au bouton ⚙ du header.
  const fusion = $("btn-fusion-edit") as HTMLButtonElement;
  if (sel.id === "model-select" && !fusion.hidden) {
    const f = document.createElement("button");
    f.type = "button";
    f.className = "ss-radio ss-fusion";
    f.innerHTML = `${icon("merge")}<span class="ss-rn">${esc(t("ss_fusion"))}</span>`;
    f.onclick = () => { closeSheet(); fusion.click(); };
    sec.appendChild(f);
  }
  return sec;
}

/** Section « modèle libre » (agent sans liste annoncée) : champ + Apply
 *  qui rejoue le keydown Enter du #model-free réel. */
function freeSection(mf: HTMLInputElement): HTMLElement {
  const sec = document.createElement("div");
  sec.className = "ss-sec";
  sec.appendChild(optLabel(t("ss_model"), true));
  const row = document.createElement("div");
  row.className = "ss-free";
  const inp = document.createElement("input");
  inp.value = mf.value;
  inp.placeholder = mf.placeholder;
  const go = document.createElement("button");
  go.type = "button";
  go.className = "ss-apply";
  go.textContent = t("ss_apply");
  const apply = () => {
    mf.value = inp.value.trim();
    mf.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter" }));
  };
  go.onclick = apply;
  inp.addEventListener("keydown", (e) => {
    if (e.key === "Enter") apply();
  });
  row.append(inp, go);
  sec.appendChild(row);
  return sec;
}

// ── Actions : grille 4 colonnes + ligne danger ───────────────────────────

/** Mêmes entrées et mêmes conditions que l'ancien menu ⋮ du chat. */
function actionItems(s: SessionMeta): SheetItem[] {
  const c = s.conn ?? "default";
  const items: SheetItem[] = [
    { icon: "search", label: t("search_title"), fn: () => deps.openSearch() },
  ];
  if (deps.firstPostCapable(c))
    items.push({ icon: "top", label: t("first_post"), fn: () => $("btn-first").click() });
  if (deps.pinCapable(c))
    items.push({ icon: "pin", label: t("pins_title"), fn: () => $("btn-pins").click() });
  if (deps.stepsCapable(c))
    items.push({ icon: "steps", label: t("steps_title"), fn: () => $("btn-steps").click() });
  if (deps.termSupported())
    items.push({ icon: "terminal", label: t("term_title"), fn: () => $("btn-term").click() });
  items.push({ icon: "folder", label: t("files_title"), fn: () => $("btn-files").click() });
  items.push({ icon: "search", label: t("qo_open_path"), fn: () => deps.openPath() });
  if (!app.alive && s.state !== "starting")
    items.push({ icon: "play", label: t("resume"), fn: () => $("btn-resume").click() });
  if (s.state === "starting" && s.native_session_id)
    items.push({ icon: "refresh", label: t("fresh_ctx"), fn: () => $("btn-fresh").click() });
  if (app.alive)
    items.push({ icon: "sleep", label: t("ctx_sleep"), fn: () => $("btn-sleep").click() });
  items.push({ icon: "refresh", label: t("refresh_title"), fn: () => $("btn-refresh").click() });
  return items;
}

function renderActs(s: SessionMeta) {
  const host = $("ss-acts");
  host.innerHTML = "";
  const lab = document.createElement("div");
  lab.className = "ss-label";
  lab.textContent = t("ss_actions");
  host.appendChild(lab);
  const grid = document.createElement("div");
  grid.className = "ss-grid";
  for (const it of actionItems(s)) {
    const b = document.createElement("button");
    b.type = "button";
    b.className = "ss-act";
    b.innerHTML = (it.icon ? icon(it.icon) : "") + `<span>${esc(it.label)}</span>`;
    b.onclick = () => { closeSheet(); it.fn(); };
    grid.appendChild(b);
  }
  host.appendChild(grid);
  // « Arrêter l'agent » — destructif → double-tap armé (même motif que
  // ctx_exit dans le menu session de la sidebar). L'arme vit dans un
  // flag module (killArmed) : un re-rendu live reconstruit ce bouton —
  // dataset.armed serait perdu et le premier tap devrait être refait.
  const kill = document.createElement("button");
  kill.type = "button";
  kill.className = "ss-danger";
  kill.innerHTML = `${icon("stop")}<span>${esc(killArmed ? t("ss_kill_confirm") : t("ctx_exit"))}</span>`;
  if (killArmed) kill.dataset.armed = "1";
  kill.onclick = () => {
    const lbl = kill.querySelector("span")!;
    if (!kill.dataset.armed) {
      killArmed = true;
      kill.dataset.armed = "1";
      lbl.textContent = t("ss_kill_confirm");
      return;
    }
    closeSheet();
    // Kill volontaire en starting : pas un échec de spawn à toaster.
    app.spawnWatch.delete(s.id);
    void call("kill_session", { session: s.id, conn: s.conn })
      .then(() => deps.refreshSidebar())
      .catch((e) => showErr(String(e)));
  };
  host.appendChild(kill);
}
