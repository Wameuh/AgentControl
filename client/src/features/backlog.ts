// ---------------------------------------------------------------------------
// Vue « Backlog » de la sidebar (onglet sv-backlog → #backlog-list) : file
// AgentBacklog exposée par le composant démon `agentbacklog`. Le client
// n'appelle JAMAIS le CLI directement — il demande `component_query
// {name:"agentbacklog", query:"list"}` : l'argv audité vit dans la spec
// déclarée (components.toml → spec.queries.list), exécuté borné côté démon.
//
// Refresh (spec #35) : fetch à l'ouverture de l'onglet + intervalle court
// (~4 s) tant que l'onglet est actif + invalidation debouncée sur
// les events session pertinents (status_changed — un worker qui passe
// idle/waiting a souvent soldé sa tâche ; session_created/deleted — un
// spawn qui pop ou une session libérée).
// Onglet inactif : cycle lent (~60 s) qui ne sert qu'au compteur de
// l'onglet — vraie couverture des mutations externes (autre worker,
// CLI) : les pushes session-event ne suivent que la session ouverte —
// quand c'est elle qui travaille la file, le même debounce rafraîchit
// le badge sans attendre le tick. Le détail du panneau profite du
// même fetch — aucune requête « count » plus légère n'existe dans la
// spec du composant.
// ---------------------------------------------------------------------------

import { $, esc, q } from "../core/dom";
import { app } from "../core/state";
import { call, callBg, isConnLost } from "../core/rpc";
import { b64decode } from "../core/encoding";
import { themeRgb } from "../core/chrome";
import { icon } from "../ui/icons";
import { nav } from "../ui/nav";
import { t, type Key } from "../i18n";
import type { CompItem } from "../core/types";

const COMP = "agentbacklog";
const ORCH_COMP = "backlog-orchestrator";
/** `origin` posé au spawn d'un Manager — constante partagée avec le
 *  démon (orch.rs `MANAGER_ORIGIN`), comme `COMP` pour le nom du
 *  composant. */
const MGR_ORIGIN = "feature:backlog-manager";
const QUERY = "list";
const POLL_MS = 4000;
/** Cadence du badge onglet inactif — assez rare pour rester du bruit de
 *  fond (2 RPC locales), assez dense pour qu'un item ajouté ailleurs
 *  apparaisse au compteur sans action. */
const IDLE_POLL_MS = 60_000;
const DONE_KEEP_MS = 24 * 3600 * 1000; // « récentes » : done_at < 24 h
const DONE_MAX = 8;
/** Sentinelle du filtre déroulant « items sans repo » — une valeur
 *  qui ne peut pas être un nom de repo taggé. */
const REPO_UNTAGGED = "__untagged__";
/** Fenêtres proposées par la RPC `backlog_metrics` (parse démon :
 *  `<n>h` / `<n>d` / `all`). Libellé affiché neutre fr/en. */
const M_WINDOWS: [string, string][] = [
  ["24h", "24 h"],
  ["7d", "7 j"],
  ["30d", "30 j"],
];

/** Sample du journal orch (1 point par tick) — séries du graphe. */
interface MetricsSample {
  ts?: number;
  pending?: number;
  wip?: number;
  workers_alive?: number;
}

/** Réponse `backlog_metrics` — agrégats calculés côté démon ; les
 *  champs durée sont null quand agentbacklog est indisponible. */
interface MetricsResp {
  window?: string;
  sample_count?: number;
  samples?: MetricsSample[];
  aggregates?: {
    items_available?: boolean;
    done_per_day?: number | null;
    lead_time_avg_s?: number | null;
    queue_wait_avg_s?: number | null;
    workers_peak?: number | null;
  };
}

export interface BacklogDeps {
  /** Clé connKey de la connexion liée — "" = démon par défaut. */
  conn: () => string;
  /** Capability `components` du démon de cette conn (daemon_status). */
  componentsCapable: (conn: string) => boolean;
  /** Clic sur la pastille agent → ouvrir son chat. */
  openSession: (id: string, conn: string) => void;
  /** Surligneur du filtre sidebar partagé. */
  hl: (txt: string) => string;
}

interface BacklogItem {
  id?: number | string;
  title?: string;
  status?: string;
  agent_id?: string;
  created_at?: string;
  started_at?: string | null;
  done_at?: string | null;
  dep_status?: string | null;
  depends_on?: number | null;
  /** Tag workspace/repo de l'item (multi-workspaces) — null = non
   *  taggé (legacy) : pas de badge affiché, un libellé inventé genre
   *  « défaut » induirait en erreur (aucun défaut n'est connu). */
  repo?: string | null;
}

interface QueryResp {
  code: number | null;
  /** stdout borné, base64. */
  data: string;
  size?: number;
  truncated?: boolean;
  /** stderr bornée, base64 — présente seulement si non vide. */
  stderr?: string;
}

type State =
  | { kind: "loading" | "cap" | "absent" | "noquery" | "empty" }
  | { kind: "err"; msg: string }
  | { kind: "ok"; items: BacklogItem[] };

export class BacklogView {
  private deps: BacklogDeps = {
    conn: () => "",
    componentsCapable: () => false,
    openSession: () => {},
    hl: (s) => esc(s),
  };
  private state: State = { kind: "loading" };
  private active = false;
  private timer: number | undefined;
  private evTimer: number | undefined;
  private installing = false;
  private filter = "";
  /** Repo sélectionné dans le filtre déroulant — "" = tous,
   *  REPO_UNTAGGED = seulement les non taggés. */
  private repoFilter = "";
  /** Workspaces routés exposés par l'orchestrateur actif (surface
   *  « Manager » — un proposable par repo sans session vivante). */
  private mgrWs: { repo: string; cwd: string }[] = [];
  private mgrBusy = new Set<string>();
  private mgrErr: string | null = null;
  /** Signature du dernier rendu — le poll ne rebuild le DOM que si la
   *  file ou la résolution d'agents a réellement changé. */
  private lastSig = "";
  /** Section « Activité » (RPC backlog_metrics). `avail` retombe à
   *  false au premier refus applicatif (vieux démon / RPC inconnue) →
   *  section masquée ; un changement de conn la réarme. Fenêtre et
   *  ouverture persistées en localStorage (spec #101). */
  private metrics: MetricsResp | null = null;
  private metricsAvail = true;
  private metricsConn: string | null = null;
  private metricsWin = localStorage.getItem("ac-bl-metrics-win") || "7d";
  private metricsOpen = localStorage.getItem("ac-bl-metrics-open") !== "0";
  private metricsRo: ResizeObserver | null = null;

  bind(d: Partial<BacklogDeps>) {
    Object.assign(this.deps, d);
    // Cycle badge lent dès le boot : le compteur doit refléter la file
    // même sans jamais ouvrir l'onglet.
    this.armPoll(IDLE_POLL_MS);
  }

  /** Intervalle unique du fetch — la cadence dépend de l'onglet
   *  (rapide actif, lente pour le badge inactif). */
  private armPoll(ms: number) {
    clearInterval(this.timer);
    this.timer = window.setInterval(() => void this.refresh(), ms);
  }

  /** Onglet activé : fetch immédiat + poll rapide. Désactivé : retombe
   *  sur la cadence badge (60 s) — l'evTimer survit : ses events
   *  continuent d'invalider le compteur. */
  setActive(on: boolean) {
    if (this.active === on) return;
    this.active = on;
    if (on) {
      void this.refresh();
      this.armPoll(POLL_MS);
    } else {
      this.armPoll(IDLE_POLL_MS);
    }
  }

  /** Invalidation par event session — debounce : une rafale de
   *  status_changed = un seul fetch. Onglet actif ou non — les pushes
   *  ne couvrant que la session ouverte, le cas utile est « la session
   *  affichée travaille la file » ; le reste relève du poll lent. */
  noteSessionEvent(type: string) {
    if (
      type !== "status_changed" &&
      type !== "session_created" &&
      type !== "session_deleted"
    ) {
      return;
    }
    clearTimeout(this.evTimer);
    this.evTimer = window.setTimeout(() => {
      this.evTimer = undefined;
      void this.refresh();
    }, 800);
  }

  /** Capability `components` (re)découverte — premier paint du badge
   *  sans attendre le prochain tick (le poll lent part du boot, avant
   *  la connexion). */
  noteCaps() {
    void this.refresh();
  }

  setFilter(f: string) {
    this.filter = f;
    this.render();
  }

  /** app.lastSessions vient d'être rafraîchi : la résolution
   *  agent_id → titre peut avoir changé (signature-gardé). */
  noteSessions() {
    if (this.active) this.render();
  }

  private async refresh() {
    const conn = this.deps.conn();
    // Métriques : même cadence que le panneau mais en parallèle — une
    // RPC lente ne retarde jamais le rendu de la file. Onglet inactif
    // ou section repliée → pas de fetch (la série 30 j peut peser
    // ~15 Mo JSON, inutile quand elle n'est pas affichée).
    if (this.active && this.metricsOpen) void this.refreshMetrics(conn);
    if (!this.deps.componentsCapable(conn)) {
      this.state = { kind: "cap" };
      this.mgrWs = []; // vieux démon : pas de surface Manager
      this.render();
      return;
    }
    const comps = await callBg<CompItem[]>("list_components", { conn }).catch(
      () => null,
    );
    if (!comps) {
      this.mgrWs = []; // démon injoignable : boutons « Créer » voués à échouer
      this.state = { kind: "err", msg: t("bl_fetch_err") };
      this.render();
      return;
    }
    const comp = comps.find((c) => c.name === COMP);
    // Surface « Manager » : workspaces routés exposés par
    // l'orchestrateur actif (status.workspaces) — détection voulue
    // = composant agentbacklog déclaré + orchestrateur actif.
    const orch = comps.find((c) => c.name === ORCH_COMP)?.orchestrator;
    this.mgrWs =
      comp && orch?.enabled
        ? (orch.workspaces ?? [])
            .filter((w): w is { repo: string; cwd?: string } => !!w.repo)
            .map((w) => ({ repo: w.repo, cwd: w.cwd ?? "" }))
        : [];
    if (!comp) {
      this.state = { kind: "absent" };
      this.render();
      return;
    }
    const qs = (comp.spec?.queries ?? {}) as Record<string, string[]>;
    if (!qs[QUERY]?.length) {
      this.state = { kind: "noquery" };
      this.render();
      return;
    }
    const r = await callBg<QueryResp>("component_query", {
      conn,
      name: COMP,
      query: QUERY,
    }).catch((e: unknown) => ({ err: String(e) }));
    if ("err" in r) {
      this.state = { kind: "err", msg: r.err };
      this.render();
      return;
    }
    if (r.code !== 0) {
      const tail = r.stderr
        ? new TextDecoder().decode(b64decode(r.stderr)).trim()
        : "";
      this.state = {
        kind: "err",
        msg: `${t("bl_q_err")} (code ${r.code ?? "?"})${tail ? ` — ${tail.slice(0, 140)}` : ""}`,
      };
      this.render();
      return;
    }
    let items: BacklogItem[] = [];
    try {
      const raw = new TextDecoder().decode(b64decode(r.data ?? ""));
      const parsed: unknown = JSON.parse(raw);
      if (Array.isArray(parsed)) items = parsed as BacklogItem[];
    } catch {
      // Sortie tronquée côté démon (>512 Kio) → JSON cassé : message dédié.
      this.state = {
        kind: "err",
        msg: t(r.truncated ? "bl_truncated" : "bl_bad_data"),
      };
      this.render();
      return;
    }
    this.state = items.length ? { kind: "ok", items } : { kind: "empty" };
    this.render();
  }

  /** Série + agrégats `backlog_metrics` — RPC additive : un démon
   *  antérieur répond « req inconnue » → rejet → section masquée à
   *  vie (réarmée au changement de connexion), jamais d'erreur brute
   *  (spec #101, cohérent avec #96). Les transitoires de coupure sont
   *  absorbés/rejoués par callBg ; un isConnLost résiduel conserve
   *  l'état courant plutôt que masquer sur du bruit transport. */
  private async refreshMetrics(conn: string) {
    if (conn !== this.metricsConn) {
      this.metricsConn = conn;
      this.metrics = null;
      this.metricsAvail = true;
    }
    if (!this.metricsAvail) return;
    const win = this.metricsWin;
    const r = await callBg<MetricsResp>("backlog_metrics", {
      conn,
      window: win,
    }).then(
      (v) => ({ v, err: null as unknown }),
      (e: unknown) => ({ v: null, err: e }),
    );
    // Réponse périmée : la conn a basculé ou la fenêtre a changé
    // pendant le vol (callBg peut retenir ~120 s en reconnexion) —
    // un refus applicatif de l'ancien contexte (vieux démon A)
    // masquerait à tort la section sur le nouveau (conn B).
    if (conn !== this.metricsConn || win !== this.metricsWin) return;
    if (r.err) {
      if (isConnLost(r.err, conn)) return;
      this.metricsAvail = false;
      this.render();
      return;
    }
    this.metrics = r.v;
    this.render();
  }

  /** CTA « Intégrer » : recette catalogue (install + upsert de la spec —
   *  la query `list` vient avec). Utilisé pour les états absent/noquery.
   *  Un échec du démon remonte en état `err` plutôt que de retomber
   *  silencieusement sur l'état précédent. */
  private async install() {
    if (this.installing) return;
    this.installing = true;
    this.render();
    const conn = this.deps.conn();
    const err = await call("component_install", { conn, name: COMP }).then(
      () => null,
      (e: unknown) => String(e),
    );
    this.installing = false;
    if (!this.active) return;
    if (err) {
      this.state = { kind: "err", msg: `${t("bl_install_err")} — ${err}` };
      this.render();
      return;
    }
    void this.refresh();
  }

  // --- Rendu ---------------------------------------------------------------

  private render() {
    const el = $("backlog-list");
    const st = this.state;
    // Le choix du filtre repo peut devenir invalide quand la file
    // change (repo disparu, plus aucun non taggé) → normalisé avant la
    // signature pour ne pas figer un filtrage fantôme dans lastSig.
    if (st.kind === "ok") {
      const rs = new Set(
        st.items.map((i) => i.repo).filter((r): r is string => !!r),
      );
      if (
        this.repoFilter === REPO_UNTAGGED
          ? !st.items.some((i) => !i.repo)
          : this.repoFilter !== "" && !rs.has(this.repoFilter)
      ) {
        this.repoFilter = "";
      }
    }
    // La résolution agent → titre dépend d'app.lastSessions : la signature
    // l'inclut (re-rendu si une session connue apparaît/change de titre).
    const sig =
      JSON.stringify(this.state) +
      this.filter +
      this.repoFilter +
      this.installing +
      JSON.stringify(this.mgrWs) +
      JSON.stringify([...this.mgrBusy]) +
      this.mgrErr +
      JSON.stringify(
        app.lastSessions.map((s) => [s.id, s.title, s.conn, s.origin, s.state]),
      ) +
      // Métriques : la série entière serait coûteuse à sérialiser à
      // chaque poll — count + dernier ts + agrégats suffisent à
      // détecter un nouveau point ou une nouvelle fenêtre.
      `${this.metricsAvail}|${this.metricsOpen}|${this.metricsWin}|` +
      (this.metrics
        ? `${this.metrics.sample_count}|${this.metrics.samples?.at(-1)?.ts}|${JSON.stringify(this.metrics.aggregates)}`
        : "null");
    if (sig === this.lastSig) return;
    this.lastSig = sig;

    const badge = $("sv-bcount");
    if (st.kind === "ok") {
      const n = st.items.filter((i) => i.status !== "done").length;
      badge.hidden = n === 0;
      badge.textContent = String(n);
    } else {
      badge.hidden = true;
    }

    const mgr = this.mgrHtml() + this.metricsHtml();
    if (st.kind !== "ok") {
      this.renderState(el, st, mgr);
      return;
    }
    const items = st.items;

    // ok : 3 sections — wip (agent résolu + depuis), pending (bloquées
    // marquées), done récentes (24 h, 8 max).
    const now = Date.now();
    const wip = items.filter((i) => i.status === "wip");
    const done = items
      .filter(
        (i) =>
          i.status === "done" &&
          i.done_at &&
          now - Date.parse(i.done_at) < DONE_KEEP_MS,
      )
      .sort((a, b) => (b.done_at ?? "").localeCompare(a.done_at ?? ""))
      .slice(0, DONE_MAX);
    const pend = items.filter(
      (i) => i.status !== "wip" && i.status !== "done",
    );

    const f = this.filter;
    const rf = this.repoFilter;
    const match = (i: BacklogItem) => {
      if (rf === REPO_UNTAGGED) {
        if (i.repo) return false;
      } else if (rf !== "" && i.repo !== rf) {
        return false;
      }
      if (!f) return true;
      const agent = this.agentTitle(i.agent_id) ?? i.agent_id ?? "";
      const hay =
        `${i.id ?? ""} ${i.title ?? ""} ${agent} ${i.repo ?? ""}`.toLowerCase();
      return hay.includes(f);
    };
    const wv = wip.filter(match);
    const pv = pend.filter(match);
    const dv = done.filter(match);

    // Filtre par repo — rendu seulement quand ≥2 « files » coexistent
    // (repos distincts + les non taggés comptés comme un groupe) :
    // à un seul repo possible il n'apporte rien et encombre l'onglet.
    // Posé AVANT les sections : un filtre qui masque tout doit rester
    // visible au-dessus du « aucun item » pour pouvoir être défait.
    const repos = [
      ...new Set(items.map((i) => i.repo).filter((r): r is string => !!r)),
    ].sort();
    const untagged = items.some((i) => !i.repo);
    let html = mgr; // section « Managers » en tête, avant filtre et file
    if (repos.length + (untagged ? 1 : 0) >= 2) {
      html +=
        `<div class="bl-repo-filter"><select id="bl-repo-sel" title="${q(esc(t("bl_repo_filter")))}">` +
        `<option value="">${esc(t("bl_all_repos"))}</option>` +
        repos
          .map(
            (r) =>
              `<option value="${q(esc(r))}"${r === rf ? " selected" : ""}>${esc(r)}</option>`,
          )
          .join("") +
        (untagged
          ? `<option value="${REPO_UNTAGGED}"${rf === REPO_UNTAGGED ? " selected" : ""}>${esc(t("bl_repo_none"))}</option>`
          : "") +
        `</select></div>`;
    }
    if (!wv.length && !pv.length && !dv.length) {
      el.innerHTML =
        html + `<div class="sess-none">${esc(t("bl_none_filt"))}</div>`;
    } else {
      if (wv.length) html += this.section("bl_wip", wv, "wip");
      if (pv.length) html += this.section("bl_pending", pv, "pend");
      if (dv.length) html += this.section("bl_done", dv, "done");
      el.innerHTML = html;
    }
    el.querySelector<HTMLSelectElement>("#bl-repo-sel")?.addEventListener(
      "change",
      (e) => {
        this.repoFilter = (e.target as HTMLSelectElement).value;
        this.render();
      },
    );
    el.querySelectorAll<HTMLElement>(".bl-agent[data-sid]").forEach((b) => {
      b.onclick = () => {
        nav.closeDrawer(); // mobile : la drawer laisserait le chat masqué
        this.deps.openSession(b.dataset.sid!, b.dataset.conn ?? "");
      };
    });
    this.wireMgr(el);
    this.wireMetrics(el);
    this.drawMetrics();
    this.watchMetrics();
  }

  /** États sans items : chargement, démon trop vieux (capability
   *  `components` absente), composant non déclaré / sans query « list »
   *  (CTA Intégrer), erreur de fetch, file vide. */
  private renderState(
    el: HTMLElement,
    st: Exclude<State, { kind: "ok" }>,
    pre: string,
  ) {
    if (st.kind === "absent" || st.kind === "noquery") {
      el.innerHTML =
        pre +
        `<div class="sess-none">${icon("warn")} ${esc(t(st.kind === "absent" ? "bl_absent" : "bl_noquery"))}</div>` +
        `<button class="bl-cta" id="bl-install"${this.installing ? " disabled" : ""}>` +
        `${icon(this.installing ? "loader" : "plus")} ${esc(this.installing ? t("bl_installing") : t("bl_integrate"))}</button>`;
      $("bl-install").onclick = () => void this.install();
      this.wireMgr(el);
      this.wireMetrics(el);
      this.drawMetrics();
      this.watchMetrics();
      return;
    }
    const [ic, txt, warn] =
      st.kind === "loading"
        ? (["loader", t("bl_loading"), false] as const)
        : st.kind === "cap"
          ? (["warn", t("bl_cap"), false] as const)
          : st.kind === "err"
            ? (["warn", st.msg, true] as const)
            : (["check", t("bl_empty"), false] as const);
    el.innerHTML =
      pre +
      `<div class="sess-none${warn ? " warn" : ""}">${icon(ic)} ${esc(txt)}</div>`;
    this.wireMgr(el);
    this.wireMetrics(el);
    this.drawMetrics();
    this.watchMetrics();
  }

  // --- Surface « Manager » ---------------------------------------------------

  /** Session Manager vivante du workspace `repo` — provenance
   *  `feature:backlog-manager` + titre « Manager · <repo> » ; dead /
   *  interrupted = respawn proposable. */
  private mgrSession(repo: string) {
    const title = `Manager · ${repo}`;
    const conn = this.deps.conn();
    return app.lastSessions.find(
      (s) =>
        (s.conn ?? "") === conn && // lastSessions est fusionné multi-démons
        s.origin === MGR_ORIGIN &&
        s.title === title &&
        s.state !== "dead" &&
        s.state !== "interrupted",
    );
  }

  /** Section « Managers » : une ligne par workspace routé — bouton
   *  « Ouvrir » vers la session vivante, « Créer » sinon. Vide quand
   *  l'orchestrateur n'est pas actif (ou backlog non déclaré). */
  private mgrHtml(): string {
    if (!this.mgrWs.length) return "";
    let rows = "";
    for (const w of this.mgrWs) {
      const s = this.mgrSession(w.repo);
      const busy = this.mgrBusy.has(w.repo);
      rows +=
        `<div class="bl-item" title="${q(esc(w.cwd))}">` +
        `<span class="bl-title">${icon("bot")} Manager · ${esc(w.repo)}</span>` +
        (s
          ? `<button class="bl-cta" data-mgr-open="${q(esc(s.id))}" ` +
            `data-conn="${q(esc(s.conn ?? ""))}">${esc(t("bl_mgr_open"))}</button>`
          : `<button class="bl-cta" data-mgr-spawn="${q(esc(w.repo))}"${busy ? " disabled" : ""}>` +
            `${icon(busy ? "loader" : "plus")} ${esc(busy ? t("bl_mgr_busy") : t("bl_mgr_create"))}</button>`) +
        `</div>`;
    }
    if (this.mgrErr) {
      rows += `<div class="sess-none warn">${icon("warn")} ${esc(this.mgrErr)}</div>`;
    }
    return (
      `<div class="bl-sec"><div class="grp-head">${esc(t("bl_mgr_head"))}</div>` +
      rows +
      `</div>`
    );
  }

  /** Boutons de la section Manager — rebind après chaque innerHTML. */
  private wireMgr(el: HTMLElement) {
    el.querySelectorAll<HTMLElement>("[data-mgr-open]").forEach((b) => {
      b.onclick = () => {
        nav.closeDrawer();
        this.deps.openSession(b.dataset.mgrOpen!, b.dataset.conn ?? "");
      };
    });
    el.querySelectorAll<HTMLElement>("[data-mgr-spawn]").forEach((b) => {
      b.onclick = () => void this.spawnManager(b.dataset.mgrSpawn!);
    });
  }

  // --- Section « Activité » (métriques) --------------------------------------

  /** Section dépliable : sélecteur de fenêtre + courbes + cartes
   *  stats. Entièrement absente du DOM si le démon ne connaît pas la
   *  RPC — un bandeau « indisponible » serait du bruit permanent. */
  private metricsHtml(): string {
    if (!this.metricsAvail) return "";
    const open = this.metricsOpen;
    let body = "";
    if (open) {
      const sel =
        `<select id="bl-mw" title="${q(esc(t("bl_m_window")))}">` +
        M_WINDOWS.map(
          ([v, l]) =>
            `<option value="${v}"${v === this.metricsWin ? " selected" : ""}>${l}</option>`,
        ).join("") +
        `</select>`;
      const m = this.metrics;
      if (!m) {
        body = `<div class="sess-none">${icon("loader")} ${esc(t("bl_m_loading"))}</div>`;
      } else if (m.samples?.length) {
        body =
          `<canvas id="bl-mcanvas"></canvas>` +
          `<div class="bl-m-legend">` +
          `<span><i class="bl-m-dot" style="background:var(--warn)"></i>${esc(t("bl_pending"))}</span>` +
          `<span><i class="bl-m-dot" style="background:var(--accent)"></i>${esc(t("bl_wip"))}</span>` +
          `<span><i class="bl-m-dot" style="background:var(--ok)"></i>${esc(t("bl_m_workers"))}</span>` +
          `</div>` +
          this.cardsHtml(m);
      } else {
        // Journal vide (orch off / jamais échantillonné) : les
        // agrégats items restent utiles — cartes + placeholder discret.
        body =
          `<div class="sess-none">${esc(t("bl_m_none"))}</div>` +
          this.cardsHtml(m);
      }
      body = `<div class="bl-m-body">${sel}${body}</div>`;
    }
    return (
      `<div class="bl-sec bl-metrics"><div class="grp-head bl-m-head" id="bl-m-tgl">` +
      `<span class="grp-arrow">${icon(open ? "chev-down" : "chev-right")}</span> ${esc(t("bl_metrics"))}</div>` +
      body +
      `</div>`
    );
  }

  /** Cartes stats — valeurs toutes issues des agrégats de la RPC,
   *  jamais recalculées côté client (spec) ; null → « — ». */
  private cardsHtml(m: MetricsResp): string {
    const a = m.aggregates;
    const card = (k: Key, v: string) =>
      `<div class="bl-m-card"><div class="bl-m-v">${esc(v)}</div>` +
      `<div class="bl-m-k">${esc(t(k))}</div></div>`;
    const dur = (s: number | null | undefined) =>
      s == null ? "—" : fmtDur(s);
    const num = (x: number | null | undefined) =>
      x == null ? "—" : String(Math.round(x * 10) / 10);
    return (
      `<div class="bl-m-cards">` +
      card("bl_m_lead", dur(a?.lead_time_avg_s)) +
      card("bl_m_wait", dur(a?.queue_wait_avg_s)) +
      card("bl_m_day", num(a?.done_per_day)) +
      card("bl_m_peak", a?.workers_peak == null ? "—" : String(a.workers_peak)) +
      `</div>`
    );
  }

  /** Toggle dépliable + sélecteur de fenêtre — rebind après chaque
   *  innerHTML (comme wireMgr). */
  private wireMetrics(el: HTMLElement) {
    el.querySelector<HTMLElement>("#bl-m-tgl")?.addEventListener(
      "click",
      () => {
        this.metricsOpen = !this.metricsOpen;
        localStorage.setItem(
          "ac-bl-metrics-open",
          this.metricsOpen ? "1" : "0",
        );
        // Dépliée → fetch immédiat (le poll ne fetch que si ouverte).
        if (this.metricsOpen) void this.refreshMetrics(this.deps.conn());
        this.render();
      },
    );
    el.querySelector<HTMLSelectElement>("#bl-mw")?.addEventListener(
      "change",
      (e) => {
        this.metricsWin = (e.target as HTMLSelectElement).value;
        localStorage.setItem("ac-bl-metrics-win", this.metricsWin);
        this.metrics = null; // nouvelle fenêtre → refetch immédiat
        this.render();
        void this.refreshMetrics(this.deps.conn());
      },
    );
  }

  /** Redessine quand la sidebar change de largeur — le canvas suit
   *  `width:100%` en CSS, seul le backing store DPR est à recaler. */
  private watchMetrics() {
    this.metricsRo?.disconnect();
    this.metricsRo = null;
    const c = $("bl-mcanvas");
    if (!c) return;
    this.metricsRo = new ResizeObserver(() => this.drawMetrics());
    this.metricsRo.observe(c);
  }

  /** Courbes pending / wip / workers_alive — canvas maison (le client
   *  n'a pas de lib de chart, style mesh-bg : backing store ×DPR,
   *  couleurs = vars thème résolues → lisible clair et sombre). Les
   *  samples sont triés par ts et décimés au-delà de ~800 points. */
  private drawMetrics() {
    const c = $("bl-mcanvas") as HTMLCanvasElement | null;
    const samples = this.metrics?.samples;
    if (!c || !samples?.length) return;
    // Taille affichée = CSS (width:100% du content-box parent — le
    // padding de .bl-m-body ne déborde pas), backing store ×DPR.
    const w = c.clientWidth || 240;
    const h = c.clientHeight || 120;
    const dpr = window.devicePixelRatio || 1;
    c.width = Math.round(w * dpr);
    c.height = Math.round(h * dpr);
    const ctx = c.getContext("2d");
    if (!ctx) return;
    ctx.scale(dpr, dpr);

    const pts = [...samples].sort((a, b) => (a.ts ?? 0) - (b.ts ?? 0));
    const stride = Math.max(1, Math.ceil(pts.length / 800));
    const t0 = pts[0].ts ?? 0;
    const span = Math.max(1e-9, (pts[pts.length - 1].ts ?? t0) - t0);
    const padL = 24, padR = 8, padT = 8, padB = 15;
    const iw = w - padL - padR;
    const ih = h - padT - padB;
    // Pas de spread : ~100 k samples possibles (30 j à tick 10 s) —
    // Math.max(...pts) dépasserait la limite d'arguments de JSC.
    let maxV = 1;
    for (const s of pts) {
      maxV = Math.max(
        maxV,
        s.pending ?? 0,
        s.wip ?? 0,
        s.workers_alive ?? 0,
      );
    }

    const dim = themeRgb("--dim") ?? "#888";
    ctx.font = "9px ui-monospace, SFMono-Regular, monospace";
    ctx.strokeStyle = dim;
    ctx.globalAlpha = 0.3;
    for (const g of [0.5, 1]) {
      const y = padT + ih * g;
      ctx.beginPath();
      ctx.moveTo(padL, y);
      ctx.lineTo(w - padR, y);
      ctx.stroke();
    }
    ctx.globalAlpha = 1;
    ctx.fillStyle = dim;
    ctx.textAlign = "left";
    ctx.fillText(String(maxV), 2, padT + 8);
    const wl =
      M_WINDOWS.find(([v]) => v === this.metricsWin)?.[1] ?? this.metricsWin;
    ctx.fillText(`−${wl}`, padL, h - 4);
    ctx.textAlign = "right";
    ctx.fillText(t("bl_now"), w - padR, h - 4);

    const series: [keyof MetricsSample, string][] = [
      ["pending", "--warn"],
      ["wip", "--accent"],
      ["workers_alive", "--ok"],
    ];
    for (const [k, varName] of series) {
      const col = themeRgb(varName) ?? dim;
      ctx.strokeStyle = col;
      ctx.fillStyle = col;
      ctx.lineWidth = 1.6;
      ctx.beginPath();
      let first = true;
      pts.forEach((s, i) => {
        // Décimation régulière + dernier point toujours tracé.
        if (i % stride && i !== pts.length - 1) return;
        const x = padL + (((s.ts ?? t0) - t0) / span) * iw;
        const y = padT + ih - ((s[k] ?? 0) / maxV) * ih;
        if (first) {
          ctx.moveTo(x, y);
          first = false;
        } else {
          ctx.lineTo(x, y);
        }
      });
      ctx.stroke();
      if (pts.length === 1) {
        // Une seule mesure : pas de ligne — marqueur visible.
        const y = padT + ih - ((pts[0][k] ?? 0) / maxV) * ih;
        ctx.beginPath();
        ctx.arc(padL, y, 2.5, 0, 7);
        ctx.fill();
      }
    }
  }

  /** CTA « Créer » : `manager_spawn` démon (idempotent côté démon —
   *  un Manager vivant est renvoyé plutôt que doublonné), puis focus
   *  sur la session. Erreur affichée dans la section, la file reste. */
  private async spawnManager(repo: string) {
    if (this.mgrBusy.has(repo)) return;
    this.mgrBusy.add(repo);
    this.mgrErr = null;
    this.render();
    const conn = this.deps.conn();
    const r = await call<{ session?: string }>("manager_spawn", {
      conn,
      repo,
    }).then(
      (v) => ({ v, err: null as string | null }),
      (e: unknown) => ({ v: null, err: String(e) }),
    );
    this.mgrBusy.delete(repo);
    if (r.err) {
      this.mgrErr = `${t("bl_mgr_err")} — ${r.err}`;
      this.render();
      return;
    }
    this.render();
    if (r.v?.session) {
      nav.closeDrawer(); // mobile : la drawer laisserait le chat masqué
      this.deps.openSession(r.v.session, conn);
    }
  }

  /** Titre de la session portant cet agent_id — null si inconnue/absente. */
  private agentTitle(agentId: string | undefined): string | null {
    if (!agentId) return null;
    const s = app.lastSessions.find((x) => x.id === agentId);
    return s?.title ?? null;
  }

  private agentHtml(i: BacklogItem): string {
    if (!i.agent_id) return "";
    const s = app.lastSessions.find((x) => x.id === i.agent_id);
    if (s) {
      // title attr = meta complète ; pastille cliquable → ouvre le chat.
      return (
        `<button class="bl-agent" data-sid="${q(esc(s.id))}" ` +
        `data-conn="${q(esc(s.conn ?? ""))}" title="${q(esc(s.id))} — ${q(esc(s.title ?? ""))}">` +
        `${icon("bot")}<span>${esc(s.title ?? s.id)}</span></button>`
      );
    }
    return `<span class="bl-agent off" title="${q(esc(i.agent_id))}">${esc(shortId(i.agent_id))}</span>`;
  }

  private section(headKey: Key, items: BacklogItem[], cls: string) {
    let html = `<div class="bl-sec ${cls}"><div class="grp-head">${esc(t(headKey))} <span class="bl-n">${items.length}</span></div>`;
    for (const i of items) {
      const id = i.id ?? "?";
      const when =
        cls === "wip"
          ? rel(i.started_at, "bl_since")
          : cls === "done"
            ? rel(i.done_at, "bl_ago")
            : rel(i.created_at, "bl_ago");
      const blocked =
        cls === "pend" && i.dep_status && i.dep_status !== "done" && i.depends_on != null;
      html +=
        `<div class="bl-item" title="${q(esc(i.title ?? ""))}">` +
        `<span class="bl-id">#${esc(String(id))}</span>` +
        (i.repo
          ? `<span class="badge bl-repo" title="${q(esc(i.repo))}">${esc(i.repo)}</span>`
          : "") +
        `<span class="bl-title">${this.deps.hl(i.title ?? "")}</span>` +
        (blocked
          ? `<span class="bl-blocked" title="${q(esc(t("bl_blocked", { id: String(i.depends_on) })))}">${icon("lock")}</span>`
          : "") +
        this.agentHtml(i) +
        (when
          ? `<span class="bl-when" title="${q(esc(when.title))}">${esc(when.label)}</span>`
          : "") +
        `</div>`;
    }
    return html + "</div>";
  }
}

/** `s-84f27d4c` → `s-84f27d4…` — agent_id brut non résolu, lisible en 1 ligne. */
function shortId(id: string): string {
  return id.length > 9 ? `${id.slice(0, 8)}…` : id;
}

/** Durée en secondes → « 42 s », « 23 m », « 5,5 h », « 3,2 j » —
 *  compact façon `rel`, une décimale sous 10 unités, entier au-delà. */
function fmtDur(sec: number): string {
  const r = (v: number, u: string) =>
    `${v < 10 ? Math.round(v * 10) / 10 : Math.round(v)} ${u}`;
  if (sec < 90) return r(sec, "s");
  if (sec < 5400) return r(sec / 60, "m");
  if (sec < 129600) return r(sec / 3600, "h");
  return r(sec / 86400, "j");
}

/** Temps relatif compact (« 5 m », « 2 h », « 3 j » — neutre fr/en) + date
 *  locale complète en title. `prefKey` habille le préfixe (« depuis » /
 *  « il y a »). */
function rel(
  iso: string | null | undefined,
  prefKey: Key,
): { label: string; title: string } | null {
  if (!iso) return null;
  const ts = Date.parse(iso);
  if (Number.isNaN(ts)) return null;
  const d = Math.max(0, Date.now() - ts);
  const ago =
    d < 60_000
      ? t("bl_now")
      : d < 3600_000
        ? `${Math.floor(d / 60_000)} m`
        : d < 86400_000
          ? `${Math.floor(d / 3600_000)} h`
          : `${Math.floor(d / 86400_000)} j`;
  return {
    label: t(prefKey, { t: ago }),
    title: new Date(ts).toLocaleString(),
  };
}
