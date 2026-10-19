import "./styles.css";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

// ---------------------------------------------------------------------------
// Types miroir du protocole
// ---------------------------------------------------------------------------

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
  available_commands: string[];
  state: string;
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
const PAGE = 120;
const toolBlocks = new Map<string, HTMLElement>();
const answered = new Set<string>();
let loadingOld = false;
let hasMoreHistory = true;

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const eventsEl = $("events");
const sentinel = $("scroll-sentinel");

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
    div.innerHTML = `<span class="dot st-${s.state}"></span><span class="t">${esc(s.title)}</span><span class="a">${esc(s.agent)}</span>`;
    div.onclick = () => openSession(s.id);
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
  await invoke("subscribe", { session: id, afterSeq }).catch(console.error);
  refreshSessions();
}

function updateHeader() {
  if (!current) return;
  const st = $("chat-state");
  st.textContent = current.state;
  st.className = `badge st-${current.state}`;
  $("btn-resume").hidden = !(current.state === "interrupted" || current.state === "dead");
  $("btn-cancel").hidden = !(current.state === "working" || current.state === "waiting_input");

  const ms = $("mode-select") as HTMLSelectElement;
  if (current.available_modes.length) {
    ms.hidden = false;
    ms.innerHTML = "";
    for (const m of current.available_modes) {
      const o = document.createElement("option");
      o.value = m;
      o.textContent = m;
      ms.appendChild(o);
    }
    if (current.mode) ms.value = current.mode;
  } else {
    ms.hidden = true;
  }

  const cs = $("cmd-select") as HTMLSelectElement;
  if (current.available_commands.length) {
    cs.hidden = false;
    cs.innerHTML = '<option value="">⌘ commandes…</option>';
    for (const c of current.available_commands) {
      const o = document.createElement("option");
      o.value = c;
      o.textContent = c;
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
    case "commands_available": {
      const d = mkEv("sysline", ev.seq);
      d.textContent = `${(ev.commands as string[]).length} commandes disponibles`;
      if (current) {
        current.available_commands = ev.commands as string[];
        updateHeader();
      }
      return d;
    }
    case "status_changed": {
      const d = mkEv("sysline", ev.seq);
      d.textContent = `état → ${ev.state}`;
      if (current) {
        current.state = ev.state as string;
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
      }).catch((e) => alert(String(e)));
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
    }).catch((e) => alert(String(e)));
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
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    sendInput();
  }
});

async function sendInput() {
  const t = ($("input") as HTMLTextAreaElement).value.trim();
  if (!t || !current) return;
  ($("input") as HTMLTextAreaElement).value = "";
  await call("send", { session: current.id, text: t }).catch((e) => alert(String(e)));
}

$("btn-cancel").onclick = () =>
  current && call("cancel", { session: current.id }).catch((e) => alert(String(e)));

$("btn-resume").onclick = () =>
  current &&
  call("resume_session", { session: current.id })
    .then(() => openSession(current!.id))
    .catch((e) => alert(String(e)));

($("mode-select") as HTMLSelectElement).onchange = (e) =>
  current &&
  call("set_mode", {
    session: current.id,
    mode: (e.target as HTMLSelectElement).value,
  }).catch((e) => alert(String(e)));

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
  ($("dlg-new") as HTMLDialogElement).showModal();
};

$("f-create").addEventListener("click", async (e) => {
  e.preventDefault();
  const cwd = ($("f-cwd") as HTMLInputElement).value.trim();
  if (!cwd) return;
  await call("new_session", {
    agent: ($("f-agent") as HTMLSelectElement).value,
    cwd,
    title: ($("f-title") as HTMLInputElement).value || cwd.split("/").pop(),
    mode: ($("f-mode") as HTMLInputElement).value || undefined,
    model: ($("f-model") as HTMLInputElement).value || undefined,
    prompt: ($("f-prompt") as HTMLTextAreaElement).value || undefined,
  });
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
$("c-go").addEventListener("click", async (e) => {
  e.preventDefault();
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
    alert(String(err));
  }
});
