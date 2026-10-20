import "./styles.css";
import "./mesh-bg.ts";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { getVersion } from "@tauri-apps/api/app";
import { gemSVG } from "./crystal";
import { initLang, setLang, getLang, t, applyStatic, type Lang } from "./i18n";

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

// Thème + langue persistés (localStorage), appliqués au boot
initLang();
const mesh = (window as { MeshBackground?: { setMood: (m: string, o: object) => void } })
  .MeshBackground;
const bgMood = () => (document.documentElement.classList.contains("light") ? "paper" : "sand");

function applyTheme(name: "dark" | "light") {
  document.documentElement.classList.toggle("light", name === "light");
  localStorage.setItem("ac-theme", name);
  mesh?.setMood(bgMood(), { intensity: 0.55 });
}
applyTheme(localStorage.getItem("ac-theme") === "light" ? "light" : "dark");
applyStatic();

// Gemme cristal : verte (neutral) = identité app ; dorée (human) = travail
for (const id of ["titlebar-gem", "about-gem"]) {
  const el = document.getElementById(id);
  if (el) el.innerHTML = gemSVG("neutral");
}
const cg = document.getElementById("chat-gem");
if (cg) cg.innerHTML = gemSVG("human");

// Fenêtre custom (decorations: false) : drag natif via data-tauri-drag-region
const appWin = getCurrentWindow();
$("tb-min").onclick = () => appWin.minimize();
$("tb-max").onclick = () => appWin.toggleMaximize();
$("tb-close").onclick = () => appWin.close();
$("titlebar").addEventListener("dblclick", (e) => {
  if (!(e.target as HTMLElement).closest("button")) appWin.toggleMaximize();
});
// Drag fenêtre explicite (plus fiable que data-tauri-drag-region seul)
$("titlebar").addEventListener("mousedown", (e) => {
  if (e.button === 0 && !(e.target as HTMLElement).closest("button"))
    void appWin.startDragging();
});
document.querySelectorAll<HTMLElement>(".resize-edge").forEach((el) => {
  el.addEventListener("mousedown", (e) => {
    e.preventDefault();
    void appWin.startResizeDragging(el.dataset.dir as never);
  });
});

// Settings : thème + langue
$("tb-settings").onclick = () => {
  ($("s-theme") as HTMLSelectElement).value =
    document.documentElement.classList.contains("light") ? "light" : "dark";
  ($("s-lang") as HTMLSelectElement).value = getLang();
  ($("dlg-settings") as HTMLDialogElement).showModal();
};
($("s-theme") as HTMLSelectElement).onchange = (e) =>
  applyTheme((e.target as HTMLSelectElement).value as "dark" | "light");
($("s-lang") as HTMLSelectElement).onchange = (e) => {
  setLang((e.target as HTMLSelectElement).value as Lang);
  refreshSessions();
  updateHeader();
};

// About : version réelle + lien GitHub via le plugin shell
$("tb-about").onclick = async () => {
  try {
    $("about-version").textContent = "v" + (await getVersion());
  } catch {
    /* dev */
  }
  ($("dlg-about") as HTMLDialogElement).showModal();
};
$("about-gh").addEventListener("click", (e) => {
  e.preventDefault();
  invoke("plugin:shell|open", { path: "https://github.com/Wameuh/AgentControl" }).catch(() => {});
});

// Check update au boot : version distante (gh release ou package.json sur
// main) > locale → chip "⬆ vX" dans la titlebar → clic = update (deb+dpkg)
// ou ouverture de la page releases si aucun asset.
function cmpVer(a: string, b: string): number {
  const pa = a.split(".").map(Number);
  const pb = b.split(".").map(Number);
  for (let i = 0; i < 3; i++) {
    const d = (pa[i] || 0) - (pb[i] || 0);
    if (d) return d;
  }
  return 0;
}
(async () => {
  try {
    const remote = await invoke<string>("check_update");
    const local = await getVersion().catch(() => "0.0.0");
    if (cmpVer(remote, local) <= 0) return;
    const b = document.createElement("button");
    b.className = "tb-update";
    b.textContent = `⬆ v${remote}`;
    b.title = `Update v${remote} available — click to install`;
    b.onclick = () => {
      b.textContent = "…";
      invoke("update_client", { tag: remote }).catch(() => {
        // Pas d'asset .deb → ouvrir la page releases à la place
        invoke("plugin:shell|open", {
          path: "https://github.com/Wameuh/AgentControl/releases",
        }).catch(() => {});
        b.textContent = `⬆ v${remote}`;
      });
    };
    $("titlebar").insertBefore(b, document.querySelector(".tb-btns"));
  } catch {
    /* pas de gh/réseau → tant pis */
  }
})();

// Bloque le menu contextuel natif du webview (son "Reload" tuait la connexion)
window.addEventListener("contextmenu", (e) => e.preventDefault());

// ---------------------------------------------------------------------------
// Types miroir du protocole
// ---------------------------------------------------------------------------

type CmdInfo = string | { name: string; description?: string; hint?: string };
const cmdName = (c: CmdInfo) => (typeof c === "string" ? c.replace(/^\//, "") : c.name);
const cmdDesc = (c: CmdInfo) => (typeof c === "string" ? "" : (c.description ?? ""));
const cmdHint = (c: CmdInfo) => (typeof c === "string" ? "" : (c.hint ?? ""));

interface SessionMeta {
  id: string;
  agent: string;
  title: string;
  cwd: string;
  created_at: string;
  native_session_id?: string;
  mode?: string;
  model?: string;
  available_modes: string[];
  available_commands: CmdInfo[];
  available_models: string[];
  config_options?: ConfigOption[];
  state: string;
}

interface ConfigOpt { value: string; name: string; description?: string }
interface ConfigOption {
  id: string; name: string; category?: string;
  currentValue?: string; options: ConfigOpt[];
}

// Devin : les modèles portent l'effort en suffixe (-medium/-high/...)
const EFF_RE = /-(low|minimal|medium|high|max|lightning)$/;
const splitModel = (v: string): [string, string] => {
  const m = EFF_RE.exec(v);
  return m ? [v.slice(0, -m[0].length), m[1]] : [v, ""];
};

/** Décompose "fusion-<prim>-<eff>-sidekick-<side>-<eff>" → [pb, pe, sb, se]. */
function fusionParts(v: string): [string, string, string, string] | null {
  if (!v.startsWith("fusion-")) return null;
  const rest = v.slice("fusion-".length);
  const i = rest.indexOf("-sidekick-");
  if (i < 0) return null;
  const [pb, pe] = splitModel(rest.slice(0, i));
  const [sb, se] = splitModel(rest.slice(i + "-sidekick-".length));
  return [pb, pe, sb, se];
}

interface AcEvent {
  seq: number;
  ts: string;
  type: string;
  [k: string]: unknown;
}

// ---------------------------------------------------------------------------
// État UI
// ---------------------------------------------------------------------------

let current: SessionMeta | null = null;
let minRendered = Number.MAX_SAFE_INTEGER;
let renderedCount = 0;
const MAX_RENDERED = 400;
const PAGE = 50; // paquets au scroll
const INITIAL_PAGE = 200; // base d'historique à l'ouverture
const toolBlocks = new Map<string, HTMLElement>();
const answered = new Set<string>();
let loadingOld = false;
let hasMoreHistory = true;
let alive = false;

const eventsEl = $("events");
const sentinel = $("scroll-sentinel");

// Redimensionnement de la sidebar : drag sur la poignée bord droit.
// Largeur persistée dans localStorage.
{
  const bar = $("sidebar");
  const grip = $("sidebar-resize");
  const clamp = (x: number) => Math.min(520, Math.max(180, x));
  const saved = Number(localStorage.getItem("sidebar-w"));
  if (saved) {
    bar.style.width = clamp(saved) + "px";
    bar.style.minWidth = clamp(saved) + "px";
  }
  grip.addEventListener("mousedown", (e) => {
    e.preventDefault();
    grip.classList.add("drag");
    document.body.style.userSelect = "none";
    const move = (ev: MouseEvent) => {
      const w = clamp(ev.clientX);
      bar.style.width = w + "px";
      bar.style.minWidth = w + "px";
    };
    const up = (ev: MouseEvent) => {
      document.removeEventListener("mousemove", move);
      document.removeEventListener("mouseup", up);
      grip.classList.remove("drag");
      document.body.style.userSelect = "";
      localStorage.setItem("sidebar-w", String(clamp(ev.clientX)));
    };
    document.addEventListener("mousemove", move);
    document.addEventListener("mouseup", up);
  });
}

/** Erreur visible et copiable (remplace alert). */
function showErr(msg: string) {
  const bar = $("err-bar");
  $("err-text").textContent = msg;
  bar.hidden = false;
}
$("err-close").addEventListener("click", () => ($("err-bar").hidden = true));

async function call<T = unknown>(method: string, params: Record<string, unknown> = {}): Promise<T> {
  return invoke<T>("call", { method, params });
}

// ---------------------------------------------------------------------------
// Connexion
// ---------------------------------------------------------------------------

async function doConnect(cfg: Record<string, unknown>) {
  const status = await call("daemon_status").catch(async () => {
    const r = await invoke("connect", { cfg });
    return r;
  });
  const el = $("conn-status");
  el.textContent = t("connected_n", { n: (status as { sessions?: number }).sessions ?? 0 });
  el.className = "badge ok";
  await refreshSessions();
}

// ---------------------------------------------------------------------------
// Liste des sessions
// ---------------------------------------------------------------------------

/** Gemme d'état : dorée working, rouge question/failed, verte idle, grise dead. */
const DOT_ROLE: Record<string, string> = {
  working: "human", running: "human", starting: "human",
  waiting_input: "inference", failed: "inference",
  idle: "neutral",
  interrupted: "slate", dead: "slate",
};
function dotIcon(state: string): string {
  return gemSVG(DOT_ROLE[state] ?? "slate");
}

async function refreshSessions() {
  const list = await call<SessionMeta[]>("list_sessions");
  // Lumières "agents" du fond : une par session active
  const n = list.filter((s) => s.state === "working" || s.state === "waiting_input").length;
  (window as { MeshBackground?: { setMood: (m: string, o: object) => void } })
    .MeshBackground?.setMood(bgMood(), { agents: Math.min(n, 4), intensity: 0.55 });
  const el = $("session-list");
  el.innerHTML = "";
  for (const s of list) {
    const div = document.createElement("div");
    div.className = "sess" + (current?.id === s.id ? " active" : "");
    div.innerHTML = `<span class="dot st-${s.state}" title="${esc(s.state)}">${dotIcon(s.state)}</span><span class="t">${esc(s.title)}</span><span class="a">${esc(s.agent)}</span><button class="sess-del" title="Supprimer">✕</button>`;
    div.onclick = () => openSession(s.id);
    const del = div.querySelector<HTMLElement>(".sess-del")!;
    del.addEventListener("click", async (e) => {
      e.stopPropagation();
      if (!del.dataset.armed) {
        del.dataset.armed = "1";
        del.textContent = t("delete_armed");
        setTimeout(() => { del.dataset.armed = ""; del.textContent = "✕"; }, 2500);
        return;
      }
      await call("delete_session", { session: s.id }).catch((er) => showErr(String(er)));
      if (current?.id === s.id) {
        current = null;
        eventsEl.innerHTML = "";
      }
      refreshSessions();
    });
    el.appendChild(div);
  }
}

// ---------------------------------------------------------------------------
// Ouverture d'une session : session_info + dernière page + subscribe
// ---------------------------------------------------------------------------

async function openSession(id: string) {
  const info = await call<{ meta: SessionMeta; pending: string[]; alive: boolean; last_seq: number }>(
    "session_info",
    { session: id },
  );
  alive = info.alive;
  current = info.meta;
  minRendered = Number.MAX_SAFE_INTEGER;
  renderedCount = 0;
  hasMoreHistory = true;
  toolBlocks.clear();
  answered.clear();
  for (const c of eventsEl.querySelectorAll(".ev")) c.remove();

  $("chat-title").textContent = `${current.title} — ${current.agent}`;
  updateHeader();

  // Auto-resume : agent mort (ex. restart démon) → reprise silencieuse
  // plutôt qu'un "interrupted" qui demande un clic.
  if (!alive && current.native_session_id) {
    const sys = document.createElement("div");
    sys.className = "sysline";
    sys.textContent = "⏳ reprise de l'agent…";
    eventsEl.appendChild(sys);
    try {
      await call("resume_session", { session: id });
      const info2 = await call<{ meta: SessionMeta; alive: boolean }>("session_info", {
        session: id,
      });
      alive = info2.alive;
      current = info2.meta;
      sys.textContent = "✔ agent repris";
      updateHeader();
    } catch (e) {
      sys.textContent = `⚠ reprise: ${e}`;
    }
  }

  // Charge la dernière page d'événements — un échec (journal absent/corrompu)
  // ne doit PAS tuer le subscribe ni la vue live derrière.
  let page = { events: [] as AcEvent[], last_seq: 0 };
  try {
    page = await call<{ events: AcEvent[]; last_seq: number }>("events", {
      session: id,
      limit: INITIAL_PAGE,
    });
  } catch (e) {
    showErr(`events: ${e}`);
  }
  for (const ev of page.events) appendEvent(ev, false);
  scrollBottom();

  // Subscribe live à partir du dernier event connu
  const afterSeq = page.events.length ? page.events[page.events.length - 1].seq : 0;
  await invoke("subscribe", { session: id, afterSeq }).catch((e) => {
    // Le flux live est mort (ex. restart démon) → statut déconnecté visible
    const el = $("conn-status");
    el.textContent = t("disconnected_reconnect");
    el.className = "badge st-dead";
    showErr(`subscribe: ${e}`);
  });
  refreshSessions();
}

function updateHeader() {
  if (!current) return;
  const st = $("chat-state");
  st.textContent = current.state;
  st.className = `badge st-${current.state}`;
  $("chat-gem").hidden = current.state !== "working";
  // Reprendre = process mort, quel que soit l'état persisté dans meta.json
  $("btn-resume").hidden = alive;
  $("btn-cancel").hidden = !(current.state === "working" || current.state === "waiting_input");

  // Options de config dynamiques (ex. Devin : config_option_update → mode + model)
  const copts = current.config_options ?? [];
  const modeOpt = copts.find((o) => o.id === "mode" || o.category === "mode");
  const modelOpt = copts.find((o) => o.id === "model" || o.category === "model");

  const ms = $("mode-select") as HTMLSelectElement;
  if (modeOpt || current.available_modes.length) {
    ms.hidden = false;
    ms.innerHTML = "";
    const items = modeOpt
      ? modeOpt.options.map((o) => ({ v: o.value, label: o.name || o.value }))
      : current.available_modes.map((m) => ({ v: m, label: m }));
    for (const it of items) {
      const o = document.createElement("option");
      o.value = it.v;
      o.textContent = it.label;
      ms.appendChild(o);
    }
    ms.dataset.cfgid = modeOpt ? modeOpt.id : "";
    ms.value = modeOpt?.currentValue || current.mode || "";
  } else {
    ms.hidden = true;
  }

  const md = $("model-select") as HTMLSelectElement;
  const es = $("effort-select") as HTMLSelectElement;
  const mf = $("model-free") as HTMLInputElement;
  const rawOpts = modelOpt
    ? modelOpt.options.map((o) => ({ v: o.value, label: o.name || o.value }))
    : current.available_models.map((m) => ({ v: m, label: m }));
  if (rawOpts.length) {
    md.hidden = false;
    mf.hidden = true;
    // Modèles de base dédupliqués (effort géré à part) + entrée Fusion
    const bases = new Map<string, string>();
    for (const o of rawOpts) {
      if (o.v.startsWith("fusion-")) continue;
      const [b] = splitModel(o.v);
      if (!bases.has(b)) bases.set(b, o.label);
    }
    md.innerHTML = "";
    for (const [b, label] of bases) {
      const o = document.createElement("option");
      o.value = b;
      o.textContent = "🧠 " + label;
      md.appendChild(o);
    }
    const fo = document.createElement("option");
    fo.value = "__fusion";
    fo.textContent = t("fusion_entry");
    md.appendChild(fo);
    md.dataset.cfgid = modelOpt ? modelOpt.id : "";
    md.dataset.opts = JSON.stringify(rawOpts);
    const curVal = modelOpt?.currentValue || current.model || "";
    const curIsFusion = curVal.startsWith("fusion-");
    if (curIsFusion) {
      const fp = rawOpts.find((o) => o.v === curVal);
      if (fp) fo.textContent = `✨ ${fp.label}`;
    }
    const [curBase, curEff] = splitModel(curVal);
    md.value = curIsFusion ? "__fusion" : curBase;
    // Bouton ⚙ : reconfigurer la fusion quand elle est déjà active
    // (re-sélectionner la même option dans un <select> ne tire pas change)
    $("btn-fusion-edit").hidden = !curIsFusion;
    // Select d'effort : suffixes réellement présents pour la base choisie
    // (masqué en fusion — les efforts se choisissent dans le dialogue)
    fillEfforts(md.value, curEff);
    es.hidden = curIsFusion;
  } else {
    md.hidden = true;
    es.hidden = true;
    $("btn-fusion-edit").hidden = true;
    mf.hidden = false;
    mf.value = current.model ?? "";
  }

}

// ---------------------------------------------------------------------------
// Rendu des événements
// ---------------------------------------------------------------------------

function esc(s: string): string {
  const d = document.createElement("div");
  d.textContent = s;
  return d.innerHTML;
}

// Chemins de fichiers cliquables → panneau latéral (read_file côté démon)
const FILE_RE =
  /((?:\/[\w.\-~]+){2,}|[\w.\-]+\/[\w./\-]+|[\w.\-]+\.(?:rs|ts|tsx|js|jsx|py|toml|json|md|html?|css|ya?ml|sh|c|cpp|h|hpp|go|java|rb|txt|log|svg|cfg|ini|sql|vue|svelte|lock|service))(?::(\d+))?/g;

function linkify(escaped: string): string {
  return escaped.replace(FILE_RE, (m, p: string) => {
    const clean = p.replace(/[.,;:!?'")\]]+$/, "");
    // lien = chemin sans ponctuation finale ; le reste (:ligne, ponct.) reste dehors
    return `<span class="flink" data-path="${clean}">${clean}</span>${m.slice(clean.length)}`;
  });
}

/** Colorise les lignes de diff (+add / -del / @@ hunks) dans le texte. */
function diffFmt(escaped: string): string {
  if (!/^(?:\+{3}|-{3}|@@|[-+]\S)/m.test(escaped)) return escaped;
  return escaped
    .split("\n")
    .map((l) => {
      const cls =
        l.startsWith("@@") || l.startsWith("+++") || l.startsWith("---")
          ? "df-hd"
          : l.startsWith("+")
            ? "df-add"
            : l.startsWith("-")
              ? "df-del"
              : "";
      return cls ? `<span class="${cls}">${l}</span>` : l;
    })
    .join("\n");
}

async function openFilePanel(path: string) {
  if (!current) return;
  $("file-panel").hidden = false;
  $("file-path").textContent = path;
  $("file-path").title = path;
  $("file-view").textContent = "…";
  try {
    const r = await call<{ content: string; truncated: boolean }>("read_file", {
      session: current.id,
      path,
    });
    $("file-view").textContent = r.content + (r.truncated ? "\n… (tronqué)" : "");
  } catch (e) {
    $("file-view").textContent = String(e);
  }
}
$("file-close").onclick = () => ($("file-panel").hidden = true);
eventsEl.addEventListener("click", (e) => {
  const el = (e.target as HTMLElement).closest<HTMLElement>(".flink");
  if (el?.dataset.path) void openFilePanel(el.dataset.path);
});

function mkEv(cls: string, seq: number): HTMLElement {
  const d = document.createElement("div");
  d.className = `ev ${cls}`;
  d.dataset.seq = String(seq);
  return d;
}

function appendEvent(ev: AcEvent, live: boolean) {
  // tool_call_update modifie le bloc existant
  if (ev.type === "tool_call_update") {
    updateToolBlock(ev);
    return;
  }
  const nearBottom =
    eventsEl.scrollHeight - eventsEl.scrollTop - eventsEl.clientHeight < 120;
  const node = renderEvent(ev, live);
  eventsEl.appendChild(node);
  renderedCount++;
  if (ev.seq < minRendered) minRendered = ev.seq;
  if (ev.type === "tool_call") {
    toolBlocks.set(ev.call_id as string, node);
  }
  if (live && nearBottom) scrollBottom();
  trimRendered();
}

/** Supprime les plus anciens events rendus au-delà du cap (buffer). */
function trimRendered() {
  while (renderedCount > MAX_RENDERED) {
    const first = eventsEl.querySelector<HTMLElement>(".ev");
    if (!first) break;
    // retirer aussi son bloc de la map si tool_call
    for (const [k, v] of toolBlocks) if (v === first) toolBlocks.delete(k);
    first.remove();
    renderedCount--;
    minRendered = Number(
      eventsEl.querySelector<HTMLElement>(".ev")?.dataset.seq ?? Number.MAX_SAFE_INTEGER,
    );
  }
}

function prependEvents(events: AcEvent[]) {
  if (!events.length) {
    hasMoreHistory = false;
    return;
  }
  const oldHeight = eventsEl.scrollHeight;
  for (const ev of events) {
    const node = renderEvent(ev, false);
    eventsEl.insertBefore(node, eventsEl.children[1]); // après sentinel
    renderedCount++;
    if (ev.seq < minRendered) minRendered = ev.seq;
    if (ev.type === "tool_call") toolBlocks.set(ev.call_id as string, node);
    if (ev.type === "tool_call_update") updateToolBlock(ev);
  }
  // garder la position visuelle
  eventsEl.scrollTop += eventsEl.scrollHeight - oldHeight;
}

function renderEvent(ev: AcEvent, live: boolean): HTMLElement {
  switch (ev.type) {
    case "user_message": {
      const d = mkEv("msg user", ev.seq);
      d.innerHTML = linkify(esc(ev.text as string));
      return d;
    }
    case "agent_text": {
      const d = mkEv("msg agent", ev.seq);
      d.innerHTML = diffFmt(linkify(esc(ev.text as string)));
      return d;
    }
    case "agent_thought": {
      const d = mkEv("thought", ev.seq);
      const txt = ev.text as string;
      const head = txt.trim().split("\n")[0].slice(0, 110);
      d.innerHTML = `<details class="th"><summary>💭 ${esc(head)}</summary><pre>${diffFmt(esc(txt))}</pre></details>`;
      return d;
    }
    case "tool_call":
      return renderToolCall(ev);
    case "permission_request":
      return renderPermission(ev);
    case "question_request":
      return renderQuestion(ev);
    case "permission_resolved":
    case "question_resolved": {
      const d = mkEv("resolved", ev.seq);
      const rid = ev.request_id as string;
      answered.add(rid);
      d.textContent = t("resolved") + ((ev.outcome ?? ev.answer) as string);
      const card = eventsEl.querySelector(`[data-req="${rid}"]`);
      if (card) card.classList.add("done");
      return d;
    }
    case "mode_changed": {
      const d = mkEv("sysline", ev.seq);
      d.textContent = t("mode_arrow") + (ev.mode as string);
      if (current) {
        current.mode = ev.mode as string;
        updateHeader();
      }
      return d;
    }
    case "config_options": {
      if (current) {
        current.config_options = ev.options as ConfigOption[];
        const mo = current.config_options.find((o) => o.id === "model" || o.category === "model");
        if (mo) current.model = mo.currentValue;
        const md = current.config_options.find((o) => o.id === "mode" || o.category === "mode");
        if (md) current.mode = md.currentValue;
        updateHeader();
      }
      const d = mkEv("sysline", ev.seq);
      d.hidden = true;
      return d;
    }
    case "commands_available": {
      const d = mkEv("sysline", ev.seq);
      const cmds = ev.commands as CmdInfo[];
      d.textContent = t("cmds_avail", { n: cmds.length });
      if (current) {
        current.available_commands = cmds;
        updateHeader();
      }
      return d;
    }
    case "status_changed": {
      const d = mkEv("sysline", ev.seq);
      d.textContent = t("state_arrow") + (ev.state as string);
      // Seul un event LIVE met à jour l'état — rejouer l'historique du
      // journal écraserait alive/meta avec un état périmé ("working"
      // d'une session morte → bouton Reprendre masqué).
      if (live && current) {
        current.state = ev.state as string;
        alive = ev.state !== "dead";
        updateHeader();
        refreshSessions();
      }
      return d;
    }
    case "error": {
      const d = mkEv("errline", ev.seq);
      d.textContent = `⚠ ${ev.message}`;
      return d;
    }
    case "system": {
      const d = mkEv("sysline", ev.seq);
      d.textContent = `⦿ ${ev.text}`;
      return d;
    }
    default: {
      const d = mkEv("sysline", ev.seq);
      d.textContent = ev.type;
      return d;
    }
  }
}

// ---------------------------------------------------------------------------
// Tool calls : bloc pliable + expand blob
// ---------------------------------------------------------------------------

function renderToolCall(ev: AcEvent): HTMLElement {
  const d = mkEv("tool", ev.seq);
  const status = ev.status as string;
  d.dataset.callId = ev.call_id as string;
  d.innerHTML = `
    <div class="tool-head">
      <span class="tool-ico">⚙</span>
      <span class="tool-title">${esc(ev.title as string)}</span>
      <span class="badge st-${status}">${status}</span>
    </div>
    <div class="tool-body" hidden>
      <pre class="tool-out"></pre>
      ${ev.blob ? '<button class="btn-more">charger la sortie complète…</button>' : ""}
      ${ev.diff ? `<pre class="tool-diff">${esc(ev.diff as string)}</pre>` : ""}
    </div>`;
  const body = d.querySelector<HTMLElement>(".tool-body")!;
  const pre = d.querySelector<HTMLElement>(".tool-out")!;
  if (ev.excerpt) pre.textContent = ev.excerpt as string;
  d.querySelector(".tool-head")!.addEventListener("click", () => {
    body.hidden = !body.hidden;
    d.classList.toggle("open", !body.hidden);
  });
  const btn = d.querySelector<HTMLButtonElement>(".btn-more");
  if (btn && ev.blob) {
    btn.addEventListener("click", async () => {
      btn.disabled = true;
      btn.textContent = t("loading");
      const r = await call<{ data: string; total: number }>("get_blob", {
        session: current!.id,
        blob: ev.blob,
        offset: 0,
        limit: 1 << 20,
      });
      pre.textContent = r.data + (r.total > r.data.length ? `\n… (${r.total} o total)` : "");
      btn.remove();
    });
  }
  return d;
}

function updateToolBlock(ev: AcEvent) {
  const d = toolBlocks.get(ev.call_id as string);
  if (!d) return;
  const badge = d.querySelector<HTMLElement>(".tool-head .badge");
  if (badge && ev.status) {
    badge.textContent = ev.status as string;
    badge.className = `badge st-${ev.status}`;
  }
  const body = d.querySelector<HTMLElement>(".tool-body")!;
  const pre = d.querySelector<HTMLElement>(".tool-out")!;
  if (ev.excerpt) {
    pre.textContent += ev.excerpt as string;
    if (body.hidden) {
      // signaler du nouveau contenu sans ouvrir
      d.querySelector(".tool-head")!.classList.add("has-new");
    }
  }
  if (ev.blob && !d.querySelector(".btn-more")) {
    const b = document.createElement("button");
    b.className = "btn-more";
    b.textContent = t("load_full");
    b.addEventListener("click", async () => {
      b.disabled = true;
      const r = await call<{ data: string; total: number }>("get_blob", {
        session: current!.id,
        blob: ev.blob,
        offset: 0,
        limit: 1 << 20,
      });
      pre.textContent = r.data + (r.total > r.data.length ? `\n… (${r.total} o total)` : "");
      b.remove();
    });
    body.appendChild(b);
  }
}

// ---------------------------------------------------------------------------
// Permissions & questions
// ---------------------------------------------------------------------------

function renderPermission(ev: AcEvent): HTMLElement {
  const d = mkEv("perm", ev.seq);
  d.dataset.req = ev.request_id as string;
  const opts = (ev.options as { option_id: string; name: string; kind: string }[]) ?? [];
  d.innerHTML = `<div class="perm-title">🔐 ${esc(ev.title as string)}</div><div class="perm-opts"></div>`;
  const box = d.querySelector<HTMLElement>(".perm-opts")!;
  for (const o of opts) {
    const b = document.createElement("button");
    b.textContent = o.name || o.option_id;
    b.className = o.kind.startsWith("reject") ? "btn-reject" : "btn-allow";
    b.onclick = async () => {
      if (answered.has(ev.request_id as string)) return;
      await call("respond", {
        session: current!.id,
        request_id: ev.request_id,
        option_id: o.option_id,
      }).catch((e) => showErr(String(e)));
      d.classList.add("done");
    };
    box.appendChild(b);
  }
  if (answered.has(ev.request_id as string)) d.classList.add("done");
  return d;
}

function renderQuestion(ev: AcEvent): HTMLElement {
  const d = mkEv("perm question", ev.seq);
  d.dataset.req = ev.request_id as string;
  d.innerHTML = `
    <div class="perm-title">❓ ${esc(ev.message as string)}</div>
    <div class="q-row">
      <input class="q-input" placeholder='${t("answer_ph")}' />
      <button class="btn-allow">${t("answer_btn")}</button>
      <button class="btn-reject">${t("ignore_btn")}</button>
    </div>`;
  const input = d.querySelector<HTMLInputElement>(".q-input")!;
  const answer = async (val: string | null) => {
    if (answered.has(ev.request_id as string)) return;
    let parsed: unknown = null;
    if (val) {
      try {
        parsed = JSON.parse(val);
      } catch {
        parsed = { answer: val };
      }
    }
    await call("respond", {
      session: current!.id,
      request_id: ev.request_id,
      answer: parsed,
    }).catch((e) => showErr(String(e)));
    d.classList.add("done");
  };
  d.querySelector<HTMLButtonElement>(".btn-allow")!.onclick = () => answer(input.value || "{}");
  d.querySelector<HTMLButtonElement>(".btn-reject")!.onclick = () => answer(null);
  if (answered.has(ev.request_id as string)) d.classList.add("done");
  return d;
}

// ---------------------------------------------------------------------------
// Scroll : chargement des pages anciennes via sentinel
// ---------------------------------------------------------------------------

const observer = new IntersectionObserver(async (entries) => {
  if (!entries[0].isIntersecting || loadingOld || !current || !hasMoreHistory) return;
  loadingOld = true;
  try {
    const page = await call<{ events: AcEvent[] }>("events", {
      session: current.id,
      before_seq: minRendered,
      limit: PAGE,
    });
    prependEvents(page.events);
  } finally {
    loadingOld = false;
  }
});
observer.observe(sentinel);

function scrollBottom() {
  eventsEl.scrollTop = eventsEl.scrollHeight;
}

// ---------------------------------------------------------------------------
// Push events du démon
// ---------------------------------------------------------------------------

await listen<{ session: string; event: AcEvent }>("session-event", (msg) => {
  const { session, event } = msg.payload;
  if (current && session === current.id) {
    appendEvent(event, true);
  }
  if (event.type === "status_changed") refreshSessions();
});

// ---------------------------------------------------------------------------
// Actions UI
// ---------------------------------------------------------------------------

$("btn-send").onclick = () => sendInput();
$("input").addEventListener("keydown", (e) => {
  const ac = $("cmd-ac");
  if (!ac.hidden) {
    const sel = ac.querySelector<HTMLElement>(".ac-item.sel");
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      moveAcSel(e.key === "ArrowDown" ? 1 : -1);
      return;
    }
    if ((e.key === "Tab" || e.key === "Enter") && sel) {
      e.preventDefault();
      applyAc(sel.dataset.name!);
      return;
    }
    if (e.key === "Escape") {
      ac.hidden = true;
      return;
    }
  }
  if (e.key === "Enter" && e.ctrlKey) {
    e.preventDefault();
    sendInput(true); // Ctrl+Entrée = envoi forcé (annule le tour en cours)
    return;
  }
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    sendInput();
  }
});

// ---- Commandes built-in (toujours dispo, exécutées côté client) ----
const BUILTINS = (): { name: string; description: string; hint: string }[] => [
  { name: "model", description: t("builtin_model"), hint: "<model>" },
  { name: "resume", description: t("builtin_resume"), hint: "" },
  { name: "cancel", description: t("builtin_cancel"), hint: "" },
];

async function runBuiltin(name: string, arg: string): Promise<boolean> {
  if (!current) return false;
  switch (name) {
    case "model":
      if (!arg) return false;
      await call("set_model", { session: current.id, model: arg });
      return true;
    case "resume":
      await openNativePicker();
      return true;
    case "cancel":
      await call("cancel", { session: current.id });
      return true;
  }
  return false;
}

// ---- Autocomplete des slash-commands ----
let acSel = 0;

function moveAcSel(dir: number) {
  const items = [...$("cmd-ac").querySelectorAll<HTMLElement>(".ac-item")];
  if (!items.length) return;
  acSel = (acSel + dir + items.length) % items.length;
  items.forEach((el, i) => el.classList.toggle("sel", i === acSel));
  items[acSel].scrollIntoView({ block: "nearest" });
}

function applyAc(name: string) {
  const inp = $("input") as HTMLTextAreaElement;
  inp.value = "/" + name + " ";
  $("cmd-ac").hidden = true;
  inp.focus();
}

function refreshAc() {
  const inp = $("input") as HTMLTextAreaElement;
  const ac = $("cmd-ac");
  const v = inp.value;
  const m = /^\/([\w-]*)$/.exec(v);
  if (!m || !current) {
    ac.hidden = true;
    return;
  }
  const q = m[1].toLowerCase();
  const matches = [
    ...current.available_commands,
    ...BUILTINS(),
  ]
    .filter((c) => cmdName(c).toLowerCase().startsWith(q))
    .slice(0, 12);
  if (!matches.length) {
    ac.hidden = true;
    return;
  }
  ac.innerHTML = "";
  acSel = 0;
  matches.forEach((c, i) => {
    const n = cmdName(c);
    const el = document.createElement("div");
    el.className = "ac-item" + (i === 0 ? " sel" : "");
    el.dataset.name = n;
    const h = cmdHint(c);
    el.innerHTML = `<span class="n">/${esc(n)}</span>` +
      (h ? `<span class="h">${esc(h)}</span>` : "") +
      `<span class="d">${esc(cmdDesc(c))}</span>`;
    el.title = cmdDesc(c);
    el.onclick = () => applyAc(n);
    ac.appendChild(el);
  });
  ac.hidden = false;
}

$("input").addEventListener("input", refreshAc);
$("input").addEventListener("blur", () => {
  // délai pour laisser le click sur un item passer
  setTimeout(() => ($("cmd-ac").hidden = true), 150);
});

/** Dialogue : sessions natives de l'agent (ACP session/list) → reprise. */
async function openNativePicker() {
  if (!current) return;
  const box = $("native-list");
  box.innerHTML = "…";
  ($("dlg-resume") as HTMLDialogElement).showModal();
  try {
    const r = await call<{ sessions?: { sessionId: string; title?: string; updatedAt?: string; cwd?: string }[] }>(
      "list_native_sessions",
      { session: current.id },
    );
    const list = r.sessions ?? [];
    box.innerHTML = list.length
      ? ""
      : t("no_native");
    for (const s of list) {
      const row = document.createElement("div");
      row.className = "ac-item";
      row.innerHTML = `<span class="n">${esc(s.sessionId.slice(0, 12))}</span>` +
        `<span class="d">${esc(s.title || s.cwd || "")}</span>` +
        `<span class="h">${esc((s.updatedAt || "").slice(0, 16).replace("T", " "))}</span>`;
      row.onclick = async () => {
        ($("dlg-resume") as HTMLDialogElement).close();
        await call("resume_session", { session: current!.id, native_id: s.sessionId })
          .catch((e) => showErr(String(e)));
        await openSession(current!.id);
      };
      box.appendChild(row);
    }
  } catch (e) {
    // Agent mort → impossible de lister ; proposer la reprise de la session courante
    box.innerHTML = `<div class="errline">${esc(String(e))}</div>`;
    const row = document.createElement("div");
    row.className = "ac-item";
    row.innerHTML = `<span class="n">↻</span><span class="d">${t("resume_current")}</span>`;
    row.onclick = async () => {
      ($("dlg-resume") as HTMLDialogElement).close();
      await call("resume_session", { session: current!.id }).catch((er) => showErr(String(er)));
      await openSession(current!.id);
    };
    box.appendChild(row);
  }
}

async function sendInput(force = false) {
  const t = ($("input") as HTMLTextAreaElement).value.trim();
  if (!t || !current) return;
  // Built-ins client (interceptent avant envoi à l'agent)
  const bm = /^\/(\w+)(?:\s+(.*))?$/.exec(t);
  if (bm && BUILTINS().some((b) => b.name === bm[1])) {
    ($("input") as HTMLTextAreaElement).value = "";
    try {
      await runBuiltin(bm[1], (bm[2] ?? "").trim());
    } catch (e) {
      showErr(String(e));
    }
    return;
  }
  ($("input") as HTMLTextAreaElement).value = "";
  await call("send", { session: current.id, text: t, force }).catch((e) => showErr(String(e)));
}

$("btn-cancel").onclick = () =>
  current && call("cancel", { session: current.id }).catch((e) => showErr(String(e)));

$("btn-resume").onclick = () =>
  current &&
  call("resume_session", { session: current.id })
    .then(() => openSession(current!.id))
    .catch((e) => showErr(String(e)));

($("mode-select") as HTMLSelectElement).onchange = (e) => {
  if (!current) return;
  const sel = e.target as HTMLSelectElement;
  const req = sel.dataset.cfgid
    ? call("set_config_option", { session: current.id, config_id: sel.dataset.cfgid, value: sel.value })
    : call("set_mode", { session: current.id, mode: sel.value });
  req.catch((er) => showErr(String(er)));
};

/** Remplit le select d'effort pour une base de modèle donnée. */
function effortsFor(base: string): string[] {
  const opts: { v: string }[] = JSON.parse(($("model-select") as HTMLSelectElement).dataset.opts || "[]");
  const effs = new Set<string>();
  for (const o of opts) {
    if (o.v === base || o.v.startsWith(base + "-")) {
      const e = splitModel(o.v)[1];
      if (e) effs.add(e);
    }
  }
  return [...effs];
}

function fillEfforts(base: string, cur: string) {
  const es = $("effort-select") as HTMLSelectElement;
  const effs = effortsFor(base);
  es.innerHTML = "";
  for (const e of effs.length ? effs : ["medium", "high"]) {
    const o = document.createElement("option");
    o.value = e;
    o.textContent = e;
    es.appendChild(o);
  }
  if (effs.includes(cur)) es.value = cur;
}

function applyModelValue(v: string) {
  if (!current) return;
  const md = $("model-select") as HTMLSelectElement;
  const req = md.dataset.cfgid
    ? call("set_config_option", { session: current.id, config_id: md.dataset.cfgid, value: v })
    : call("set_model", { session: current.id, model: v });
  req.catch((er) => showErr(String(er)));
}

/** Combinaisons Fusion réellement annoncées par l'agent (liste fermée :
 *  composer librement fusion-A-eff-sidekick-B-eff renvoie Invalid params). */
function fusionTable() {
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

function fillSel(s: HTMLSelectElement, items: [string, string][]) {
  s.innerHTML = "";
  for (const [v, label] of items) {
    const o = document.createElement("option");
    o.value = v;
    o.textContent = label;
    s.appendChild(o);
  }
}

let fusionApplied = false;

/** Dialogue Fusion : seules les paires réellement proposées par l'agent
 *  sont sélectionnables. Pré-rempli avec la fusion courante le cas échéant. */
function openFusion() {
  const { prims, sides } = fusionTable();
  if (!prims.size) {
    showErr(t("fusion_none"));
    return;
  }
  const a = $("fu-a") as HTMLSelectElement;
  const ea = $("fu-ea") as HTMLSelectElement;
  const b = $("fu-b") as HTMLSelectElement;
  const eb = $("fu-eb") as HTMLSelectElement;
  const effLabel = (x: string) => x || "défaut";

  const refreshEB = () => {
    const effs = [...(sides.get(`${a.value}|${ea.value}`)?.get(b.value) ?? new Set<string>())];
    fillSel(eb, effs.map((e) => [e, effLabel(e)]));
  };
  const refreshB = () => {
    const m = sides.get(`${a.value}|${ea.value}`) ?? new Map<string, Set<string>>();
    fillSel(b, [...m.keys()].map((k) => [k, k]));
    refreshEB();
  };
  const refreshEA = () => {
    fillSel(ea, [...(prims.get(a.value) ?? new Set<string>())].map((e) => [e, effLabel(e)]));
    refreshB();
  };

  fillSel(a, [...prims.keys()].map((k) => [k, k]));
  a.onchange = refreshEA;
  ea.onchange = refreshB;
  b.onchange = refreshEB;

  // Pré-remplir depuis la valeur fusion courante (reconfiguration)
  const curVal =
    current?.config_options?.find((o) => o.id === "model" || o.category === "model")
      ?.currentValue || current?.model || "";
  const p = fusionParts(curVal);
  if (p && prims.has(p[0])) {
    a.value = p[0];
    refreshEA();
    if ([...ea.options].some((o) => o.value === p[1])) ea.value = p[1];
    refreshB();
    if ([...b.options].some((o) => o.value === p[2])) b.value = p[2];
    refreshEB();
    if ([...eb.options].some((o) => o.value === p[3])) eb.value = p[3];
  } else {
    refreshEA();
  }
  fusionApplied = false;
  ($("dlg-fusion") as HTMLDialogElement).showModal();
}

$("fu-ok").addEventListener("click", (e) => {
  e.preventDefault();
  const a = ($("fu-a") as HTMLSelectElement).value;
  const ea = ($("fu-ea") as HTMLSelectElement).value;
  const b = ($("fu-b") as HTMLSelectElement).value;
  const eb = ($("fu-eb") as HTMLSelectElement).value;
  const v = `fusion-${a}${ea ? "-" + ea : ""}-sidekick-${b}${eb ? "-" + eb : ""}`;
  if (!fusionTable().valid.has(v)) {
    showErr(t("fusion_bad") + v);
    return;
  }
  fusionApplied = true;
  ($("dlg-fusion") as HTMLDialogElement).close();
  applyModelValue(v);
});

// Annulation du dialogue : si le select affichait "✨ Fusion…" alors que le
// modèle courant n'est pas une fusion, restaurer sa valeur réelle.
($("dlg-fusion") as HTMLDialogElement).addEventListener("close", () => {
  const md = $("model-select") as HTMLSelectElement;
  const curVal = current?.model ?? "";
  if (!fusionApplied && md.value === "__fusion" && !curVal.startsWith("fusion-")) {
    md.value = splitModel(curVal)[0];
  }
});

$("btn-fusion-edit").onclick = () => openFusion();

($("model-select") as HTMLSelectElement).onchange = (e) => {
  const sel = e.target as HTMLSelectElement;
  if (sel.value === "__fusion") {
    openFusion();
    return;
  }
  fillEfforts(sel.value, "");
  const eff = ($("effort-select") as HTMLSelectElement).value;
  applyModelValue(sel.value + (eff ? "-" + eff : ""));
};

($("effort-select") as HTMLSelectElement).onchange = (e) => {
  const base = ($("model-select") as HTMLSelectElement).value;
  const eff = (e.target as HTMLSelectElement).value;
  if (base && base !== "__fusion") applyModelValue(base + (eff ? "-" + eff : ""));
};

$("btn-refresh").onclick = () => current && openSession(current.id);

// Double-clic sur le titre → renommage inline
$("chat-title").addEventListener("dblclick", () => {
  if (!current) return;
  const t = $("chat-title");
  const inp = document.createElement("input");
  inp.id = "title-edit";
  inp.value = current.title;
  t.replaceWith(inp);
  inp.focus();
  inp.select();
  const done = async (save: boolean) => {
    const v = inp.value.trim();
    inp.replaceWith(t);
    if (save && v && v !== current!.title) {
      current!.title = v;
      t.textContent = v;
      await call("rename_session", { session: current!.id, title: v })
        .catch((e) => showErr(String(e)));
      refreshSessions();
    }
  };
  inp.addEventListener("keydown", (e) => {
    if (e.key === "Enter") done(true);
    if (e.key === "Escape") done(false);
  });
  inp.addEventListener("blur", () => done(true));
});

($("model-free") as HTMLInputElement).addEventListener("keydown", async (e) => {
  if (e.key !== "Enter" || !current) return;
  const v = (e.target as HTMLInputElement).value.trim();
  if (v) await call("set_model", { session: current.id, model: v }).catch((er) => showErr(String(er)));
});

$("btn-new-session").onclick = async () => {
  const agents = await call<{ id: string; name: string }[]>("list_agents");
  const sel = $("f-agent") as HTMLSelectElement;
  sel.innerHTML = "";
  for (const a of agents) {
    const o = document.createElement("option");
    o.value = a.id;
    o.textContent = a.name;
    sel.appendChild(o);
  }
  // Suggestions de modèles : union des config_options connues des sessions
  const sess = await call<SessionMeta[]>("list_sessions").catch(() => [] as SessionMeta[]);
  const seen = new Set<string>();
  const dl = $("model-list");
  dl.innerHTML = "";
  for (const s of sess) {
    for (const co of s.config_options ?? []) {
      if (co.id !== "model" && co.category !== "model") continue;
      for (const o of co.options) {
        if (seen.has(o.value)) continue;
        seen.add(o.value);
        const el = document.createElement("option");
        el.value = o.value;
        el.label = o.name || o.value;
        dl.appendChild(el);
      }
    }
  }
  ($("dlg-new") as HTMLDialogElement).showModal();
};

$("f-create").addEventListener("click", async (e) => {
  e.preventDefault();
  const cwd = ($("f-cwd") as HTMLInputElement).value.trim();
  if (!cwd) return;
  const attach = ($("f-attach") as HTMLInputElement).value.trim();
  if (attach) {
    await call("attach_session", {
      agent: ($("f-agent") as HTMLSelectElement).value,
      cwd,
      native_id: attach,
      title: ($("f-title") as HTMLInputElement).value || cwd.split("/").pop(),
    });
  } else {
    await call("new_session", {
      agent: ($("f-agent") as HTMLSelectElement).value,
      cwd,
      title: ($("f-title") as HTMLInputElement).value || cwd.split("/").pop(),
      mode: ($("f-mode") as HTMLInputElement).value || undefined,
      model: ($("f-model") as HTMLInputElement).value || undefined,
      prompt: ($("f-prompt") as HTMLTextAreaElement).value || undefined,
    });
  }
  ($("dlg-new") as HTMLDialogElement).close();
  await refreshSessions();
});

interface SavedConn { kind: string; host?: string; remote_port?: number; args?: string[]; path?: string; addr?: string }
let savedConns: SavedConn[] = [];

// Auto-reconnexion au démarrage avec la dernière connexion utilisée
invoke<SavedConn[]>("saved_connections")
  .then(async (conns) => {
    if (conns.length) await doConnect({ ...conns[0] }).catch((e) => showErr(String(e)));
  })
  .catch(() => {});

function fillConnForm(c: SavedConn) {
  ($("c-kind") as HTMLSelectElement).value = c.kind;
  ($("c-host") as HTMLInputElement).value = c.host ?? c.addr ?? c.path ?? "";
  ($("c-port") as HTMLInputElement).value = String(c.remote_port ?? 7331);
  ($("c-args") as HTMLInputElement).value = (c.args ?? []).join(" ");
}

$("btn-connect").onclick = async () => {
  // Alias Host de ~/.ssh/config en suggestions
  const dl = $("ssh-hosts");
  dl.innerHTML = "";
  for (const h of await invoke<string[]>("ssh_hosts").catch(() => [] as string[])) {
    const o = document.createElement("option");
    o.value = h;
    dl.appendChild(o);
  }
  // Connexions récentes sauvegardées
  savedConns = await invoke<SavedConn[]>("saved_connections").catch(() => []);
  const sv = $("c-saved") as HTMLSelectElement;
  sv.innerHTML = `<option value="">${t("choose")}</option>`;
  savedConns.forEach((c, i) => {
    const o = document.createElement("option");
    o.value = String(i);
    o.textContent = `${c.kind} ${c.host ?? c.addr ?? c.path ?? ""}`;
    sv.appendChild(o);
  });
  // Pré-remplir avec la dernière utilisée → un clic sur Connecter suffit
  if (savedConns.length) {
    sv.value = "0";
    fillConnForm(savedConns[0]);
  }
  ($("dlg-conn") as HTMLDialogElement).showModal();
};

($("c-saved") as HTMLSelectElement).onchange = (e) => {
  const i = Number((e.target as HTMLSelectElement).value);
  if (savedConns[i]) fillConnForm(savedConns[i]);
};
$("c-logs").addEventListener("click", async () => {
  const v = $("log-view");
  v.hidden = !v.hidden;
  if (!v.hidden) v.textContent = await invoke<string>("log_tail", { n: 200 });
});

$("c-go").addEventListener("click", async (e) => {
  e.preventDefault();
  const btn = e.currentTarget as HTMLButtonElement;
  btn.disabled = true;
  btn.classList.add("spin");
  const kind = ($("c-kind") as HTMLSelectElement).value;
  const cfg =
    kind === "ssh"
      ? {
          kind: "ssh",
          host: ($("c-host") as HTMLInputElement).value,
          remote_port: Number(($("c-port") as HTMLInputElement).value) || 7331,
          args: ($("c-args") as HTMLInputElement).value.split(" ").filter(Boolean),
        }
      : kind === "tcp"
        ? { kind: "tcp", addr: ($("c-host") as HTMLInputElement).value }
        : { kind: "local" };
  try {
    await doConnect(cfg);
    invoke("save_connection", { cfg }).catch(console.error);
    ($("dlg-conn") as HTMLDialogElement).close();
  } catch (err) {
    showErr(String(err));
  } finally {
    btn.disabled = false;
    btn.classList.remove("spin");
  }
});
