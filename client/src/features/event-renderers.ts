// ---------------------------------------------------------------------------
// Rendu des événements : messages (user/agent/peer), tool calls (bloc
// pliable + expand blob + updates différées), permissions & questions.
// L'état de rendu (toolBlocks/answered/pendingToolUpdates) est swapé par
// le cache de conversation à chaque changement de session.
// ---------------------------------------------------------------------------

import { $, esc, mkEv, q, showErr } from "../core/dom";
import { linkify, mdRender } from "../core/format";
import { call } from "../core/rpc";
import { app } from "../core/state";
import type { AcEvent, CmdInfo, ConfigOption } from "../core/types";
import { t } from "../i18n";
import { icon } from "../ui/icons";

// Seuils au-delà desquels une bulle user est repliée par défaut.
const USER_FOLD_CHARS = 480;
const USER_FOLD_LINES = 5;

export class EventRenderer {
  // Réassignés au swap de session (cache par conversation) — publics pour
  // que ChatView puisse les stash/restaurer.
  toolBlocks = new Map<string, HTMLElement>();
  answered = new Set<string>();
  // Updates reçues avant leur bloc tool_call (bloc dans une page
  // d'historique pas encore chargée) — flushées quand le bloc est rendu.
  pendingToolUpdates = new Map<string, AcEvent[]>();
  // Pendant un prepend, la compensation de fin de page couvre déjà tout.
  inPrepend = false;
  /** Ancre anti-doublon pensée/message : un agent peut émettre le même
   *  texte dans les deux rôles (observé : bloc 💭 reprenant intégralement
   *  le message qui précède). La comparaison porte sur les blocs texte
   *  — les events intermédiaires (tool_call, status…) n'y coupent pas.
   *  Swapée avec le reste de l'état de rendu. */
  lastText: { thought: boolean; norm: string; el: HTMLElement } | null = null;
  /** Nb de user_message rendus — le 1er d'une session « service » est le
   *  prompt de spawn par construction → pill compacte (état swapé). */
  userMsgs = 0;

  private deps = {
    /** Meta de la session affichée mutée par un event (mode/model/state…) →
     *  re-render du header. */
    metaChanged: () => {},
    ctxChanged: () => {},
    /** Event live status_changed → rafraîchir la sidebar. */
    statusChanged: () => {},
    /** Clic sur un en-tête de message peer → ouvrir la session cible. */
    openSession: (_id: string) => {},
  };

  bind(d: Partial<typeof this.deps>) {
    Object.assign(this.deps, d);
  }

  private normText(t: unknown) {
    return String(t ?? "").replace(/\s+/g, " ").trim();
  }

  /// Dédup pensée/message à l'append (même texte normalisé que le bloc
  /// texte agent précédent, à travers les events non-texte) :
  ///  - "skip" : doublon pur — ne pas rendre ;
  ///  - HTMLElement : la pensée précédente était la redondante — la
  ///    retirer, le message identique la remplace ;
  ///  - null : rendre normalement.
  textDup(ev: AcEvent): "skip" | HTMLElement | null {
    if (ev.type !== "agent_text" && ev.type !== "agent_thought") return null;
    const norm = this.normText(ev.text);
    if (!norm) return null;
    const prev = this.lastText;
    if (!prev || prev.norm !== norm) return null;
    if (ev.type === "agent_text" && prev.thought) return prev.el;
    return "skip";
  }

  /// Enregistre le bloc texte réellement rendu — ancre de la dédup
  /// suivante (à appeler après l'insertion du nœud). Un message user/peer
  /// ouvre un nouveau tour : répéter un texte y est légitime → reset.
  noteText(ev: AcEvent, el: HTMLElement) {
    if (ev.type === "user_message" || ev.type === "peer_message") {
      this.lastText = null;
      return;
    }
    if (ev.type !== "agent_text" && ev.type !== "agent_thought") return;
    const norm = this.normText(ev.text);
    if (norm) this.lastText = { thought: ev.type === "agent_thought", norm, el };
  }

  render(ev: AcEvent, live: boolean): HTMLElement {
    switch (ev.type) {
      case "user_message": {
        const d = mkEv("msg user", ev.seq);
        const txt = ev.text as string;
        // Prompt de spawn d'une session « service » (origin) = le 1er
        // user_message du journal par construction : pill d'une ligne
        // plutôt que bulle — le boilerplate ne prime pas sur le post
        // de l'item qui suit (badge « via » conservé, dépliable).
        const servicePrompt = !!ev.origin && this.userMsgs === 0;
        this.userMsgs += 1;
        const body = document.createElement("div");
        body.className = "um-body";
        body.innerHTML =
          (ev.origin
            ? `<span class="origin-badge">${esc(t("via"))} ${esc(ev.origin as string)}</span> `
            : "") +
          (ev.steer ? `<span class="steer-badge">${t("steer_badge")}</span> ` : "") +
          linkify(esc(txt));
        d.appendChild(body);
        // Prompt long → replié par défaut, bouton pour déplier/replier.
        if (
          servicePrompt ||
          txt.length > USER_FOLD_CHARS ||
          txt.split("\n").length > USER_FOLD_LINES
        ) {
          d.classList.add("um-collapsed");
          if (servicePrompt) d.classList.add("um-service");
          const btn = document.createElement("button");
          btn.type = "button";
          btn.className = "um-toggle";
          const setLabel = (collapsed: boolean) => {
            btn.innerHTML = `${icon("chev-down", "ic-caret")} ${esc(t(collapsed ? "um_expand" : "um_collapse"))}`;
          };
          setLabel(true);
          btn.addEventListener("click", () => {
            setLabel(d.classList.toggle("um-collapsed"));
          });
          d.appendChild(btn);
        }
        return d;
      }
      case "agent_text": {
        const d = mkEv("msg agent", ev.seq);
        d.innerHTML = `<div class="md">${mdRender(ev.text as string)}</div>`;
        return d;
      }
      case "peer_message": {
        // Message agent→agent : journalisé dans les deux sessions — la
        // direction se déduit de from_session vs la session ouverte.
        const outgoing = app.current?.id === ev.from_session;
        const peerId = (outgoing ? ev.to_session : ev.from_session) as string;
        const d = mkEv(`msg peer ${outgoing ? "out" : "in"}`, ev.seq);
        const label = outgoing
          ? `${t("peer_to")} «${ev.to_title}»`
          : `${t("peer_from")} «${ev.from_title}»`;
        d.innerHTML =
          `<div class="peer-head" title="${q(esc(peerId))}">${icon(outgoing ? "arrow-right" : "peer")} ${esc(label)}</div>` +
          `<div class="peer-body">${linkify(esc(ev.text as string))}</div>`;
        d.querySelector(".peer-head")?.addEventListener("click", () =>
          this.deps.openSession(peerId));
        return d;
      }
      case "agent_thought": {
        const d = mkEv("thought", ev.seq);
        const txt = ev.text as string;
        const head = txt.trim().split("\n")[0].slice(0, 110);
        d.innerHTML = `<details class="th"><summary>${icon("chev-right", "ic-caret")} ${icon("thought")} ${esc(head)}</summary><div class="md th-body">${mdRender(txt)}</div></details>`;
        return d;
      }
      case "tool_call":
        return this.renderToolCall(ev);
      case "permission_request":
        return this.renderPermission(ev);
      case "question_request":
        return this.renderQuestion(ev);
      case "permission_resolved":
      case "question_resolved": {
        const d = mkEv("resolved", ev.seq);
        const rid = ev.request_id as string;
        this.answered.add(rid);
        d.innerHTML = `${icon("check")} ${esc(t("resolved") + ((ev.outcome ?? ev.answer) as string))}`;
        const card = $("events").querySelector(`[data-req="${rid}"]`);
        if (card) card.classList.add("done");
        return d;
      }
      case "mode_changed": {
        const d = mkEv("sysline", ev.seq);
        d.textContent = t("mode_arrow") + (ev.mode as string);
        if (app.current) {
          app.current.mode = ev.mode as string;
          this.deps.metaChanged();
        }
        return d;
      }
      case "config_options": {
        if (app.current) {
          app.current.config_options = ev.options as ConfigOption[];
          const mo = app.current.config_options.find((o) => o.id === "model" || o.category === "model");
          if (mo) app.current.model = mo.currentValue;
          const md = app.current.config_options.find((o) => o.id === "mode" || o.category === "mode");
          if (md) app.current.mode = md.currentValue;
          this.deps.metaChanged();
        }
        const d = mkEv("sysline", ev.seq);
        d.hidden = true;
        return d;
      }
      case "usage_update": {
        // Jauge contexte — pas rendue comme message ; met à jour le badge
        // du header (replay : la dernière valeur de la page gagne).
        if (app.current) {
          app.current.context_used = ev.used as number;
          app.current.context_size = ev.size as number;
          this.deps.ctxChanged();
        }
        const d = mkEv("sysline", ev.seq);
        d.hidden = true;
        return d;
      }
      case "commands_available": {
        const d = mkEv("sysline", ev.seq);
        const cmds = ev.commands as CmdInfo[];
        d.textContent = t("cmds_avail", { n: cmds.length });
        if (app.current) {
          app.current.available_commands = cmds;
          this.deps.metaChanged();
        }
        return d;
      }
      case "status_changed": {
        const d = mkEv("sysline", ev.seq);
        d.textContent = t("state_arrow") + (ev.state as string);
        // Seul un event LIVE met à jour l'état — rejouer l'historique du
        // journal écraserait alive/meta avec un état périmé ("working"
        // d'une session morte → bouton Reprendre masqué).
        if (live && app.current) {
          app.current.state = ev.state as string;
          app.alive = ev.state !== "dead" && ev.state !== "asleep" &&
            ev.state !== "interrupted";
          this.deps.metaChanged();
          this.deps.statusChanged();
        }
        return d;
      }
      case "error": {
        const d = mkEv("errline", ev.seq);
        d.innerHTML = `${icon("warn")} ${esc(ev.message as string)}`;
        return d;
      }
      case "system": {
        const d = mkEv("sysline", ev.seq);
        d.innerHTML = `${icon("sys")} ${esc(ev.text as string)}`;
        return d;
      }
      default: {
        const d = mkEv("sysline", ev.seq);
        d.textContent = ev.type;
        return d;
      }
    }
  }

  // -------------------------------------------------------------------------
  // Tool calls : bloc pliable + expand blob
  // -------------------------------------------------------------------------

  private renderToolCall(ev: AcEvent): HTMLElement {
    const d = mkEv("tool", ev.seq);
    const status = ev.status as string;
    d.dataset.callId = ev.call_id as string;
    d.innerHTML = `
      <div class="tool-head">
        ${icon("chev-right", "ic-caret")}
        <span class="tool-ico">${icon("tool")}</span>
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
          session: app.current!.id,
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

  private stashToolUpdate(ev: AcEvent) {
    const id = ev.call_id as string;
    const l = this.pendingToolUpdates.get(id) ?? [];
    // ré-insertion en fin → l'éviction au cap est LRU, pas FIFO strict
    this.pendingToolUpdates.delete(id);
    this.pendingToolUpdates.set(id, l);
    if (this.pendingToolUpdates.size > 300)
      this.pendingToolUpdates.delete(this.pendingToolUpdates.keys().next().value!);
    if (l.length < 400) l.push(ev);
  }

  /// Les updates du stash sont toujours plus récentes que la page en cours
  /// (live ou pages déjà rendues) → à appeler APRÈS le rendu de la page.
  flushToolUpdates(callId: string) {
    const list = this.pendingToolUpdates.get(callId);
    if (!list?.length) return;
    this.pendingToolUpdates.delete(callId);
    list.sort((a, b) => a.seq - b.seq);
    for (const ev of list) this.updateToolBlock(ev);
  }

  /// Exécute `fn` en compensant le scroll si `d` est entièrement au-dessus
  /// du viewport : sa croissance pousserait sinon le contenu visible.
  /// Pendant un prepend, la compensation de fin de page couvre déjà tout.
  private anchorIfAbove(d: HTMLElement, fn: () => void) {
    const eventsEl = $("events");
    if (this.inPrepend) {
      fn();
      return;
    }
    const r = d.getBoundingClientRect();
    if (r.bottom >= eventsEl.getBoundingClientRect().top) {
      fn();
      return;
    }
    const h = r.height;
    fn();
    eventsEl.scrollTop += d.getBoundingClientRect().height - h;
  }

  updateToolBlock(ev: AcEvent) {
    const d = this.toolBlocks.get(ev.call_id as string);
    if (!d) {
      this.stashToolUpdate(ev);
      return;
    }
    // Idempotent par nœud : un bloc re-rendu (trim → refetch) repart à 0.
    const last = Number(d.dataset.updSeq ?? 0);
    if (ev.seq <= last) return;
    d.dataset.updSeq = String(ev.seq);
    this.anchorIfAbove(d, () => {
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
            session: app.current!.id,
            blob: ev.blob,
            offset: 0,
            limit: 1 << 20,
          });
          pre.textContent = r.data + (r.total > r.data.length ? `\n… (${r.total} o total)` : "");
          b.remove();
        });
        body.appendChild(b);
      }
    });
  }

  // -------------------------------------------------------------------------
  // Permissions & questions
  // -------------------------------------------------------------------------

  private renderPermission(ev: AcEvent): HTMLElement {
    const d = mkEv("perm", ev.seq);
    d.dataset.req = ev.request_id as string;
    const opts = (ev.options as { option_id: string; name: string; kind: string }[]) ?? [];
    const kind = ev.kind as string | undefined;
    const detail = ev.detail as string | undefined;
    d.innerHTML =
      `<div class="perm-title">${icon("lock")} ${esc(ev.title as string)}` +
      (kind ? ` <span class="perm-kind">${esc(kind)}</span>` : "") +
      `</div>` +
      (detail ? `<pre class="perm-detail">${esc(detail)}</pre>` : "") +
      `<div class="perm-opts"></div>`;
    const box = d.querySelector<HTMLElement>(".perm-opts")!;
    for (const o of opts) {
      const b = document.createElement("button");
      b.textContent = o.name || o.option_id;
      b.className = o.kind.startsWith("reject") ? "btn-reject" : "btn-allow";
      b.onclick = async () => {
        if (this.answered.has(ev.request_id as string)) return;
        await call("respond", {
          session: app.current!.id,
          request_id: ev.request_id,
          option_id: o.option_id,
        }).catch((e) => showErr(String(e)));
        d.classList.add("done");
      };
      box.appendChild(b);
    }
    if (this.answered.has(ev.request_id as string)) d.classList.add("done");
    return d;
  }

  private renderQuestion(ev: AcEvent): HTMLElement {
    const d = mkEv("perm question", ev.seq);
    d.dataset.req = ev.request_id as string;
    d.innerHTML = `
      <div class="perm-title">${icon("question")} ${esc(ev.message as string)}</div>
      <div class="q-row">
        <input class="q-input" placeholder='${t("answer_ph")}' />
        <button class="btn-allow">${t("answer_btn")}</button>
        <button class="btn-reject">${t("ignore_btn")}</button>
      </div>`;
    const input = d.querySelector<HTMLInputElement>(".q-input")!;
    const answer = async (val: string | null) => {
      if (this.answered.has(ev.request_id as string)) return;
      let parsed: unknown = null;
      if (val) {
        try {
          parsed = JSON.parse(val);
        } catch {
          parsed = { answer: val };
        }
      }
      await call("respond", {
        session: app.current!.id,
        request_id: ev.request_id,
        answer: parsed,
      }).catch((e) => showErr(String(e)));
      d.classList.add("done");
    };
    d.querySelector<HTMLButtonElement>(".btn-allow")!.onclick = () => answer(input.value || "{}");
    d.querySelector<HTMLButtonElement>(".btn-reject")!.onclick = () => answer(null);
    if (this.answered.has(ev.request_id as string)) d.classList.add("done");
    return d;
  }
}
