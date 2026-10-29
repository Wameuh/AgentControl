// ---------------------------------------------------------------------------
// Vue diagramme : graphe radial des composants et des sessions autour du
// démon (spec docs/components-integration.md §2.7). Overlay plein #chat
// (mécanique settings-view : inert + nav.push), SVG vanilla sans dep, poll
// ~3 s uniquement tant que la vue est ouverte — pas de canal d'events
// daemon-wide (le Subscribe proto est par-session).
//
// Layout déterministe (pas de force-directed) : démon au centre,
// composants sur un anneau, sessions groupées autour de leur parent —
// le composant `origin = component:<name>` (#24 / §2.3.1), la session
// `spawned_by` (spawn agent→agent), le démon sinon sur l'anneau externe.
// ---------------------------------------------------------------------------

import { isAndroid } from "../core/chrome";
import { $, esc, q } from "../core/dom";
import { call } from "../core/rpc";
import type { CompItem, CompStatus, PrEntry, SessionMeta } from "../core/types";
import { t } from "../i18n";
import { hydrateIcons, icon, type IconName } from "../ui/icons";
import { nav } from "../ui/nav";
import { dotCls, dotIcon } from "./status";
import { gemSVG } from "../crystal";

const R_DAEMON = 27;
const R_COMP = 21;
const R_SESS = 15;
/** Rayon du cercle des enfants autour de leur nœud parent. */
const RHO_KIDS = 58;

/** Icône par kind de composant (kind inconnu → tool, le plus passif). */
const KIND_ICON: Record<string, IconName> = {
  internal: "sys",
  process: "terminal",
  external: "external",
  tool: "tool",
};

/** États wire du proto (snake_case) — whitelist pour l'attribut class.
 *  `installed`/`absent` sont les états d'observation des `tool` (pas de
 *  cycle de vie). */
const COMP_STATES = new Set([
  "starting", "running", "degraded", "stopped", "failed",
  "installed", "absent",
]);
const compCls = (s?: string) => (s && COMP_STATES.has(s) ? s : "stopped");

/** États de session « en vol » → arête parente animée (flux actif). */
const ACTIVE = new Set(["working", "running", "starting"]);

/** Niche un <svg> complet (icon()/gemSVG()) dans le SVG du diagramme. */
const nest = (svg: string, x: number, y: number, s: number) =>
  svg.replace("<svg ", `<svg x="${x}" y="${y}" width="${s}" height="${s}" `);

const trunc = (s: string, n: number) =>
  s.length > n ? `${s.slice(0, n - 1)}…` : s;

const fmtUp = (s: number) =>
  s >= 3600
    ? `${Math.floor(s / 3600)}h${String(Math.floor((s % 3600) / 60)).padStart(2, "0")}`
    : s >= 60
      ? `${Math.floor(s / 60)}m${String(Math.floor(s % 60)).padStart(2, "0")}`
      : `${Math.floor(s)}s`;

type Pt = [number, number];

export interface MapDeps {
  /** Clé connKey de la connexion liée — "" tant que rien n'est bindé. */
  conn: () => string;
  /** Capability `components` du démon de cette conn (daemon_status). */
  componentsCapable: (conn: string) => boolean;
  /** Clic sur un nœud session → ouvrir son chat. */
  openSession: (id: string, conn: string) => void;
  /** La vue s'apprête à s'ouvrir : les autres overlays de #chat
   *  (paramètres, vue connexion) doivent se fermer — même motif que
   *  settings-view/conn-view (injecté par le bootstrap). */
  onOpening: () => void;
}

export class MapView {
  private deps: MapDeps = {
    conn: () => "",
    componentsCapable: () => false,
    openSession: () => {},
    onOpening: () => {},
  };
  private timer: ReturnType<typeof setInterval> | null = null;
  /** Dernier jeu de données du poll — re-rendu de la carte info au tick. */
  private data = { sess: [] as SessionMeta[], prs: [] as PrEntry[], comps: [] as CompItem[] };
  /** Composant dont la carte info est ouverte ("" = refermée). */
  private infoComp = "";
  /** Signature JSON du dernier rendu — évite le rebuild si le poll
   *  n'a rien changé. */
  private lastSig = "";
  /** Idem pour la carte info — un tick qui ne change que les autres
   *  composants ne doit pas détruire le focus sur son bouton close. */
  private lastInfoSig = "";

  bind(d: MapDeps) {
    this.deps = d;
  }

  private isOpen() {
    return !$("map-view").hidden;
  }

  private setChatInert(v: boolean) {
    for (const el of [...$("chat").children]) {
      if (el.id !== "map-view") (el as HTMLElement).inert = v;
    }
  }

  open() {
    if (this.isOpen()) return;
    this.deps.onOpening();
    const view = $("map-view");
    view.hidden = false;
    this.setChatInert(true);
    view.focus({ preventScroll: true });
    nav.push("map");
    void this.load();
    this.timer = setInterval(() => void this.load(), 3000);
  }

  close() {
    const was = this.isOpen();
    $("map-view").hidden = true;
    // Ne lever l'inert que si les autres overlays #chat sont aussi
    // fermés — sinon on ré-expose #input derrière conn-view/settings/
    // components (frappes invisibles, le bug exact que l'inert prévient).
    if ($("conn-view").hidden && $("settings-view").hidden && $("components-view").hidden) {
      this.setChatInert(false);
    }
    if (this.timer) {
      clearInterval(this.timer);
      this.timer = null;
    }
    this.hideInfo();
    nav.close("map");
    if (!was) return;
    if (!isAndroid()) ($("input") as HTMLTextAreaElement).focus();
  }

  toggle() {
    if (this.isOpen()) this.close();
    else this.open();
  }

  // --- Données -----------------------------------------------------------

  private async load() {
    const conn = this.deps.conn();
    const [sess, prs, comps] = await Promise.all([
      call<SessionMeta[]>("list_sessions").catch(() => [] as SessionMeta[]),
      call<PrEntry[]>("pr_list").catch(() => [] as PrEntry[]),
      conn && this.deps.componentsCapable(conn)
        ? call<CompItem[]>("list_components", { conn }).catch(
            () => [] as CompItem[],
          )
        : Promise.resolve([] as CompItem[]),
    ]);
    if (!this.isOpen()) return;
    // Sentinelle `{conn, conn_error:true}` injectée par la fusion quand la
    // sonde d'une conn timeout — pas une session : à filtrer avant le tri
    // (created_at absent → localeCompare lèverait).
    const alive = sess.filter(
      (s) => !(s as { conn_error?: boolean }).conn_error,
    );
    // Tri stable → positions identiques d'un poll à l'autre (le graphe ne
    // saute pas à chaque rafraîchissement). Copie : le tableau retourné
    // par le RPC peut être partagé avec d'autres consommateurs.
    const sorted = [...alive].sort(
      (a, b) =>
        a.created_at.localeCompare(b.created_at) || a.id.localeCompare(b.id),
    );
    this.data = {
      sess: conn ? sorted.filter((s) => (s.conn ?? conn) === conn) : sorted,
      prs: conn ? prs.filter((p) => !p.conn || p.conn === conn) : prs,
      comps,
    };
    // Signature : un tick sans changement ne re-rend pas — le rebuild
    // innerHTML détruirait sinon le focus clavier et le :hover à chaque poll.
    const sig = JSON.stringify(this.data);
    if (sig !== this.lastSig) {
      this.lastSig = sig;
      this.render();
    }
    this.renderInfo();
  }

  // --- Rendu -------------------------------------------------------------

  private render() {
    const { sess, prs, comps } = this.data;
    const conn = this.deps.conn();
    const pos = new Map<string, Pt>([["daemon", [0, 0]]]);

    // Composants sur l'anneau interne (tri par nom → déterministe).
    const cs = [...comps].sort((a, b) => a.name.localeCompare(b.name));
    const RC = Math.max(130, (cs.length * 160) / (2 * Math.PI));
    cs.forEach((c, i) => {
      const a = -Math.PI / 2 + (2 * Math.PI * i) / cs.length;
      pos.set(`c:${c.name}`, [RC * Math.cos(a), RC * Math.sin(a)]);
    });

    // Parent de chaque session : le composant nommé par `origin`
    // (component:<name> et composant réellement listé — un nom inconnu
    // tombe sur le fallback spawned_by/démon), la session `spawned_by`,
    // le démon sinon.
    const compNames = new Set(cs.map((c) => c.name));
    const sessIds = new Set(sess.map((s) => s.id));
    const under = new Map<string, SessionMeta[]>();
    const roots: SessionMeta[] = [];
    for (const s of sess) {
      const oc = s.origin?.startsWith("component:")
        ? s.origin.slice("component:".length)
        : "";
      const pk =
        oc && compNames.has(oc)
          ? `c:${oc}`
          : s.spawned_by && sessIds.has(s.spawned_by)
            ? `s:${s.spawned_by}`
            : "";
      if (pk) {
        const a = under.get(pk) ?? [];
        a.push(s);
        under.set(pk, a);
      } else roots.push(s);
    }

    // Sessions racines sur l'anneau externe — déphasées d'un demi-pas pour
    // ne pas s'aligner exactement sur l'axe d'un composant.
    const RS = Math.max(
      RC + RHO_KIDS + 85,
      (Math.max(roots.length, 1) * 52) / (2 * Math.PI),
    );
    roots.forEach((s, i) => {
      const a = -Math.PI / 2 + (2 * Math.PI * (i + 0.5)) / Math.max(roots.length, 1);
      pos.set(`s:${s.id}`, [RS * Math.cos(a), RS * Math.sin(a)]);
    });

    // Enfants d'un nœud : petit cercle autour de lui, premier enfant
    // décalé de l'axe parent→centre pour dégager l'arête entrante.
    const placeKids = (pk: string): string[] => {
      const kids = under.get(pk) ?? [];
      const pp = pos.get(pk);
      if (!pp) return []; // parent jamais placé (cycle spawned_by) — skip
      const [px, py] = pp;
      const a0 = Math.atan2(py, px);
      kids.forEach((s, i) => {
        const a = a0 + Math.PI / kids.length + (2 * Math.PI * i) / kids.length;
        pos.set(`s:${s.id}`, [
          px + RHO_KIDS * Math.cos(a),
          py + RHO_KIDS * Math.sin(a),
        ]);
      });
      return kids.map((s) => s.id);
    };
    // BFS : composants puis sessions — le parent est toujours placé avant
    // ses enfants quelle que soit la profondeur de la chaîne de spawn.
    const queue: string[] = [];
    for (const c of cs) queue.push(...placeKids(`c:${c.name}`));
    for (const s of roots) queue.push(s.id);
    while (queue.length) queue.push(...placeKids(`s:${queue.shift()!}`));

    // --- Arêtes ---
    const edges: string[] = [];
    for (const c of cs) {
      const [x, y] = pos.get(`c:${c.name}`)!;
      const st = compCls(c.status?.state);
      const k = c.kind in KIND_ICON ? c.kind : "tool";
      edges.push(
        `<line class="e e-dc k-${k} st-${st}${st === "running" ? " flow" : ""}" ` +
          `x1="0" y1="0" x2="${x.toFixed(1)}" y2="${y.toFixed(1)}"/>`,
      );
    }
    const sessEdge = (s: SessionMeta, pk: string, i: number) => {
      const pp = pos.get(pk);
      const sp = pos.get(`s:${s.id}`);
      // Chaîne spawned_by cyclique (donnée démon) : les nœuds du cycle
      // n'ont pas de position — ni arête ni nœud pour eux.
      if (!pp || !sp) return;
      const [px, py] = pp;
      const [x, y] = sp;
      // Arc quadratique : contrôle = milieu + normale, signe alterné par
      // index → les fratries s'écartent au lieu de se superposer.
      const dx = x - px;
      const dy = y - py;
      const len = Math.hypot(dx, dy) || 1;
      const sgn = i % 2 ? 1 : -1;
      const cx = (px + x) / 2 + (-dy / len) * len * 0.14 * sgn;
      const cy = (py + y) / 2 + (dx / len) * len * 0.14 * sgn;
      const st = dotCls(s.state);
      edges.push(
        `<path class="e e-s st-${st}${ACTIVE.has(s.state) ? " flow" : ""}" ` +
          `d="M ${px.toFixed(1)} ${py.toFixed(1)} Q ${cx.toFixed(1)} ${cy.toFixed(1)} ${x.toFixed(1)} ${y.toFixed(1)}"/>`,
      );
    };
    for (const [pk, kids] of under) kids.forEach((s, i) => sessEdge(s, pk, i));
    roots.forEach((s, i) => sessEdge(s, "daemon", i));

    // --- Nœuds ---
    const nodes: string[] = [];
    nodes.push(
      `<g class="n n-daemon"><title>agentcontrol-daemon</title>` +
        `<circle class="nc" r="${R_DAEMON}"/>` +
        nest(gemSVG("agent"), -16, -16, 32) +
        `<text class="nl" y="${R_DAEMON + 16}">${esc(t("map_daemon"))}</text></g>`,
    );
    for (const c of cs) {
      const [x, y] = pos.get(`c:${c.name}`)!;
      const st = compCls(c.status?.state);
      const k = c.kind in KIND_ICON ? c.kind : "tool";
      const tip = [
        c.name,
        `${c.kind} · ${c.status?.state ?? "?"}`,
        c.status?.version ?? "",
        c.status?.uptime_s ? `${t("map_uptime")} ${fmtUp(c.status.uptime_s)}` : "",
        c.description ?? "",
      ]
        .filter(Boolean)
        .join("\n");
      nodes.push(
        `<g class="n n-comp st-${st}" transform="translate(${x.toFixed(1)} ${y.toFixed(1)})" ` +
          `data-comp="${q(c.name)}" role="button" tabindex="0">` +
          `<title>${esc(tip)}</title>` +
          `<circle class="nc" r="${R_COMP}"/>` +
          nest(icon(KIND_ICON[k]), -10, -10, 20) +
          `<circle class="nd" cx="15" cy="-15" r="4.5"/>` +
          `<text class="nl" y="${R_COMP + 15}">${esc(c.name)}</text></g>`,
      );
    }
    const prOf = new Map<string, number>();
    for (const p of prs) if (p.session && p.pr) prOf.set(p.session, p.pr);
    for (const s of sess) {
      const sp = pos.get(`s:${s.id}`);
      if (!sp) continue; // membre d'un cycle spawned_by — non rendu
      const [x, y] = sp;
      const st = dotCls(s.state);
      const pr = prOf.get(s.id);
      const via = s.origin ? `\n${t("via")} ${s.origin}` : "";
      nodes.push(
        `<g class="n n-sess st-${st}" transform="translate(${x.toFixed(1)} ${y.toFixed(1)})" ` +
          `data-session="${q(s.id)}" role="button" tabindex="0">` +
          `<title>${esc(`${s.title}\n${s.state} · ${s.agent}${via}`)}</title>` +
          `<circle class="nc" r="${R_SESS}"/>` +
          nest(dotIcon(s.state), -10, -10, 20) +
          `<text class="nl" y="${R_SESS + 14}">${esc(trunc(s.title, 18))}` +
          (pr ? ` <tspan class="npr">#${pr}</tspan>` : "") +
          `</text></g>`,
      );
    }

    const M = Math.ceil(Math.max(RS, 200) + RHO_KIDS + R_SESS + 95);
    const svg = $("map-svg");
    svg.setAttribute("viewBox", `${-M} ${-M} ${2 * M} ${2 * M}`);
    svg.innerHTML =
      `<g class="edges">${edges.join("")}</g>` +
      `<g class="nodes">${nodes.join("")}</g>` +
      (conn
        ? ""
        : `<text class="map-empty" x="0" y="${R_DAEMON + 44}">${esc(t("map_empty"))}</text>`);
    $("map-stats").textContent = t("map_stats", {
      comps: String(comps.length),
      sess: String(sess.length),
    });
  }

  // --- Interactions ------------------------------------------------------

  private activate(g: Element) {
    const sid = g.getAttribute("data-session");
    if (sid) {
      this.deps.openSession(sid, this.deps.conn());
      this.close();
      return;
    }
    const cn = g.getAttribute("data-comp");
    if (cn) {
      this.infoComp = this.infoComp === cn ? "" : cn;
      this.renderInfo();
    }
  }

  private hideInfo() {
    this.infoComp = "";
    this.lastInfoSig = "";
    $("map-info").hidden = true;
  }

  /** Carte d'info du composant sélectionné (interim du panneau §2.6 —
   *  la gestion start/stop viendra avec la vue dédiée). Re-rendue à
   *  chaque poll pour suivre l'état en direct. */
  private renderInfo() {
    const el = $("map-info");
    const c = this.data.comps.find((x) => x.name === this.infoComp);
    if (!c) {
      this.infoComp = "";
      this.lastInfoSig = "";
      el.hidden = true;
      return;
    }
    const sig = this.infoComp + JSON.stringify(c);
    if (sig === this.lastInfoSig) return;
    this.lastInfoSig = sig;
    const st: Partial<CompStatus> = c.status ?? {};
    const row = (k: string, v?: string | number) =>
      v === undefined || v === ""
        ? ""
        : `<div class="mi-row"><span>${esc(k)}</span><b>${esc(String(v))}</b></div>`;
    el.innerHTML =
      `<header><b>${esc(c.name)}</b><span class="badge">${esc(c.kind)}</span>` +
      `<button id="mi-x" data-icon="close" title="${q(esc(t("close")))}"></button></header>` +
      (c.description ? `<p class="mi-desc">${esc(c.description)}</p>` : "") +
      `<div class="mi-rows">` +
      row(t("map_state"), st.state) +
      row(t("map_version"), st.version) +
      row(t("map_health"), st.health) +
      row("PID", st.pid) +
      row(t("map_uptime"), st.uptime_s ? fmtUp(st.uptime_s) : undefined) +
      row(t("map_restarts"), st.restarts) +
      row(t("map_last_exit"), st.last_exit) +
      row(
        t("map_autostart"),
        c.autostart === undefined ? undefined : c.autostart ? "✓" : "—",
      ) +
      row(t("map_source"), c.source) +
      `</div>`;
    el.hidden = false;
    hydrateIcons(el);
    ($("mi-x") as HTMLButtonElement).onclick = () => this.hideInfo();
  }

  init() {
    $("tb-map").onclick = () => this.toggle();
    $("map-close").onclick = () => this.close();
    // Délégation : les nœuds sont réécrits à chaque poll — un handler par
    // nœud mourrait avec le DOM précédent.
    $("map-svg").addEventListener("click", (e) => {
      const g = (e.target as Element).closest?.("[data-session],[data-comp]");
      if (g) this.activate(g);
      else this.hideInfo();
    });
    $("map-view").addEventListener("keydown", (e) => {
      if (e.key !== "Enter") return;
      const g = (e.target as Element).closest?.("[data-session],[data-comp]");
      if (g) this.activate(g);
    });
    // Même garde que settings : Échap ferme la vue sauf si un dialogue
    // modal est ouvert par-dessus.
    window.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && this.isOpen() && !document.querySelector("dialog[open]"))
        this.close();
    });
  }
}
