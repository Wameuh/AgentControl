// Accueil mobile « par état » (N3) : contrôle segmenté État | Groupes au
// sommet, regroupement des sessions par rôle de gemme, puce de flotte par
// section non vide, et cartes « À traiter » répondables sans ouvrir la
// session. Le mode « Groupes » rend l'arbre #session-list inchangé.

import { $, esc, showErr } from "../core/dom";
import { isAndroid } from "../core/chrome";
import { t } from "../i18n";
import { gemSVG } from "../crystal";
import { icon } from "../ui/icons";
import { dotRole } from "../features/status";
import { parseAnswer } from "../features/event-renderers";
import type { SessionMeta } from "../core/types";
import type { Sidebar } from "../features/sidebar";
import { findPending, respondPending, sortPermOptions, type PendingReq } from "./pending";
import { hintOnce } from "./hints";

const PENDING_TTL = 30_000;

/** Sections dans l'ordre d'affichage — clé i18n du libellé + rôles de
 *  gemme couverts (`DOT_ROLE[dotKey]`). */
const SECTIONS: { id: string; label: Parameters<typeof t>[0]; roles: string[] }[] = [
  { id: "todo", label: "home_todo", roles: ["inference"] },
  { id: "active", label: "home_active", roles: ["human", "agent"] },
  { id: "wait", label: "home_waiting", roles: ["wait"] },
  { id: "idle", label: "home_idle", roles: ["neutral", "slate"] },
];

let sb: Sidebar;
/** « État » par défaut : l'accueil s'ouvre sur ce qui attend
 *  l'utilisateur (N3). « Groupes » n'est retenu que s'il a été choisi. */
let mode: "state" | "group" = "state";
let agents = true;
let lastList: SessionMeta[] = [];
/** Cache des demandes par conn:sid — invalidé quand l'état change ou
 *  au-delà de 30 s (un `refresh()` post-réponse ne refait pas de RPC). */
const reqCache = new Map<string, { at: number; st: string; reqs: PendingReq[] }>();

export function initInbox(sidebar: Sidebar) {
  if (!isAndroid()) return;
  sb = sidebar;
  try {
    if (localStorage.getItem("ac-home-mode") === "group") mode = "group";
  } catch { /* stockage indisponible : mode par défaut */ }

  // Contrôle segmenté — au-dessus des deux listes pour rester visible
  // quel que soit le mode (sinon pas de retour possible depuis Groupes).
  const seg = document.createElement("div");
  seg.id = "m-home-seg";
  seg.innerHTML =
    `<button class="mh-seg" data-m="state">${esc(t("home_by_state"))}</button>` +
    `<button class="mh-seg" data-m="group">${esc(t("home_by_group"))}</button>`;
  $("m-home-state").before(seg);
  seg.querySelectorAll<HTMLButtonElement>(".mh-seg").forEach((b) => {
    b.classList.toggle("on", b.dataset.m === mode);
    b.onclick = () => {
      mode = b.dataset.m as typeof mode;
      seg.querySelectorAll(".mh-seg").forEach((x) =>
        x.classList.toggle("on", x === b));
      try {
        localStorage.setItem("ac-home-mode", mode);
      } catch { /* idem */ }
      sync();
    };
  });

  sb.onListRendered = (list) => {
    lastList = list;
    if (mode === "state" && agents) render(lastList);
    hintRowActions();
  };
  sb.onViewChanged = (a) => {
    agents = a;
    sync();
  };
  sync();
}

/** Indice de découverte (N4, une seule fois) : les actions d'une ligne —
 *  suppression comprise — vivent dans l'appui long. Montré dès que
 *  l'accueil Agents est affiché avec des sessions (il l'était à
 *  l'ouverture du drawer, disparu avec la phase 3). Appelé à chaque rendu
 *  de la liste ET à chaque retour à l'accueil (shell.ts) : la liste peut
 *  être rendue sous un overlay (vue connexion) puis ne plus changer. */
export function hintRowActions() {
  if (lastList.length && agents && document.body.classList.contains("m-home"))
    hintOnce("longpress", t("hint_longpress"));
}

/** Visibilités croisées : la liste classique et la vue par état ne
 *  cohabitent jamais ; hors onglet Agents, rien de tout cela n'existe. */
function sync() {
  const box = $("m-home-state");
  const seg = $("m-home-seg");
  const stateOn = agents && mode === "state";
  if (!stateOn) box.before(seg); // rangée autonome, à droite de l'accueil
  seg.hidden = !agents;
  box.hidden = !stateOn;
  // sidebar.ts pose déjà `hidden = v !== "agents"` sur #session-list
  // avant de notifier onViewChanged — l'override « mode État » est
  // donc toujours appliqué après lui.
  $("session-list").hidden = !agents || mode === "state";
  if (stateOn) render(lastList);
  // Contenu vidé quand caché : les .sess des cartes ne doivent pas
  // être matchées par les sélecteurs globaux (audit, appui long…).
  else box.innerHTML = "";
}

function render(list: SessionMeta[]) {
  const box = $("m-home-state");
  // Le segmenté vit dans l'en-tête de la 1re section en mode État
  // (ligne « À TRAITER · État|Groupes » de la maquette) — on le détache
  // avant le vidage pour conserver ses handlers.
  const seg = $("m-home-seg");
  seg.remove();
  box.innerHTML = "";
  const byRole = new Map<string, SessionMeta[]>();
  for (const s of list) {
    const r = dotRole(s.state ?? "", s.last_error);
    if (!byRole.has(r)) byRole.set(r, []);
    byRole.get(r)!.push(s);
  }
  const secs = SECTIONS
    .map((sec) => ({
      ...sec,
      list: sec.roles
        .flatMap((r) => byRole.get(r) ?? [])
        .sort((a, b) => a.title.localeCompare(b.title)),
    }))
    .filter((x) => x.list.length > 0);
  // Flotte : une puce par section non vide — tap = défilement vers elle.
  if (secs.length > 1) {
    const fleet = document.createElement("div");
    fleet.className = "mh-fleet";
    for (const x of secs) {
      const chip = document.createElement("button");
      chip.className = "mh-chip";
      // « À traiter » : le libellé embarque déjà le compte ({n} waiting) ;
      // les autres puces = compte + libellé de section.
      chip.innerHTML = x.id === "todo"
        ? `${gemSVG(x.roles[0])} ${esc(t("home_fleet_todo", { n: String(x.list.length) }))}`
        : `${gemSVG(x.roles[0])}<b>${x.list.length}</b> ${esc(t(x.label))}`;
      chip.onclick = () => box
        .querySelector(`#mh-sec-${x.id}`)
        ?.scrollIntoView({ behavior: "smooth", block: "start" });
      fleet.appendChild(chip);
    }
    box.appendChild(fleet);
  }
  for (const x of secs) {
    const sec = document.createElement("section");
    sec.id = `mh-sec-${x.id}`;
    sec.className = "mh-sec";
    sec.innerHTML = `<div class="mh-sec-t"><span>${esc(t(x.label))}</span></div>`;
    for (const s of x.list) {
      sec.appendChild(x.id === "todo" ? todoCard(s) : sb.buildRow(s));
    }
    box.appendChild(sec);
  }
  // Segmenté à droite de l'en-tête de la 1re section (ou seul si vide).
  const head = box.querySelector(".mh-sec-t");
  if (head) head.appendChild(seg);
  else box.prepend(seg);
}

/** Carte « À traiter » : la ligne session (mêmes clic/appui long/gemme)
 *  coiffée du corps de la demande ou de l'erreur. */
function todoCard(s: SessionMeta): HTMLElement {
  const card = document.createElement("div");
  card.className = "mh-card";
  const row = sb.buildRow(s);
  card.appendChild(row);
  const body = document.createElement("div");
  body.className = "mh-card-body";
  card.appendChild(body);
  if (s.state === "waiting_input") {
    // Différé d'une microtâche : la carte n'est insérée dans #m-home-state
    // qu'à la fin de render(). Sur un succès de cache, fillPending ne fait
    // aucun await et verrait un body détaché (isConnected faux) — la
    // carte resterait vide jusqu'au prochain rendu.
    queueMicrotask(() => void fillPending(body, s, row));
  } else {
    // failed / error hors rate_limit : message tronqué + Ouvrir.
    body.innerHTML =
      `<div class="mh-card-t mh-err">${esc(s.last_error?.message ?? "")}</div>` +
      `<div class="mh-card-acts"><button class="btn mh-open">${esc(t("home_open"))}</button></div>`;
    body.querySelector<HTMLButtonElement>(".mh-open")!.onclick =
      () => row.click();
  }
  return card;
}

async function fillPending(body: HTMLElement, s: SessionMeta, row: HTMLElement) {
  const key = `${s.conn ?? "default"}:${s.id}`;
  const c = reqCache.get(key);
  let reqs: PendingReq[];
  if (c && c.st === s.state && Date.now() - c.at < PENDING_TTL) {
    reqs = c.reqs;
  } else {
    reqs = await findPending(s).catch(() => [] as PendingReq[]);
    // Horodaté à la lecture seulement : un succès de cache ne prolonge
    // pas le TTL (sinon des re-rendus rapprochés ne relisent jamais).
    reqCache.set(key, { at: Date.now(), st: s.state ?? "", reqs });
  }
  // Carte remplacée par un re-rendu pendant la lecture : abandon.
  if (!body.isConnected) return;
  const req = reqs[0];
  if (!req) {
    // Aucune demande retrouvée : carte simple « Ouvrir ».
    body.innerHTML =
      `<div class="mh-card-acts"><button class="btn mh-open">${esc(t("home_open"))}</button></div>`;
    body.querySelector<HTMLButtonElement>(".mh-open")!.onclick =
      () => row.click();
    return;
  }
  if (req.type === "permission_request") {
    body.innerHTML =
      `<div class="mh-card-t">${icon("lock")} ${esc(req.title ?? "")}</div>` +
      (req.kind ? `<div class="mh-card-k">${esc(req.kind)}</div>` : "") +
      `<div class="mh-card-acts"></div>`;
    const acts = body.querySelector<HTMLElement>(".mh-card-acts")!;
    for (const o of sortPermOptions(req.options)) {
      const b = document.createElement("button");
      b.className = "btn " + (o.kind.startsWith("reject")
        ? "btn-reject"
        : o.kind === "allow_once" ? "btn-allow" : "");
      b.textContent = o.name;
      b.onclick = () => {
        b.disabled = true;
        void respondPending(s, req.request_id, { option_id: o.option_id })
          .then(() => sb.refresh())
          .catch((e) => showErr(String(e)));
      };
      acts.appendChild(b);
    }
    return;
  }
  // question_request : message + champ + « Répondre » — sauf formulaire
  // structuré (schema.properties), répondu dans la session (« Ouvrir »).
  const formSchema =
    req.schema && typeof req.schema === "object"
      ? (req.schema as { properties?: Record<string, unknown> }).properties
      : undefined;
  if (formSchema && Object.keys(formSchema).length > 0) {
    body.innerHTML =
      `<div class="mh-card-t">${esc(req.title ?? "")}</div>` +
      `<div class="mh-card-acts"><button class="btn mh-open">${esc(t("home_open"))}</button></div>`;
    body.querySelector<HTMLButtonElement>(".mh-open")!.onclick =
      () => row.click();
    return;
  }
  body.innerHTML =
    `<div class="mh-card-t">${esc(req.title ?? "")}</div>` +
    `<div class="mh-card-acts mh-q-row">` +
    `<input class="q-input mh-q" />` +
    `<button class="btn btn-allow">${esc(t("home_reply"))}</button></div>`;
  const inp = body.querySelector<HTMLInputElement>(".mh-q")!;
  body.querySelector<HTMLButtonElement>(".btn-allow")!.onclick = () => {
    void respondPending(s, req.request_id, { answer: parseAnswer(inp.value || null) })
      .then(() => sb.refresh())
      .catch((e) => showErr(String(e)));
  };
}
