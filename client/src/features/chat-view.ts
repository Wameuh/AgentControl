// ---------------------------------------------------------------------------
// Vue conversation : ouverture d'une session (session_info + dernière page +
// subscribe), cache mémoire des conversations, pagination au scroll,
// header (titre, état, jauge contexte, selects) et ligne système.
// Le rendu des events eux-mêmes est délégué à EventRenderer.
// ---------------------------------------------------------------------------

import { invoke } from "../core/bridge";
import { $, esc, showErr } from "../core/dom";
import { fmtTok } from "../core/format";
import { call } from "../core/rpc";
import { app } from "../core/state";
import type { AcEvent, SessionMeta } from "../core/types";
import { gemSVG } from "../crystal";
import { t } from "../i18n";
import { icon, type IconName } from "../ui/icons";
import { nav } from "../ui/nav";
import { fillEfforts, splitModel } from "./modelbar";
import type { EventRenderer } from "./event-renderers";

const MAX_RENDERED = 400;
const PAGE = 50; // paquets au scroll
const INITIAL_PAGE = 200; // base d'historique à l'ouverture
const JUMP_BEFORE = 40; // contexte rendu avant un résultat de recherche
const JUMP_AFTER = 40; // … et après
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
  /** Session native au moment du stash : un reset de journal (/clear,
   *  /new, rebind) en crée une nouvelle → l'entrée devient invalide. */
  nativeId?: string;
  frag: DocumentFragment;
  toolBlocks: Map<string, HTMLElement>;
  pendingToolUpdates: Map<string, AcEvent[]>;
  answered: Set<string>;
  lastText: { thought: boolean; norm: string; el: HTMLElement } | null;
  userMsgs: number;
  headSeen: boolean;
  minRendered: number;
  renderedCount: number;
  hasMoreHistory: boolean;
  /** Vue ancrée en plein journal (saut de recherche) : du contenu plus
   *  récent existe après le dernier seq rendu. */
  hasMoreNewer: boolean;
  /** Events live tamponnés pendant que la vue est ancrée. */
  pendingLive: AcEvent[];
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
  private tailCache = new Map<string, { conn: string; evs: AcEvent[]; nativeId?: string }>();
  private minRendered = Number.MAX_SAFE_INTEGER;
  private maxRendered = 0;
  private renderedCount = 0;
  private loadingPage = false;
  private hasMoreHistory = true;
  private hasMoreNewer = false;
  private pendingLive: AcEvent[] = [];
  private jumpSeq = 0; // invalidation des sauts de recherche en vol
  private loadSeq = 0; // propriété du load en cours : un finally périmé ne doit pas libérer le flag d'un load plus récent
  private unseenBelow = 0;
  private convLoadingEl: HTMLElement | null = null;
  /** Hôtes en cours d'auto-connexion (écran vide "connexion à…"). */
  private connectingHosts: string[] = [];

  private deps = {
    /** Marque la ligne de session active dans la sidebar. */
    setActiveRow: (_id: string) => {},
    /** Rafraîchit la sidebar. */
    refresh: (): Promise<void> => Promise.resolve(),
    /** Barre de file (prompts en attente) — rafraîchie par updateHeader. */
    refreshQueue: () => {},
    /** Bouton d'envoi alternatif (steer ↔ file selon la préférence) —
     *  icône + visibilité resynchronisés par updateHeader. */
    syncAltBtn: () => {},
    /** Bouton « premier post » affiché seulement si le démon de la conn
     *  connaît first_post. */
    firstPostCapable: (_conn: string) => false,
    /** Bouton « épinglés » + actions pin sur les messages — même gate
     *  capability (daemon_status.pinned). */
    pinCapable: (_conn: string) => false,
    /** Marque l'état épinglé d'un nœud rendu (feature pins). */
    pinsMark: (_el: HTMLElement) => {},
    /** L'arbre fichiers suit le workspace de la session ouverte. */
    onSessionOpened: () => {},
    /** Session affichée changée ou disparue (suppression, conn coupée,
     *  journal vidé) — la recherche ferme, ses résultats sont d'un
     *  autre journal. */
    onSessionChanged: () => {},
    /** Session affichée disparue pour de bon (markGone — toutes
     *  origines confondues) : purge de son état volatil (brouillon du
     *  composer, pièces garées). */
    onSessionGone: (_id: string) => {},
    /** Nb de connexions connues — pour l'écran d'accueil (injecté). */
    connCount: (): number => 0,
    /** Aucune conn bound, ou toutes hors ligne/déconnectées → la vue
     *  connexion devient l'écran d'accueil (empty-state + CTA derrière). */
    connsDownOrNone: (): boolean => true,
    /** Demande à la vue connexion de s'auto-ouvrir en accueil (respecte
     *  son flag « repoussée par l'utilisateur »). */
    connHome: () => {},
    /** ⌨ affiché seulement si le démon de la conn connaît term_open. */
    termCapable: (_conn: string) => false,
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
        `<span class="load-gem">${gemSVG("agent")}</span>` +
        `<span class="cl-ico" hidden>${icon("warn")}</span>` +
        `<span class="cl-txt">${esc(t("loading"))}</span>` +
        `<button class="cl-retry" hidden>${esc(t("retry"))}</button>`;
    }
    if (v) {
      // Mode chargement : gemme visible, état d'erreur masqué.
      this.convLoadingEl.querySelector(".load-gem")!.removeAttribute("hidden");
      this.convLoadingEl.querySelector(".cl-ico")!.setAttribute("hidden", "");
      const txt = this.convLoadingEl.querySelector(".cl-txt")!;
      txt.textContent = t("loading");
      this.convLoadingEl.querySelector(".cl-retry")!.setAttribute("hidden", "");
      this.eventsEl.appendChild(this.convLoadingEl);
    } else {
      this.convLoadingEl.remove();
    }
  }

  /** Échec de chargement sans cache : message centré + bouton Réessayer. */
  private showConvError(msg: string) {
    this.showConvLoading(true);
    const el = this.convLoadingEl!;
    el.querySelector(".load-gem")!.setAttribute("hidden", "");
    el.querySelector(".cl-ico")!.removeAttribute("hidden");
    el.querySelector(".cl-txt")!.textContent = `${t("conv_load_fail")} — ${msg}`;
    const retry = el.querySelector<HTMLButtonElement>(".cl-retry")!;
    retry.removeAttribute("hidden");
    retry.onclick = () => {
      const id = app.current?.id;
      if (id) void this.open(id, true);
    };
  }

  /** Écran d'accueil dans #events : aucune session affichée → gemme +
   *  contexte (connexion en cours / aucun serveur / aucune session) et
   *  l'action qui débloque (Connecter / Nouvel agent). Re-rendu par
   *  sidebar.refresh() et les transitions de connexion. */
  updateEmptyState() {
    const el = $("empty-state");
    const hasConv =
      !!app.current || this.eventsEl.querySelector(".ev") != null;
    if (hasConv || this.convLoadingEl?.isConnected) {
      el.hidden = true;
      return;
    }
    const nConns = this.deps.connCount();
    const nSess = app.lastSessions.length;
    const h = $("empty-h");
    const txt = $("empty-txt");
    const cta = $("empty-cta") as HTMLButtonElement;
    el.classList.remove("spin");
    cta.hidden = true;
    if (this.connectingHosts.length) {
      el.hidden = false;
      el.classList.add("spin");
      h.textContent = t("empty_connecting", { host: this.connectingHosts.join(", ") });
      txt.textContent = "";
    } else if (!nConns || this.deps.connsDownOrNone()) {
      // Aucune conn joignable (rien de bound ou toutes down/gone) → la vue
      // connexion est l'écran d'accueil (mono-connexion) : elle s'auto-ouvre
      // si l'utilisateur ne l'a pas repoussée ; l'empty-state derrière
      // garde le rappel + le CTA.
      this.deps.connHome();
      el.hidden = false;
      h.textContent = t("empty_h");
      txt.textContent = nConns ? t("empty_conn_down") : t("empty_no_conn");
      cta.hidden = false;
      cta.textContent = nConns ? t("open_conns") : t("new_conn");
      cta.onclick = () => {
        nav.closeDrawer();
        $("btn-connect").click();
      };
    } else if (!nSess) {
      el.hidden = false;
      h.textContent = t("empty_h");
      txt.textContent = t("empty_no_sess");
      cta.hidden = false;
      cta.textContent = t("new_session");
      cta.onclick = () => {
        nav.closeDrawer();
        $("btn-new-session").click();
      };
    } else {
      el.hidden = true;
    }
  }

  /** Auto-connexion au boot : les hôtes en cours apparaissent sur l'écran
   *  vide ; [] = terminé (retour à l'état calculé). */
  setConnecting(hosts: string[]) {
    this.connectingHosts = hosts;
    this.updateEmptyState();
  }

  /** Éléments décoratifs conservés au vidage/stash (sentinelle + accueil). */
  private isDecor(n: Element) {
    return n === this.sentinel || n.id === "empty-state";
  }

  /** Vide la zone (sentinelle + empty-state conservés) et réinitialise
   *  l'état de rendu. */
  clearConv() {
    for (const n of [...this.eventsEl.children]) {
      if (!this.isDecor(n)) n.remove();
    }
    this.renderer.toolBlocks = new Map();
    this.renderer.pendingToolUpdates = new Map();
    this.renderer.answered = new Set();
    this.renderer.lastText = null;
    this.renderer.userMsgs = 0;
    this.renderer.headSeen = false;
    this.minRendered = Number.MAX_SAFE_INTEGER;
    this.maxRendered = 0;
    this.renderedCount = 0;
    this.hasMoreHistory = true;
    this.hasMoreNewer = false;
    this.pendingLive = [];
    this.loadingPage = false;
    this.unseenBelow = 0;
    this.jumpSeq++; // un saut de recherche en vol ne doit plus réinjecter sa fenêtre
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
      if (this.isDecor(n) || n === this.convLoadingEl) continue;
      const s = Number((n as HTMLElement).dataset.seq ?? 0);
      if (s > lastSeq) lastSeq = s;
      frag.appendChild(n);
      n0++;
    }
    if (!n0) return; // rien d'affiché → rien à garder
    this.convCache.delete(id); // re-set = touche LRU
    this.convCache.set(id, {
      conn,
      nativeId: app.current?.native_session_id ?? undefined,
      frag,
      toolBlocks: this.renderer.toolBlocks,
      pendingToolUpdates: this.renderer.pendingToolUpdates,
      answered: this.renderer.answered,
      lastText: this.renderer.lastText,
      userMsgs: this.renderer.userMsgs,
      headSeen: this.renderer.headSeen,
      minRendered: this.minRendered,
      renderedCount: this.renderedCount,
      hasMoreHistory: this.hasMoreHistory,
      hasMoreNewer: this.hasMoreNewer,
      pendingLive: this.pendingLive,
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
      if (!this.isDecor(n)) n.remove();
    }
    this.eventsEl.appendChild(c.frag);
    this.renderer.toolBlocks = c.toolBlocks;
    this.renderer.pendingToolUpdates = c.pendingToolUpdates;
    this.renderer.answered = c.answered;
    this.renderer.lastText = c.lastText;
    this.renderer.userMsgs = c.userMsgs;
    this.renderer.headSeen = c.headSeen;
    this.minRendered = c.minRendered;
    this.maxRendered = c.lastSeq; // dernier seq rendu = borne haute du cache
    this.renderedCount = c.renderedCount;
    this.hasMoreHistory = c.hasMoreHistory;
    this.hasMoreNewer = c.hasMoreNewer;
    this.pendingLive = c.pendingLive;
    this.loadingPage = false;
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
      this.deps.onSessionChanged();
      this.updateEmptyState();
    }
  }

  /** Réouvre la session affichée si elle vivait sur `conn` (reconnexion). */
  reopenIfConn(conn: string) {
    if (app.current && (app.current.conn ?? "default") === conn) {
      void this.open(app.current.id);
    }
  }

  async open(id: string, force = false, conn = "") {
    const seq = ++this.openSeq;
    const stale = () => seq !== this.openSeq; // un open plus récent a pris le relais
    const prevId = app.current?.id;
    // La meta démon ne porte pas sa conn (notion client) — la reprendre de
    // la liste (stampée par le merge) pour router session_info sur le bon
    // serveur ; "" → connexion par défaut.
    const listMeta = app.lastSessions.find((s) => s.id === id);
    const connHint = listMeta?.conn ?? conn;

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
      app.current.state !== "dead" && app.current.state !== "interrupted" &&
      app.current.state !== "asleep";
    // Session affichée différente → la recherche se ferme : ses résultats
    // indexent le journal de l'ancienne session, pas celui de la nouvelle.
    if (app.current?.id !== prevId) this.deps.onSessionChanged();
    this.setChatTitle();
    this.updateHeader();
    // L'écran d'accueil disparaît dès qu'une session est ouverte.
    $("empty-state").hidden = true;

    // Cache frais → conversation réaffichée d'emblée ; sinon zone vidée +
    // grande gemme le temps du rechargement.
    const cached = this.cachedConv(id, connHint || "default");
    if (cached) {
      this.restoreConv(cached);
    } else {
      this.clearConv();
      this.showConvLoading(true);
    }

    let lastErr: unknown = null;
    const info = await call<{
      meta: SessionMeta; pending: string[]; alive: boolean; last_seq: number;
    }>("session_info", { session: id, conn: connHint }).catch((e) => {
      lastErr = e;
      showErr(`session_info: ${e}`);
      return null;
    });
    if (stale()) return;
    if (!info) {
      // Échec : le cache restauré reste affiché ; sans rien à l'écran,
      // état d'erreur centré + bouton Réessayer (plutôt qu'une zone vide).
      if (cached) this.showConvLoading(false);
      else this.showConvError(String(lastErr));
      return;
    }
    app.alive = info.alive;
    // listMeta absente → current était null pendant le chargement : la
    // session affichée ne devient celle-ci qu'ici — refire le
    // changement (brouillon du composer, recherche, chips garés).
    const shownId = app.current?.id;
    app.current = info.meta;
    if (!app.current.conn && connHint) app.current.conn = connHint;
    if (app.current.id !== shownId) this.deps.onSessionChanged();
    this.setChatTitle();
    this.updateHeader();

    // Auto-resume : agent mort (ex. restart démon) → reprise silencieuse
    // plutôt qu'un "interrupted" qui demande un clic. Sauf "asleep" :
    // consulter l'historique d'une session endormie ne doit pas réveiller
    // l'agent — le prochain envoi le fera (h_send → respawn côté démon).
    // Ni "starting" : le spawn est déjà en vol — un resume ici ne ferait
    // qu'attendre le spawn_lock derrière lui (sysline « reprise »
    // trompeuse pendant le boot normal).
    if (!app.alive && app.current.native_session_id &&
        app.current.state !== "asleep" && app.current.state !== "starting") {
      const sys = document.createElement("div");
      sys.className = "sysline";
      sys.innerHTML = `${icon("loader")} ${esc(t("resume_wait"))}`;
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
        sys.innerHTML = `${icon("check")} ${esc(t("resume_ok"))}`;
        this.updateHeader();
      } catch (e) {
        sys.innerHTML = `${icon("warn")} ${esc(`${t("resume_fail")} ${e}`)}`;
      }
    }

    // Journal réinitialisé depuis la prise du cache (/clear, /new,
    // rebind) : l'entrée contient un AUTRE cycle. Détection : l'id de
    // session native a changé (le respawn en recrée une) ou le seq
    // connu dépasse la fin du journal actuel. Sans ça l'ancien
    // contenu était restauré et le subscribe(afterSeq obsolète)
    // restait muet — le symptôme « /clear ne change rien ».
    const nativeId = info.meta.native_session_id ?? undefined;
    const staleCache = (e?: { nativeId?: string; lastSeq: number }) =>
      !!e && (e.nativeId !== nativeId || e.lastSeq > info.last_seq);
    // Trou énorme entre le cache et le journal (session qui a tourné
    // fermée) : rejouer ce trou via subscribe saturerait le lien — le
    // démon borne déjà le rattrapage à REPLAY_WINDOW events, mais sans
    // ça la vue garderait un gap au milieu. On jette le rendu restauré
    // et on repart sur la dernière page : tout ce qui précède reste
    // atteignable par la pagination scroll-up (events/before_seq).
    // NB : cachedConv consomme l'entrée — la décision se prend sur
    // `cached`, pas sur un second appel qui ne peut rien retourner.
    let journalReset = staleCache(cached);
    const gap = cached != null && !journalReset && info.last_seq - cached.lastSeq > REPLAY_WINDOW;
    if (gap || journalReset) {
      this.clearConv();
      this.showConvLoading(true);
    }
    const fresh = gap || journalReset ? undefined : cached;
    // Avec cache, le trou depuis lastSeq est rejoué par le subscribe ; sans
    // cache, on recharge la dernière page — un échec (journal absent/corrompu)
    // ne doit PAS tuer le subscribe ni la vue live derrière.
    let afterSeq = fresh?.lastSeq ?? 0;
    if (!fresh) {
      let tail = this.tailCache.get(id);
      const tailLastSeq = tail?.evs.length ? tail.evs[tail.evs.length - 1].seq : 0;
      if (tail && staleCache({ nativeId: tail.nativeId, lastSeq: tailLastSeq })) {
        this.tailCache.delete(id);
        tail = undefined;
        journalReset = true;
      }
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
    // Journal réinitialisé (native changé) → les résultats de recherche
    // indexent l'ancien journal (seqs reparties à 1) → fermer la recherche.
    if (journalReset) this.deps.onSessionChanged();
    void this.deps.refresh();
    // L'arbre suit le workspace de la session nouvellement ouverte
    this.deps.onSessionOpened();
  }

  /** Journal vidé côté démon (/clear, /new, rebind — poussé via
   *  session_cleared) : la vue affichait l'ancien cycle. Elle est
   *  vidée ; les events du nouveau cycle (seqs reparties à 1)
   *  arrivent derrière et remplissent la conversation neuve. La méta
   *  est rechargée au mieux : tant que le respawn n'a pas abouti,
   *  `native_session_id` peut rester absent — la queue repoussée
   *  serait alors estampillée du mauvais id et jetée à la prochaine
   *  ouverture (repli conservateur, contenu toujours correct). */
  async convCleared() {
    this.clearConv();
    this.scrollBottom();
    this.deps.onSessionChanged(); // les résultats pointent vers l'ancien journal
    if (!app.current) return;
    const sid = app.current.id;
    const info = await call<{ meta: SessionMeta }>("session_info", {
      session: sid,
    }).catch(() => null);
    if (info && app.current?.id === sid) {
      app.current = info.meta;
      this.updateHeader();
    }
  }

  /** La session courante a été supprimée (autre client / janitor) :
   *  vue figée avec marqueur, `current` libéré — les events affichés
   *  restent lisibles mais plus rien ne partira. */
  markGone() {
    if (!app.current) return;
    this.sysline(t("session_gone"), "warn");
    const goneId = app.current.id;
    this.dropConv(goneId);
    app.current = null;
    this.deps.onSessionChanged();
    this.deps.onSessionGone(goneId);
    this.unseenBelow = 0;
    $("btn-bottom").hidden = true;
    $("chat-gem").hidden = true;
    for (const b of ["btn-resume", "btn-cancel", "btn-alt"]) $(b).hidden = true;
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

  /** "titre — agent" dans le header. La garde `tl` compte : un renommage
   *  inline en cours remplace #chat-title par un input → l'écriture est
   *  un no-op (done() réécrira via cette même méthode) plutôt qu'un
   *  crash qui avorterait open() en plein milieu. */
  setChatTitle() {
    const tl = document.getElementById("chat-title");
    if (tl && app.current) tl.textContent = `${app.current.title} — ${app.current.agent}`;
  }

  updateHeader() {
    $("btn-files").hidden = !app.current;
    $("btn-search").hidden = !app.current;
    $("btn-first").hidden =
      !app.current || !this.deps.firstPostCapable(app.current?.conn ?? "default");
    $("btn-pins").hidden =
      !app.current || !this.deps.pinCapable(app.current?.conn ?? "default");
    $("btn-term").hidden =
      !app.current || !this.deps.termCapable(app.current?.conn ?? "default");
    if (!app.current) {
      $("chat-ctx").hidden = true;
      $("chat-opts").hidden = true;
      this.deps.syncAltBtn(); // sans session → bouton alternatif caché
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
    // Reprendre = process mort, quel que soit l'état persisté dans
    // meta.json — sauf "starting" : le spawn tourne déjà (afficher
    // Reprendre suggérerait un échec alors que le boot est en cours).
    $("btn-resume").hidden = app.alive || app.current.state === "starting";
    const busySt =
      app.current.state === "working" ||
      app.current.state === "waiting_input" ||
      app.current.state === "compacting";
    $("btn-cancel").hidden = !busySt;
    // Bouton alternatif (steer ↔ file selon la préférence d'envoi) :
    // rôle + visibilité calculés côté composer.
    this.deps.syncAltBtn();

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
    if (dist < 120 && !this.hasMoreNewer) {
      b.hidden = true;
      this.unseenBelow = 0;
      return;
    }
    b.hidden = false;
    // Vue ancrée : skip-down = retour au présent (recharge la queue du
    // journal) ; sinon down = simple scroll au dernier message rendu.
    b.title = t(this.hasMoreNewer ? "bottom_jump_title" : "bottom_title");
    b.innerHTML =
      icon(this.hasMoreNewer ? "skip-down" : "down") +
      (this.unseenBelow ? `<span class="cnt">${this.unseenBelow}</span>` : "");
  }

  appendEvent(ev: AcEvent, live: boolean) {
    // La queue enregistre aussi les updates (dedup par seq) — sinon une
    // réouverture via tail rend les tool_call sans leur output streamé.
    if (app.current) this.pushTail(app.current.id, app.current.conn ?? "default", ev);
    // Vue ancrée (saut de recherche) : l'event live n'est pas contigu avec
    // le rendu — il est tamponné jusqu'au retour au présent ; le compteur
    // du bouton ⇥ reflète ce qui attend.
    if (live && this.hasMoreNewer) {
      const lastP = this.pendingLive[this.pendingLive.length - 1];
      if (!lastP || ev.seq > lastP.seq) this.pendingLive.push(ev);
      this.unseenBelow++;
      this.updateBottomBtn();
      return;
    }
    if (ev.seq > this.maxRendered) this.maxRendered = ev.seq;
    // tool_call_update modifie le bloc existant
    if (ev.type === "tool_call_update") {
      this.renderer.updateToolBlock(ev);
      return;
    }
    // Anti-doublon pensée/message : le même texte que le bloc texte
    // agent précédent (les events intermédiaires n'y coupent pas) n'est
    // pas rendu une seconde fois ; une pensée déjà affichée cède sa
    // place au message identique qui la suit (contenu canonique).
    if (ev.type === "agent_text" || ev.type === "agent_thought") {
      const dup = this.renderer.textDup(ev);
      if (dup === "skip") return;
      if (dup && dup.isConnected) this.dropRendered(dup);
    }
    const nearBottom = this.nearBottom();
    const node = this.renderer.render(ev, live);
    this.eventsEl.appendChild(node);
    this.deps.pinsMark(node);
    this.renderer.noteText(ev, node);
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

  /** Retire un nœud déjà rendu — pensée doublon remplacée par le message
   *  identique (dedup). Même bookkeeping que trimRendered : compteur,
   *  borne min, compensation de scroll si le nœud est entièrement
   *  au-dessus du viewport. */
  private dropRendered(d: HTMLElement) {
    const above =
      d.getBoundingClientRect().bottom < this.eventsEl.getBoundingClientRect().top;
    const sh = this.eventsEl.scrollHeight;
    d.remove();
    if (above) this.eventsEl.scrollTop -= sh - this.eventsEl.scrollHeight;
    this.renderedCount--;
    if (Number(d.dataset.seq ?? 0) <= this.minRendered) {
      this.minRendered = Number(
        this.eventsEl.querySelector<HTMLElement>(".ev")?.dataset.seq ??
          Number.MAX_SAFE_INTEGER,
      );
    }
  }

  /** Supprime les plus anciens events rendus au-delà du cap (buffer).
   *  Un nœud n'est retiré qu'entièrement hors de vue avec un écran de
   *  marge : sans ça, garé dans l'historique, le prochain event live
   *  aspirait par le haut les pages tout juste chargées (tapis roulant
   *  — « le scroll-up ne charge rien »). Les nœuds hidden (rect nul)
   *  restent retirables : invisibles, jamais sous les yeux.
   *  Conséquence assumée : garé en tête, le cap devient mou — le DOM
   *  peut dépasser MAX_RENDERED tant que l'utilisateur y lit. */
  private trimRendered() {
    const topLimit =
      this.eventsEl.getBoundingClientRect().top - this.eventsEl.clientHeight;
    while (this.renderedCount > MAX_RENDERED) {
      const first = this.eventsEl.querySelector<HTMLElement>(".ev");
      if (!first) break;
      if (!first.hidden && first.getBoundingClientRect().bottom > topLimit) break;
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
      t = { conn, evs: [], nativeId: app.current?.native_session_id ?? undefined };
      this.tailCache.set(id, t);
      while (this.tailCache.size > TAIL_MAX) {
        this.tailCache.delete(this.tailCache.keys().next().value!);
      }
    }
    // L'id natif peut être (ré)assigné après la création de l'entrée —
    // toujours le recaler sur la méta courante pour refléter le cycle
    // des events poussés.
    t.nativeId = app.current?.native_session_id ?? undefined;
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
    // Ancre fixe = #empty-state (décor, toujours en position 1 entre la
    // sentinelle et le 1er .ev) : insérer avant elle à chaque tour
    // préserve l'ordre croissant de la page (children[1] bougerait).
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
        this.deps.pinsMark(node);
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

  // -------------------------------------------------------------------------
  // Saut de recherche : vue ancrée en plein journal + pagination avant
  // -------------------------------------------------------------------------

  /** Centre la vue sur un seq (résultat de recherche). Nœud déjà rendu →
   *  scroll simple ; sinon fenêtre chargée autour du seq — la vue devient
   *  « ancrée » : pagination avant au scroll bas, events live tamponnés. */
  async jumpToSeq(seq: number): Promise<boolean> {
    if (!app.current) return false;
    const sid = app.current.id;
    const js = ++this.jumpSeq;
    const node = () => this.eventsEl.querySelector<HTMLElement>(`.ev[data-seq="${seq}"]`);
    const hit = node();
    if (hit) {
      this.scrollToNode(hit);
      return true;
    }
    // Tamponner les events live pendant le chargement de la fenêtre :
    // hasMoreNewer déroute appendEvent vers pendingLive — rien ne se
    // perd ni ne s'insère dans l'ancienne vue avant le clearConv.
    const wasAnchored = this.hasMoreNewer;
    this.hasMoreNewer = true;
    const pending = this.pendingLive;
    try {
      const [before, after] = await Promise.all([
        call<{ events: AcEvent[] }>("events", {
          session: sid,
          before_seq: seq + 1, // exclusif → la page se termine sur le match
          limit: JUMP_BEFORE,
        }),
        call<{ events: AcEvent[]; last_seq: number }>("events", {
          session: sid,
          after_seq: seq,
          limit: JUMP_AFTER,
        }),
      ]);
      if (js !== this.jumpSeq || app.current?.id !== sid) {
        // Un saut plus récent ou un open a repris la main — hasMoreNewer/
        // pendingLive appartiennent à ce flux, ne pas les écraser.
        return false;
      }
      this.clearConv();
      for (const ev of [...before.events, ...after.events]) this.appendEvent(ev, false);
      // Tampon live accumulé pendant l'await : les events déjà couverts
      // par la fenêtre sautée sont droppés ; un seq > last_seq lu (event
      // arrivé après la requête) reste en attente et compte comme
      // contenu plus récent → la vue reste ancrée jusqu'à ⇥.
      this.pendingLive = pending.filter((e) => e.seq > this.maxRendered);
      this.unseenBelow = this.pendingLive.length;
      this.hasMoreNewer =
        this.maxRendered < after.last_seq || this.pendingLive.length > 0;
      if (!this.hasMoreNewer) this.flushPendingLive();
      const target = node() ?? this.closestEv(seq);
      if (target) this.scrollToNode(target);
      else this.scrollBottom();
      return target != null;
    } catch (e) {
      // Échec avant tout clearConv : l'ancienne vue est intacte —
      // restaurer son état ; si elle était live, les events tamponnés
      // pendant l'await sont déversés plutôt que perdus.
      this.hasMoreNewer = wasAnchored;
      if (!this.hasMoreNewer) this.flushPendingLive();
      this.updateBottomBtn();
      showErr(`events: ${e}`);
      return false;
    }
  }

  /** Le bloc rendu le plus proche d'un seq — repli quand le match n'a pas
   *  de nœud propre (tool_call_update fusionné dans son bloc parent). */
  private closestEv(seq: number): HTMLElement | null {
    let best: HTMLElement | null = null;
    let bd = Number.MAX_SAFE_INTEGER;
    for (const n of this.eventsEl.querySelectorAll<HTMLElement>(".ev")) {
      const d = Math.abs(Number(n.dataset.seq ?? 0) - seq);
      if (d < bd) {
        bd = d;
        best = n;
      }
    }
    return best;
  }

  private scrollToNode(node: HTMLElement) {
    node.scrollIntoView({ block: "center" });
    node.classList.remove("hit-flash");
    void node.offsetWidth; // reflow : rejouer l'anim à chaque saut
    node.classList.add("hit-flash");
    this.updateBottomBtn();
  }

  /** Page suivante vers le présent (scroll bas en vue ancrée). */
  private async loadNewerPage() {
    if (!app.current || this.loadingPage || !this.hasMoreNewer) return;
    const sid = app.current.id;
    const js = this.jumpSeq;
    const ls = ++this.loadSeq;
    this.loadingPage = true;
    try {
      const page = await call<{ events: AcEvent[]; last_seq: number }>("events", {
        session: sid,
        after_seq: this.maxRendered,
        limit: PAGE,
      });
      if (app.current?.id !== sid || js !== this.jumpSeq) return;
      for (const ev of page.events) this.appendEvent(ev, false);
      if (this.maxRendered >= page.last_seq) {
        this.hasMoreNewer = false;
        this.flushPendingLive();
      }
    } finally {
      if (ls === this.loadSeq) this.loadingPage = false;
    }
  }

  /** Bouton ⇥ : recharge la queue du journal — sortie du mode ancré. */
  private async loadLatest() {
    if (!app.current) return;
    const sid = app.current.id;
    const pending = this.pendingLive;
    this.clearConv();
    this.hasMoreNewer = true; // les events live restent tamponnés pendant l'await
    const js = this.jumpSeq; // post-clearConv : un saut ultérieur invalide ce load
    this.showConvLoading(true);
    try {
      const page = await call<{ events: AcEvent[]; last_seq: number }>("events", {
        session: sid,
        limit: INITIAL_PAGE,
      });
      if (app.current?.id !== sid || js !== this.jumpSeq) return;
      for (const ev of page.events) this.appendEvent(ev, false);
      // Les events live arrivés pendant l'await sont restés tamponnés
      // dans pendingLive (clearConv l'a recréée) → concat, pas écrasement.
      this.pendingLive = [...pending, ...this.pendingLive];
      this.hasMoreNewer = false;
      this.scrollBottom();
      this.flushPendingLive();
    } catch (e) {
      // Échec : la vue est vide — les events tamponnés avant l'appel
      // retournent dans le buffer, ⇥ reste actif pour réessayer.
      this.pendingLive = [...pending, ...this.pendingLive];
      this.unseenBelow = this.pendingLive.length;
      this.updateBottomBtn();
      showErr(`events: ${e}`);
    } finally {
      this.showConvLoading(false);
    }
  }

  /** Déverse les events tamponnés en vue ancrée — ceux déjà recouverts
   *  par la fenêtre sont filtrés par seq (live append les rend en bas). */
  private flushPendingLive() {
    const rest = this.pendingLive.filter((e) => e.seq > this.maxRendered);
    this.pendingLive = [];
    for (const ev of rest) this.appendEvent(ev, true);
  }

  /** Ligne système dans le fil (retourne l'élément pour mises à jour). */
  sysline(text: string, ic: IconName = "sys"): HTMLElement {
    const d = document.createElement("div");
    d.className = "ev sysline";
    d.innerHTML = `${icon(ic)} ${esc(text)}`;
    this.eventsEl.appendChild(d);
    this.scrollBottom();
    return d;
  }

  /** La sentinelle (tête de #events) est-elle dans le scrollport ? */
  private sentinelVisible() {
    const s = this.sentinel.getBoundingClientRect();
    const r = this.eventsEl.getBoundingClientRect();
    return s.bottom > r.top && s.top < r.bottom;
  }

  /** Charge les pages anciennes tant que la sentinelle reste visible.
   *  Une page peut ne produire AUCUN nœud — les rafales de
   *  tool_call_update (streaming d'outputs, ~50+ events d'affilée)
   *  n'ont pas de rendu : sans boucle, la sentinelle resterait dans le
   *  viewport et l'observer ne se redéclencherait jamais (il ne tire
   *  que sur changement d'intersection) → scroll-up mort. */
  private async loadOlderPages() {
    if (this.loadingPage || !app.current || !this.hasMoreHistory ||
        this.convLoadingEl?.isConnected || !this.sentinelVisible()) return;
    const sid = app.current.id;
    const js = this.jumpSeq;
    const ls = ++this.loadSeq;
    this.loadingPage = true;
    try {
      while (this.hasMoreHistory && this.sentinelVisible()) {
        const prevMin = this.minRendered;
        const page = await call<{ events: AcEvent[] }>("events", {
          session: sid,
          before_seq: this.minRendered,
          limit: PAGE,
        });
        // Session changée ou saut de recherche pendant l'await → la page
        // est périmée pour la fenêtre reconstruite, ne pas l'insérer.
        if (app.current?.id !== sid || js !== this.jumpSeq) return;
        this.prependEvents(page.events);
        // Page courte = tête du journal atteinte (trous de compaction
        // inclus : le démon rend ce qui reste, jamais plus) — rien de
        // plus ancien à chercher.
        if (page.events.length < PAGE) this.hasMoreHistory = false;
        // Garde-fou : la progression repose sur le contrat « page
        // strictement < before_seq » → minRendered décroît à chaque
        // tour ; sans ça la boucle enverrait des RPC à l'infini.
        if (this.minRendered >= prevMin) break;
      }
    } catch (e) {
      showErr(`events: ${e}`);
    } finally {
      if (ls === this.loadSeq) this.loadingPage = false;
    }
  }

  init() {
    // Scroll : chargement des pages anciennes via sentinel
    const observer = new IntersectionObserver((entries) => {
      if (entries[0].isIntersecting) void this.loadOlderPages();
    });
    observer.observe(this.sentinel);

    this.eventsEl.addEventListener("scroll", () => {
      this.updateBottomBtn();
      // Pagination avant (vue ancrée) : le fond de la fenêtre est atteint
      // et du contenu plus récent existe → charger la page suivante.
      if (this.hasMoreNewer && this.nearBottom()) void this.loadNewerPage();
    });
    $("btn-bottom").onclick = () => {
      if (this.hasMoreNewer) void this.loadLatest();
      else this.eventsEl.scrollTo({ top: this.eventsEl.scrollHeight, behavior: "smooth" });
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
    // Boot : écran d'accueil avant la première réponse de list_sessions.
    this.updateEmptyState();
  }
}
