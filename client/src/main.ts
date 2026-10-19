import "./styles.css";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

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
const PAGE = 120;
const toolBlocks = new Map<string, HTMLElement>();
const answered = new Set<string>();
let loadingOld = false;
let hasMoreHistory = true;
let alive = false;

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const eventsEl = $("events");
const sentinel = $("scroll-sentinel");

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
  el.textContent = `connecté (${(status as { sessions?: number }).sessions ?? 0} sessions)`;
  el.className = "badge ok";
  await refreshSessions();
}

// ---------------------------------------------------------------------------
// Liste des sessions
// ---------------------------------------------------------------------------

async function refreshSessions() {
  const list = await call<SessionMeta[]>("list_sessions");
  const el = $("session-list");
  el.innerHTML = "";
  for (const s of list) {
    const div = document.createElement("div");
    div.className = "sess" + (current?.id === s.id ? " active" : "");
    div.innerHTML = `<span class="dot st-${s.state}"></span><span class="t">${esc(s.title)}</span><span class="a">${esc(s.agent)}</span><button class="sess-del" title="Supprimer">✕</button>`;
    div.onclick = () => openSession(s.id);
    const del = div.querySelector<HTMLElement>(".sess-del")!;
    del.addEventListener("click", async (e) => {
      e.stopPropagation();
      if (!del.dataset.armed) {
        del.dataset.armed = "1";
        del.textContent = "suppr ?";
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

  // Charge la dernière page d'événements
  const page = await call<{ events: AcEvent[]; last_seq: number }>("events", {
    session: id,
    limit: PAGE,
  });
  for (const ev of page.events) appendEvent(ev, false);
  scrollBottom();

  // Subscribe live à partir du dernier event connu
  const afterSeq = page.events.length ? page.events[page.events.length - 1].seq : 0;
  await invoke("subscribe", { session: id, afterSeq }).catch((e) => {
    // Le flux live est mort (ex. restart démon) → statut déconnecté visible
    const el = $("conn-status");
    el.textContent = "déconnecté — reconnecter";
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
    fo.textContent = "✨ Fusion (2 modèles)…";
    md.appendChild(fo);
    md.dataset.cfgid = modelOpt ? modelOpt.id : "";
    md.dataset.opts = JSON.stringify(rawOpts);
    const curVal = modelOpt?.currentValue || current.model || "";
    const [curBase, curEff] = splitModel(curVal);
    md.value = curVal.startsWith("fusion-") ? "__fusion" : curBase;
    // Select d'effort : suffixes réellement présents pour la base choisie
    fillEfforts(md.value, curEff);
    es.hidden = false;
  } else {
    md.hidden = true;
    es.hidden = true;
    mf.hidden = false;
    mf.value = current.model ?? "";
  }

  const cs = $("cmd-select") as HTMLSelectElement;
  if (current.available_commands.length) {
    cs.hidden = false;
    cs.innerHTML = '<option value="">⌘ commandes…</option>';
    for (const c of current.available_commands) {
      const o = document.createElement("option");
      o.value = "/" + cmdName(c);
      const d = cmdDesc(c);
      const h = cmdHint(c);
      o.textContent = "/" + cmdName(c) + (h ? ` ${h}` : "");
      o.title = d + (h ? ` — ex: /${cmdName(c)} ${h}` : "");
      cs.appendChild(o);
    }
  } else {
    cs.hidden = true;
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
  const node = renderEvent(ev);
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
    const node = renderEvent(ev);
    eventsEl.insertBefore(node, eventsEl.children[1]); // après sentinel
    renderedCount++;
    if (ev.seq < minRendered) minRendered = ev.seq;
    if (ev.type === "tool_call") toolBlocks.set(ev.call_id as string, node);
    if (ev.type === "tool_call_update") updateToolBlock(ev);
  }
  // garder la position visuelle
  eventsEl.scrollTop += eventsEl.scrollHeight - oldHeight;
}

function renderEvent(ev: AcEvent): HTMLElement {
  switch (ev.type) {
    case "user_message": {
      const d = mkEv("msg user", ev.seq);
      d.textContent = ev.text as string;
      return d;
    }
    case "agent_text": {
      const d = mkEv("msg agent", ev.seq);
      d.textContent = ev.text as string;
      return d;
    }
    case "agent_thought": {
      const d = mkEv("thought", ev.seq);
      d.innerHTML = `<details><summary>pensée</summary><pre>${esc(ev.text as string)}</pre></details>`;
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
      d.textContent = `✔ résolu — ${(ev.outcome ?? ev.answer) as string}`;
      const card = eventsEl.querySelector(`[data-req="${rid}"]`);
      if (card) card.classList.add("done");
      return d;
    }
    case "mode_changed": {
      const d = mkEv("sysline", ev.seq);
      d.textContent = `mode → ${ev.mode}`;
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
      d.textContent = `${cmds.length} commandes disponibles`;
      if (current) {
        current.available_commands = cmds;
        updateHeader();
      }
      return d;
    }
    case "status_changed": {
      const d = mkEv("sysline", ev.seq);
      d.textContent = `état → ${ev.state}`;
      if (current) {
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
  });
  const btn = d.querySelector<HTMLButtonElement>(".btn-more");
  if (btn && ev.blob) {
    btn.addEventListener("click", async () => {
      btn.disabled = true;
      btn.textContent = "chargement…";
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
    b.textContent = "charger la sortie complète…";
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
      <input class="q-input" placeholder='réponse (JSON ou texte)' />
      <button class="btn-allow">Répondre</button>
      <button class="btn-reject">Ignorer</button>
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

$("btn-send").onclick = sendInput;
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
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    sendInput();
  }
});

// ---- Commandes built-in (toujours dispo, exécutées côté client) ----
const BUILTINS: { name: string; description: string; hint: string }[] = [
  { name: "model", description: "Changer le modèle de la session", hint: "<modèle>" },
  { name: "resume", description: "Reprendre la session (respawn agent + session/load)", hint: "" },
  { name: "cancel", description: "Annuler la requête en cours", hint: "" },
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
    ...BUILTINS,
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
      : "Aucune session native (ou session/list non supporté par l'agent).";
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
    row.innerHTML = `<span class="n">↻</span><span class="d">Reprendre la session courante (respawn + session/load)</span>`;
    row.onclick = async () => {
      ($("dlg-resume") as HTMLDialogElement).close();
      await call("resume_session", { session: current!.id }).catch((er) => showErr(String(er)));
      await openSession(current!.id);
    };
    box.appendChild(row);
  }
}

async function sendInput() {
  const t = ($("input") as HTMLTextAreaElement).value.trim();
  if (!t || !current) return;
  // Built-ins client (interceptent avant envoi à l'agent)
  const bm = /^\/(\w+)(?:\s+(.*))?$/.exec(t);
  if (bm && BUILTINS.some((b) => b.name === bm[1])) {
    ($("input") as HTMLTextAreaElement).value = "";
    try {
      await runBuiltin(bm[1], (bm[2] ?? "").trim());
    } catch (e) {
      showErr(String(e));
    }
    return;
  }
  ($("input") as HTMLTextAreaElement).value = "";
  await call("send", { session: current.id, text: t }).catch((e) => showErr(String(e)));
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

/** Dialogue Fusion : 2 modèles + efforts → value fusion-A-effA-sidekick-B-effB */
function openFusion() {
  const md = $("model-select") as HTMLSelectElement;
  const bases = [...md.querySelectorAll("option")]
    .map((o) => o.value)
    .filter((v) => v !== "__fusion" && v !== "");
  for (const id of ["fu-a", "fu-b"]) {
    const s = $(id) as HTMLSelectElement;
    s.innerHTML = "";
    for (const b of bases) {
      const o = document.createElement("option");
      o.value = b;
      o.textContent = b;
      s.appendChild(o);
    }
  }
  for (const id of ["fu-ea", "fu-eb"]) {
    const s = $(id) as HTMLSelectElement;
    s.innerHTML = "";
    for (const e of ["medium", "high"]) {
      const o = document.createElement("option");
      o.value = e;
      o.textContent = e;
      s.appendChild(o);
    }
  }
  ($("dlg-fusion") as HTMLDialogElement).showModal();
}

$("fu-ok").addEventListener("click", (e) => {
  e.preventDefault();
  const a = ($("fu-a") as HTMLSelectElement).value;
  const ea = ($("fu-ea") as HTMLSelectElement).value;
  const b = ($("fu-b") as HTMLSelectElement).value;
  const eb = ($("fu-eb") as HTMLSelectElement).value;
  ($("dlg-fusion") as HTMLDialogElement).close();
  applyModelValue(`fusion-${a}-${ea}-sidekick-${b}-${eb}`);
});

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

($("model-free") as HTMLInputElement).addEventListener("keydown", async (e) => {
  if (e.key !== "Enter" || !current) return;
  const v = (e.target as HTMLInputElement).value.trim();
  if (v) await call("set_model", { session: current.id, model: v }).catch((er) => showErr(String(er)));
});

($("cmd-select") as HTMLSelectElement).onchange = (e) => {
  const v = (e.target as HTMLSelectElement).value;
  if (v) {
    ($("input") as HTMLTextAreaElement).value = v + " ";
    ($("input") as HTMLTextAreaElement).focus();
    (e.target as HTMLSelectElement).value = "";
  }
};

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

$("btn-connect").onclick = async () => {
  // Alias Host de ~/.ssh/config en suggestions
  const dl = $("ssh-hosts");
  dl.innerHTML = "";
  for (const h of await invoke<string[]>("ssh_hosts").catch(() => [] as string[])) {
    const o = document.createElement("option");
    o.value = h;
    dl.appendChild(o);
  }
  ($("dlg-conn") as HTMLDialogElement).showModal();
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
    await invoke("connect", { cfg });
    ($("conn-status") as HTMLElement).textContent = "connecté";
    ($("conn-status") as HTMLElement).className = "badge ok";
    await refreshSessions();
    ($("dlg-conn") as HTMLDialogElement).close();
  } catch (err) {
    showErr(String(err));
  } finally {
    btn.disabled = false;
    btn.classList.remove("spin");
  }
});
