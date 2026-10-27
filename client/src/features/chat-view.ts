// ---------------------------------------------------------------------------
// Vue conversation : ouverture d'une session (session_info + dernière page +
// subscribe), cache mémoire des conversations, pagination au scroll,
// header (titre, état, jauge contexte, selects) et ligne système.
// Le rendu des events eux-mêmes est délégué à EventRenderer.
// ---------------------------------------------------------------------------

import { invoke } from "@tauri-apps/api/core";
import { $, esc, showErr } from "../core/dom";
import { fmtTok } from "../core/format";
import { call } from "../core/rpc";
import { app } from "../core/state";
import type { AcEvent, SessionMeta } from "../core/types";
import { gemSVG } from "../crystal";
import { t } from "../i18n";
import { fillEfforts, splitModel } from "./modelbar";
import type { EventRenderer } from "./event-renderers";

const MAX_RENDERED = 400;
const PAGE = 50; // paquets au scroll
const INITIAL_PAGE = 200; // base d'historique à l'ouverture
// Fenêtre de rattrapage du subscribe — doit matcher SUBSCRIBE_REPLAY_MAX
// côté démon : un trou plus grand n'est pas rejoué (cache jeté + dernière
// page rechargée, l'historique restant paginable via events/before_seq).
const REPLAY_WINDOW = 500;
const CACHE_TTL = 10 * 60_000;
const CACHE_MAX = 6;
const TAIL_KEEP = 50; // derniers events gardés par session
const TAIL_MAX = 12; // sessions distinctes gardées (LRU)

// Cache mémoire des conversations : le DOM détaché + l'état de rendu sont
// conservés à la sortie d'une session → réaffichage instantané au retour,
// puis subscribe(after_seq) rejoue le trou (read_after du journal démon).
// Expiré (TTL) ou évincé (LRU) → rechargement complet derrière la gemme.
interface ConvCache {
  conn: string;
  frag: DocumentFragment;
  toolBlocks: Map<string, HTMLElement>;
  pendingToolUpdates: Map<string, AcEvent[]>;
  answered: Set<string>;
  minRendered: number;
  renderedCount: number;
  hasMoreHistory: boolean;
  unseenBelow: number;
  lastSeq: number;
  scrollTop: number;
  atBottom: boolean;
  ts: number;
}

export class ChatView {
  private renderer!: EventRenderer;
  private eventsEl = $("events");
  private sentinel = $("scroll-sentinel");

  private openSeq = 0;
  private convCache = new Map<string, ConvCache>();
  // Queue brute des N derniers events par session — filet de secours du
  // stash DOM : expiré/évincé → la réouverture réaffiche la queue
  // instantanément, puis subscribe(after_seq) rejoue le trou borné. Les
  // pushes n'arrivent que pour la session ouverte → seules les sessions
  // visitées remplissent leur queue.
  private tailCache = new Map<string, { conn: string; evs: AcEvent[] }>();
  private minRendered = Number.MAX_SAFE_INTEGER;
  private renderedCount = 0;
  private loadingOld = false;
  private hasMoreHistory = true;
  private unseenBelow = 0;
  private convLoadingEl: HTMLElement | null = null;

  private deps = {
    /** Marque la ligne de session active dans la sidebar. */
    setActiveRow: (_id: string) => {},
    /** Rafraîchit la sidebar. */
    refresh: (): Promise<void> => Promise.resolve(),
    /** Barre de file (prompts en attente) — rafraîchie par updateHeader. */
    refreshQueue: () => {},
    /** L'arbre fichiers suit le workspace de la session ouverte. */
    onSessionOpened: () => {},
  };

  bind(renderer: EventRenderer, d: Partial<typeof this.deps>) {
    this.renderer = renderer;
    Object.assign(this.deps, d);
  }

  /** Grande gemme centrée dans la zone de discussion pendant le chargement. */
  private showConvLoading(v: boolean) {
    if (!this.convLoadingEl) {
      this.convLoadingEl = document.createElement("div");
      this.convLoadingEl.id = "conv-loading";
      this.convLoadingEl.innerHTML =
        `<span class="load-gem">${gemSVG("agent")}</span><span>${esc(t("loading"))}</span>`;
    }
    if (v) this.eventsEl.appendChild(this.convLoadingEl);
    else this.convLoadingEl.remove();
  }

  /** Vide la zone (sentinelle conservée) et réinitialise l'état de rendu. */
  clearConv() {
    for (const n of [...this.eventsEl.children]) {
      if (n !== this.sentinel) n.remove();
    }
    this.renderer.toolBlocks = new Map();
    this.renderer.pendingToolUpdates = new Map();
    this.renderer.answered = new Set();
    this.minRendered = Number.MAX_SAFE_INTEGER;
    this.renderedCount = 0;
    this.hasMoreHistory = true;
    this.loadingOld = false;
    this.unseenBelow = 0;
    $("btn-bottom").hidden = true;
  }

  /** Détache la conversation affichée dans le cache (session quittée). */
  private stashConv(id: string, conn: string) {
    const frag = document.createDocumentFragment();
    let lastSeq = 0;
    let n0 = 0;
    const atBottom = this.nearBottom();
    const scroll = this.eventsEl.scrollTop; // lu avant détachement (clamp après)
    for (const n of [...this.eventsEl.children]) {
      if (n === this.sentinel || n === this.convLoadingEl) continue;
      const s = Number((n as HTMLElement).dataset.seq ?? 0);
      if (s > lastSeq) lastSeq = s;
      frag.appendChild(n);
      n0++;
    }
    if (!n0) return; // rien d'affiché → rien à garder
    this.convCache.delete(id); // re-set = touche LRU
    this.convCache.set(id, {
      conn,
      frag,
      toolBlocks: this.renderer.toolBlocks,
      pendingToolUpdates: this.renderer.pendingToolUpdates,
      answered: this.renderer.answered,
      minRendered: this.minRendered,
      renderedCount: this.renderedCount,
      hasMoreHistory: this.hasMoreHistory,
      unseenBelow: this.unseenBelow,
      lastSeq,
      scrollTop: scroll,
      atBottom,
      ts: Date.now(),
    });
    while (this.convCache.size > CACHE_MAX) {
      this.convCache.delete(this.convCache.keys().next().value!);
    }
  }

  /** Entrée de cache exploitable (connexion identique + pas expirée).
   *  Consommée au hit : le fragment est transféré au DOM par restoreConv —
   *  laisser l'entrée dans la map laisserait un fragment vide réutilisable. */
  private cachedConv(id: string, conn: string): ConvCache | undefined {
    const c = this.convCache.get(id);
    if (!c) return;
    this.convCache.delete(id);
    if (c.conn !== conn || Date.now() - c.ts > CACHE_TTL) return;
    return c;
  }

  /** Réinstalle une conversation cachée : contenu + position de scroll. */
  private restoreConv(c: ConvCache) {
    for (const n of [...this.eventsEl.children]) {
      if (n !== this.sentinel) n.remove();
    }
    this.eventsEl.appendChild(c.frag);
    this.renderer.toolBlocks = c.toolBlocks;
    this.renderer.pendingToolUpdates = c.pendingToolUpdates;
    this.renderer.answered = c.answered;
    this.minRendered = c.minRendered;
    this.renderedCount = c.renderedCount;
    this.hasMoreHistory = c.hasMoreHistory;
    this.loadingOld = false;
    this.unseenBelow = c.unseenBelow;
    if (c.atBottom) this.scrollBottom();
    else this.eventsEl.scrollTop = c.scrollTop;
    this.updateBottomBtn();
  }

  /** Oublie la conversation cachée d'une session (suppression). */
  dropConv(id: string) {
    this.convCache.delete(id);
    this.tailCache.delete(id);
  }

  /** Purge les conversations cachées des sessions d'une connexion. */
  dropConvsForConn(conn: string) {
    for (const [k, c] of this.convCache) if (c.conn === conn) this.convCache.delete(k);
    for (const [k, tc] of this.tailCache) if (tc.conn === conn) this.tailCache.delete(k);
  }

  /** La session affichée vivait sur `conn` (supprimée) → zone vidée. */
  closeIfConn(conn: string) {
    if (app.current && (app.current.conn ?? "default") === conn) {
      app.current = null;
      this.clearConv();
    }
  }

  /** Réouvre la session affichée si elle vivait sur `conn` (reconnexion). */
  reopenIfConn(conn: string) {
    if (app.current && (app.current.conn ?? "default") === conn) {
      void this.open(app.current.id);
    }
  }

  async open(id: string, force = false) {
    const seq = ++this.openSeq;
    const stale = () => seq !== this.openSeq; // un open plus récent a pris le relais
    // La meta démon ne porte pas sa conn (notion client) — la reprendre de
    // la liste (stampée par le merge) pour router session_info sur le bon
    // serveur ; "" → connexion par défaut.
    const listMeta = app.lastSessions.find((s) => s.id === id);
    const connHint = listMeta?.conn ?? "";

    // Feedback immédiat : la ligne cliquée passe active tout de suite.
    this.deps.setActiveRow(id);
    // La session quittée garde son DOM en mémoire (réaffichage instantané).
    if (app.current && app.current.id !== id) this.stashConv(app.current.id, app.current.conn || "default");
    if (force) this.convCache.delete(id);

    // Bascule immédiate sur la meta connue de la liste — titre + header
    // changent sans attendre session_info (qui la rafraîchit ensuite).
    // Pas de meta → current = null d'emblée : sans ça les pushes de
    // l'ancienne session (sub encore vivante) s'injecteraient dans la zone
    // de la nouvelle pendant le chargement.
    app.current = listMeta ? { ...listMeta } : null;
    app.alive = !!app.current && !app.current.offline &&
      app.current.state !== "dead" && app.current.state !== "interrupted";
    if (app.current) $("chat-title").textContent = `${app.current.title} — ${app.current.agent}`;
    this.updateHeader();

    // Cache frais → conversation réaffichée d'emblée ; sinon zone vidée +
    // grande gemme le temps du rechargement.
    const cached = this.cachedConv(id, connHint || "default");
    if (cached) {
      this.restoreConv(cached);
    } else {
      this.clearConv();
      this.showConvLoading(true);
    }

    const info = await call<{
      meta: SessionMeta; pending: string[]; alive: boolean; last_seq: number;
    }>("session_info", { session: id, conn: connHint }).catch((e) => {
      showErr(`session_info: ${e}`);
      return null;
    });
    if (stale()) return;
    if (!info) {
      // Échec : on garde le cache affiché s'il existe, sinon on retire la gemme.
      this.showConvLoading(false);
      return;
    }
    app.alive = info.alive;
    app.current = info.meta;
    if (!app.current.conn && connHint) app.current.conn = connHint;
    $("chat-title").textContent = `${app.current.title} — ${app.current.agent}`;
    this.updateHeader();

    // Auto-resume : agent mort (ex. restart démon) → reprise silencieuse
    // plutôt qu'un "interrupted" qui demande un clic.
    if (!app.alive && app.current.native_session_id) {
      const sys = document.createElement("div");
      sys.className = "sysline";
      sys.textContent = "⏳ reprise de l'agent…";
      this.eventsEl.appendChild(sys);
      try {
        await call("resume_session", { session: id });
        if (stale()) { sys.remove(); return; }
        const info2 = await call<{ meta: SessionMeta; alive: boolean }>("session_info", {
          session: id,
        });
        if (stale()) { sys.remove(); return; }
        app.alive = info2.alive;
        app.current = info2.meta;
        if (!app.current.conn && connHint) app.current.conn = connHint;
        sys.textContent = "✔ agent repris";
        this.updateHeader();
      } catch (e) {
        sys.textContent = `⚠ reprise: ${e}`;
      }
    }

    // Trou énorme entre le cache et le journal (session qui a tourné
    // fermée) : rejouer ce trou via subscribe saturerait le lien — le
    // démon borne déjà le rattrapage à REPLAY_WINDOW events, mais sans
    // ça la vue garderait un gap au milieu. On jette le rendu restauré
    // et on repart sur la dernière page : tout ce qui précède reste
    // atteignable par la pagination scroll-up (events/before_seq).
    // NB : cachedConv consomme l'entrée — la décision se prend sur
    // `cached`, pas sur un second appel qui ne peut rien retourner.
    const gap = cached != null && info.last_seq - cached.lastSeq > REPLAY_WINDOW;
    if (gap) {
      this.clearConv();
      this.showConvLoading(true);
    }
    const fresh = gap ? undefined : cached;
    // Avec cache, le trou depuis lastSeq est rejoué par le subscribe ; sans
    // cache, on recharge la dernière page — un échec (journal absent/corrompu)
    // ne doit PAS tuer le subscribe ni la vue live derrière.
    let afterSeq = fresh?.lastSeq ?? 0;
    if (!fresh) {
      const tail = this.tailCache.get(id);
      const tailLastSeq = tail?.evs.length ? tail.evs[tail.evs.length - 1].seq : 0;
      if (tail && tailLastSeq && info.last_seq - tailLastSeq <= REPLAY_WINDOW) {
        // Queue brute encore fraîche → réaffichage instantané ; le
        // subscribe rejoue le trou (borné) depuis le dernier seq connu.
        for (const ev of tail.evs) this.appendEvent(ev, false);
        afterSeq = tailLastSeq;
        this.showConvLoading(false);
        this.scrollBottom();
      } else {
        let page = { events: [] as AcEvent[], last_seq: 0 };
        try {
          page = await call<{ events: AcEvent[]; last_seq: number }>("events", {
            session: id,
            limit: INITIAL_PAGE,
          });
        } catch (e) {
          showErr(`events: ${e}`);
        }
        if (stale()) return;
        this.showConvLoading(false);
        for (const ev of page.events) this.appendEvent(ev, false);
        // Page vide (échec events ou journal vide) : partir de la fin de la
        // fenêtre plutôt que 0 — un subscribe(0) resterait rejoué intégral.
        afterSeq = page.events.length
          ? page.events[page.events.length - 1].seq
          : Math.max(0, info.last_seq - REPLAY_WINDOW);
        this.scrollBottom();
      }
    }

    // Subscribe live à partir du dernier event connu
    if (stale()) return;
    await invoke("subscribe", { session: id, afterSeq, conn: app.current?.conn ?? connHint }).catch((e) => {
      // Le flux live est mort (ex. restart démon) → erreur visible
      showErr(`subscribe: ${e}`);
    });
    void this.deps.refresh();
    // L'arbre suit le workspace de la session nouvellement ouverte
    this.deps.onSessionOpened();
  }

  /** La session courante a été supprimée (autre client / janitor) :
   *  vue figée avec marqueur, `current` libéré — les events affichés
   *  restent lisibles mais plus rien ne partira. */
  markGone() {
    if (!app.current) return;
    this.sysline(t("session_gone"));
    this.dropConv(app.current.id);
    app.current = null;
    this.unseenBelow = 0;
    $("btn-bottom").hidden = true;
    $("chat-gem").hidden = true;
    for (const b of ["btn-resume", "btn-cancel", "btn-steer"]) $(b).hidden = true;
    const st = $("chat-state");
    st.textContent = "gone";
    st.className = "badge st-dead";
    this.updateHeader();
  }

  /** Session courante disparue alors que sa connexion répond encore
   *  (sessions du même serveur présentes) → supprimée côté serveur. */
  markGoneIfMissing(list: SessionMeta[]) {
    if (
      app.current &&
      !list.some((s) => s.id === app.current!.id) &&
      list.some((s) => (s.conn ?? "") === (app.current!.conn ?? ""))
    ) {
      this.markGone();
    }
  }

  /** Jauge de contexte dans le header : "53k/200k" + tooltip détaillé. */
  updateCtx() {
    const el = $("chat-ctx");
    const used = app.current?.context_used;
    const size = app.current?.context_size;
    if (used == null || !size) {
      el.hidden = true;
      return;
    }
    const pct = Math.round((used / size) * 100);
    el.hidden = false;
    el.textContent = `${fmtTok(used)}/${fmtTok(size)}`;
    el.title = t("ctx_title", {
      used: used.toLocaleString(),
      size: size.toLocaleString(),
      pct,
    });
    el.classList.toggle("ctx-hot", pct >= 80);
  }

  updateHeader() {
    $("btn-files").hidden = !app.current;
    if (!app.current) {
      $("chat-ctx").hidden = true;
      $("chat-opts").hidden = true;
      return;
    }
    this.deps.refreshQueue();
    this.updateCtx();
    const st = $("chat-state");
    st.textContent = app.current.state;
    st.className = `badge st-${app.current.state}`;
    // Le gem tourne aussi pendant "compacting" (le démon retient les
    // envois en file jusqu'au signal de fin du compact) — en améthyste
    // pour distinguer la compaction d'un vrai tour.
    const gem = $("chat-gem");
    gem.hidden = app.current.state !== "working" && app.current.state !== "compacting";
    const gemRole = app.current.state === "compacting" ? "agent" : "human";
    if (gem.dataset.role !== gemRole) {
      gem.dataset.role = gemRole;
      gem.innerHTML = gemSVG(gemRole);
    }
    // Reprendre = process mort, quel que soit l'état persisté dans meta.json
    $("btn-resume").hidden = app.alive;
    const busySt =
      app.current.state === "working" ||
      app.current.state === "waiting_input" ||
      app.current.state === "compacting";
    $("btn-cancel").hidden = !busySt;
    // Steer : n'a de sens que pendant un vrai tour (sinon = envoi normal ;
    // pendant compact un envoi est simplement retenu en file).
    $("btn-steer").hidden = app.current.state !== "working";

    // Options de config dynamiques (ex. Devin : config_option_update → mode + model)
    const copts = app.current.config_options ?? [];
    const modeOpt = copts.find((o) => o.id === "mode" || o.category === "mode");
    const modelOpt = copts.find((o) => o.id === "model" || o.category === "model");

    const ms = $("mode-select") as HTMLSelectElement;
    if (modeOpt || app.current.available_modes.length) {
      ms.hidden = false;
      ms.innerHTML = "";
      const items = modeOpt
        ? modeOpt.options.map((o) => ({ v: o.value, label: o.name || o.value }))
        : app.current.available_modes.map((m) => ({ v: m, label: m }));
      for (const it of items) {
        const o = document.createElement("option");
        o.value = it.v;
        o.textContent = it.label;
        ms.appendChild(o);
      }
      ms.dataset.cfgid = modeOpt ? modeOpt.id : "";
      ms.value = modeOpt?.currentValue || app.current.mode || "";
    } else {
      ms.hidden = true;
    }

    const md = $("model-select") as HTMLSelectElement;
    const es = $("effort-select") as HTMLSelectElement;
    const mf = $("model-free") as HTMLInputElement;
    const rawOpts = modelOpt
      ? modelOpt.options.map((o) => ({ v: o.value, label: o.name || o.value }))
      : app.current.available_models.map((m) => ({ v: m, label: m }));
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
      // Entrée Fusion : uniquement si l'agent annonce des modèles fusion-*
      // (Devin — cursor/codex n'en ont pas, inutile d'afficher une option KO).
      const hasFusion = rawOpts.some((o) => o.v.startsWith("fusion-"));
      const fo = document.createElement("option");
      fo.value = "__fusion";
      fo.textContent = t("fusion_entry");
      if (hasFusion) md.appendChild(fo);
      md.dataset.cfgid = modelOpt ? modelOpt.id : "";
      md.dataset.opts = JSON.stringify(rawOpts);
      const curVal = modelOpt?.currentValue || app.current.model || "";
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
      es.hidden = curIsFusion || es.options.length <= 1;
    } else {
      md.hidden = true;
      es.hidden = true;
      $("btn-fusion-edit").hidden = true;
      mf.hidden = false;
      mf.value = app.current.model ?? "";
    }

    // Autres options de config exposées par l'agent (ex. Devin : thought_level,
    // speed) — rendues dynamiquement, aucun hardcode.
    const dyn = $("dyn-opts");
    dyn.innerHTML = "";
    for (const o of copts) {
      if (o === modeOpt || o === modelOpt || !(o.options?.length)) continue;
      const sel = document.createElement("select");
      sel.title = o.name || o.id;
      for (const opt of o.options) {
        const el = document.createElement("option");
        el.value = opt.value;
        el.textContent = opt.name || opt.value;
        sel.appendChild(el);
      }
      sel.value = o.currentValue ?? "";
      sel.onchange = () =>
        call("set_config_option", {
          session: app.current!.id,
          config_id: o.id,
          value: sel.value,
        }).catch((e) => showErr(String(e)));
      dyn.appendChild(sel);
    }

    // Aucun contrôle visible → masquer le groupe entier : sinon l'item
    // fantôme ajoute un gap dans le header et peut déclencher un wrap.
    $("chat-opts").hidden =
      ms.hidden && md.hidden && es.hidden && mf.hidden &&
      $("btn-fusion-edit").hidden && dyn.children.length === 0;
  }

  private nearBottom() {
    return this.eventsEl.scrollHeight - this.eventsEl.scrollTop - this.eventsEl.clientHeight < 120;
  }

  scrollBottom() {
    this.eventsEl.scrollTop = this.eventsEl.scrollHeight;
    this.updateBottomBtn();
  }

  private updateBottomBtn() {
    const dist = this.eventsEl.scrollHeight - this.eventsEl.scrollTop - this.eventsEl.clientHeight;
    const b = $("btn-bottom");
    if (dist < 120) {
      b.hidden = true;
      this.unseenBelow = 0;
      return;
    }
    b.hidden = false;
    b.textContent = this.unseenBelow ? `↓ ${this.unseenBelow}` : "↓";
  }

  appendEvent(ev: AcEvent, live: boolean) {
    // La queue enregistre aussi les updates (dedup par seq) — sinon une
    // réouverture via tail rend les tool_call sans leur output streamé.
    if (app.current) this.pushTail(app.current.id, app.current.conn ?? "default", ev);
    // tool_call_update modifie le bloc existant
    if (ev.type === "tool_call_update") {
      this.renderer.updateToolBlock(ev);
      return;
    }
    const nearBottom = this.nearBottom();
    const node = this.renderer.render(ev, live);
    this.eventsEl.appendChild(node);
    this.renderedCount++;
    if (ev.seq < this.minRendered) this.minRendered = ev.seq;
    if (ev.type === "tool_call") {
      this.renderer.toolBlocks.set(ev.call_id as string, node);
      this.renderer.flushToolUpdates(ev.call_id as string);
    }
    if (live && nearBottom) this.scrollBottom();
    if (live && !nearBottom) {
      this.unseenBelow++;
      this.updateBottomBtn();
    }
    this.trimRendered();
  }

  /** Supprime les plus anciens events rendus au-delà du cap (buffer). */
  private trimRendered() {
    while (this.renderedCount > MAX_RENDERED) {
      const first = this.eventsEl.querySelector<HTMLElement>(".ev");
      if (!first) break;
      // retirer aussi son bloc de la map si tool_call
      for (const [k, v] of this.renderer.toolBlocks) if (v === first) this.renderer.toolBlocks.delete(k);
      // Suppression en haut → tout remonterait : compenser pour figer la vue.
      const sh = this.eventsEl.scrollHeight;
      first.remove();
      this.eventsEl.scrollTop -= sh - this.eventsEl.scrollHeight;
      this.renderedCount--;
      this.minRendered = Number(
        this.eventsEl.querySelector<HTMLElement>(".ev")?.dataset.seq ?? Number.MAX_SAFE_INTEGER,
      );
    }
  }

  /** Alimente la queue brute de la session (les derniers TAIL_KEEP events
   *  rendus) — dédup par seq, LRU bornée à TAIL_MAX sessions. */
  private pushTail(id: string, conn: string, ev: AcEvent) {
    let t = this.tailCache.get(id);
    if (!t) {
      t = { conn, evs: [] };
      this.tailCache.set(id, t);
      while (this.tailCache.size > TAIL_MAX) {
        this.tailCache.delete(this.tailCache.keys().next().value!);
      }
    }
    const last = t.evs[t.evs.length - 1];
    if (last && last.seq >= ev.seq) return;
    t.evs.push(ev);
    if (t.evs.length > TAIL_KEEP) t.evs.splice(0, t.evs.length - TAIL_KEEP);
  }

  private prependEvents(events: AcEvent[]) {
    if (!events.length) {
      this.hasMoreHistory = false;
      return;
    }
    const oldHeight = this.eventsEl.scrollHeight;
    // Ancre fixe = premier event déjà rendu : insérer avant elle à chaque
    // tour préserve l'ordre croissant de la page (children[1] bougerait).
    const anchor = this.eventsEl.children[1] ?? null;
    const pageCalls: string[] = [];
    this.renderer.inPrepend = true;
    try {
      for (const ev of events) {
        if (ev.seq < this.minRendered) this.minRendered = ev.seq;
        if (ev.type === "tool_call_update") {
          this.renderer.updateToolBlock(ev); // pas de nœud : met à jour le bloc (ou stash)
          continue;
        }
        const node = this.renderer.render(ev, false);
        this.eventsEl.insertBefore(node, anchor);
        this.renderedCount++;
        if (ev.type === "tool_call") {
          this.renderer.toolBlocks.set(ev.call_id as string, node);
          pageCalls.push(ev.call_id as string);
        }
      }
      // Les updates stashées sont plus récentes que la page → après la boucle.
      for (const id of pageCalls) this.renderer.flushToolUpdates(id);
      // garder la position visuelle
      this.eventsEl.scrollTop += this.eventsEl.scrollHeight - oldHeight;
      this.updateBottomBtn();
    } finally {
      this.renderer.inPrepend = false;
    }
  }

  /** Ligne système dans le fil (retourne l'élément pour mises à jour). */
  sysline(text: string): HTMLElement {
    const d = document.createElement("div");
    d.className = "ev sysline";
    d.textContent = `⦿ ${text}`;
    this.eventsEl.appendChild(d);
    this.scrollBottom();
    return d;
  }

  init() {
    // Scroll : chargement des pages anciennes via sentinel
    const observer = new IntersectionObserver(async (entries) => {
      if (!entries[0].isIntersecting || this.loadingOld || !app.current || !this.hasMoreHistory ||
          this.convLoadingEl?.isConnected) return;
      const sid = app.current.id;
      this.loadingOld = true;
      try {
        const page = await call<{ events: AcEvent[] }>("events", {
          session: sid,
          before_seq: this.minRendered,
          limit: PAGE,
        });
        // Session changée pendant l'await → ne pas insérer l'ancienne page.
        if (app.current?.id === sid) this.prependEvents(page.events);
      } finally {
        this.loadingOld = false;
      }
    });
    observer.observe(this.sentinel);

    this.eventsEl.addEventListener("scroll", () => this.updateBottomBtn());
    $("btn-bottom").onclick = () => {
      this.eventsEl.scrollTo({ top: this.eventsEl.scrollHeight, behavior: "smooth" });
    };
    // Image cassée (404, hôte inaccessible…) → on la remplace par son texte/URL,
    // l'<a> englobant reste cliquable. capture=true : error ne bubble pas.
    this.eventsEl.addEventListener(
      "error",
      (e) => {
        const img = e.target;
        if (img instanceof HTMLImageElement && img.classList.contains("md-img"))
          img.replaceWith(img.alt || img.src);
      },
      true,
    );
  }
}
