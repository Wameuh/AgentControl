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
import { call } from "../core/rpc";
import { b64decode } from "../core/encoding";
import { icon } from "../ui/icons";
import { nav } from "../ui/nav";
import { t, type Key } from "../i18n";
import type { CompItem } from "../core/types";

const COMP = "agentbacklog";
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
  /** Signature du dernier rendu — le poll ne rebuild le DOM que si la
   *  file ou la résolution d'agents a réellement changé. */
  private lastSig = "";

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
    if (!this.deps.componentsCapable(conn)) {
      this.state = { kind: "cap" };
      this.render();
      return;
    }
    const comps = await call<CompItem[]>("list_components", { conn }).catch(
      () => null,
    );
    if (!comps) {
      this.state = { kind: "err", msg: t("bl_fetch_err") };
      this.render();
      return;
    }
    const comp = comps.find((c) => c.name === COMP);
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
    const r = await call<QueryResp>("component_query", {
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
      JSON.stringify(app.lastSessions.map((s) => [s.id, s.title, s.conn]));
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

    if (st.kind !== "ok") {
      this.renderState(el, st);
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
    let html = "";
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
  }

  /** États sans items : chargement, démon trop vieux (capability
   *  `components` absente), composant non déclaré / sans query « list »
   *  (CTA Intégrer), erreur de fetch, file vide. */
  private renderState(el: HTMLElement, st: Exclude<State, { kind: "ok" }>) {
    if (st.kind === "absent" || st.kind === "noquery") {
      el.innerHTML =
        `<div class="sess-none">${icon("warn")} ${esc(t(st.kind === "absent" ? "bl_absent" : "bl_noquery"))}</div>` +
        `<button class="bl-cta" id="bl-install"${this.installing ? " disabled" : ""}>` +
        `${icon(this.installing ? "loader" : "plus")} ${esc(this.installing ? t("bl_installing") : t("bl_integrate"))}</button>`;
      $("bl-install").onclick = () => void this.install();
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
      `<div class="sess-none${warn ? " warn" : ""}">${icon(ic)} ${esc(txt)}</div>`;
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
