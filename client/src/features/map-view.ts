// ---------------------------------------------------------------------------
// Vue diagramme : graphe radial des composants et des sessions autour du
// démon (spec docs/components-integration.md §2.7). Overlay plein #chat
// (mécanique settings-view : inert + nav.push), SVG vanilla sans dep, poll
// ~3 s uniquement tant que la vue est ouverte — pas de canal d'events
// daemon-wide (le Subscribe proto est par-session).
//
// Layout déterministe = arbre radial sectorisé : chaque racine (composant
// ou session sans parent) reçoit un secteur angulaire proportionnel à son
// nombre de feuilles, subdivisé récursivement entre ses enfants — les
// formes ne peuvent pas se chevaucher (secteurs disjoints ; les labels
// texte sous les nœuds peuvent encore se chevaucher sur les arbres très
// denses). Le rayon code la profondeur : démon au
// centre, programmes (chips) sur l'anneau interne, agents (cercles) à
// RC + profondeur·STEP. Parent : `origin = component:<name>` (#24/§2.3.1),
// la session `spawned_by`, le démon sinon.
// Rôles distincts : programme = chip rectangulaire, agent = cercle,
// démon = gemme ; arêtes typées (supervision / observé / spawn) + légende.
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
const R_SESS = 15;
/** Hauteur des chips programme (largeur calculée sur le nom). */
const CHIP_H = 30;
/** Pas radial entre deux profondeurs de sessions. */
const STEP_MIN = 96;
/** Corde minimale entre deux nœuds voisins de même profondeur. */
const PITCH_MIN = 44;

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
  /** Ouverture/fermeture effective — la sidebar y reflète son onglet
   *  « Diagramme » (sv-map), qui lance cette vue. */
  onToggle: (open: boolean) => void;
}

export class MapView {
  private deps: MapDeps = {
    conn: () => "",
    componentsCapable: () => false,
    openSession: () => {},
    onOpening: () => {},
    onToggle: () => {},
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
    this.deps.onToggle(true);
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
    if (was) this.deps.onToggle(false);
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
    // innerHTML détruirait sinon le focus clavier et le :hover à chaque
    // poll. `conn` et la capability sont inclus : ils pilotent le rendu
    // (filtre sessions, map-empty, note « aucun composant ») sans faire
    // partie des données — sinon leur changement laisserait un rendu
    // périmé tant que la data ne bouge pas.
    const sig =
      JSON.stringify(this.data) +
      "|" + conn +
      "|" + (conn ? this.deps.componentsCapable(conn) : "");
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

    // Composants triés par nom → déterministe, et largeur de chip par nom.
    const cs = [...comps].sort((a, b) => a.name.localeCompare(b.name));
    const chipW = new Map<string, number>();
    for (const c of cs) {
      chipW.set(c.name, Math.max(58, trunc(c.name, 17).length * 7.4 + 34));
    }

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

    // --- Arbre sectorisé ---------------------------------------------------
    // Chaque racine (composant ou session sans parent) reçoit un secteur
    // angulaire proportionnel à son nombre de feuilles, subdivisé
    // récursivement entre ses enfants : deux nœuds ne peuvent jamais se
    // chevaucher (secteurs disjoints), quelle que soit la topologie.
    interface TNode {
      key: string;
      leaf: number;
      kids: TNode[];
    }
    const build = (key: string, stack: Set<string>): TNode | null => {
      if (stack.has(key)) return null; // cycle spawned_by → non placé
      stack.add(key);
      const tk: TNode[] = [];
      for (const s of under.get(key) ?? []) {
        const t = build(`s:${s.id}`, stack);
        if (t) tk.push(t);
      }
      stack.delete(key);
      return {
        key,
        leaf: Math.max(1, tk.reduce((a, t) => a + t.leaf, 0)),
        kids: tk,
      };
    };
    // Les clés `c:` ne sont jamais enfants → build non nul ; la garde de
    // cycle ne peut rejeter que des sessions inaccessibles depuis les
    // racines (elles restent simplement non rendues, comme avant).
    const compTrees = cs
      .map((c) => build(`c:${c.name}`, new Set()))
      .filter((t): t is TNode => t !== null);
    const sessTrees = roots
      .map((s) => build(`s:${s.id}`, new Set()))
      .filter((t): t is TNode => t !== null);
    // Unités de slot de chaque racine : session = son nombre de feuilles ;
    // composant = max(feuilles, la place que prend sa chip sur l'anneau) —
    // un slot = PITCH_MIN px d'arc, la corde est toujours < l'arc → deux
    // racines voisines ne peuvent pas se toucher quelle que soit la
    // topologie. Les enfants subdivisent le secteur parent ∝ feuilles.
    const compU = new Map<string, number>();
    let U = sessTrees.reduce((a, t) => a + t.leaf, 0);
    for (const t of compTrees) {
      const u = Math.max(
        t.leaf,
        Math.ceil((chipW.get(t.key.slice(2))! + 16) / PITCH_MIN),
      );
      compU.set(t.key, u);
      U += u;
    }
    U = Math.max(1, U);
    const spanU = (2 * Math.PI) / U;
    // Anneau interne des programmes : chaque unité = PITCH_MIN d'arc.
    // Contrainte corde : deux chips voisines sur l'anneau ne doivent pas
    // se toucher — pour les grands secteurs la corde sature (< l'arc),
    // d'où le calcul exact par paires adjacentes plutôt que l'arc seul.
    const pairNeed = (a: TNode, b: TNode) => {
      const ang = ((compU.get(a.key)! + compU.get(b.key)!) / 2) * spanU;
      const need =
        chipW.get(a.key.slice(2))! / 2 + chipW.get(b.key.slice(2))! / 2 + 10;
      return need / (2 * Math.sin(Math.min(Math.PI / 2, ang / 2)));
    };
    // La contrainte arc porte sur le rayon des SESSIONS (RC+STEP) — les
    // chips n'ont besoin que de pairNeed ; le −STEP évite ~96 px de rayon
    // superflu sur les gros graphes.
    let RC = Math.max(
      120,
      (U * PITCH_MIN) / (2 * Math.PI) - STEP_MIN,
    );
    for (let i = 1; i < compTrees.length; i++) {
      RC = Math.max(RC, pairNeed(compTrees[i - 1], compTrees[i]));
    }
    // Paire de bouclage : adjacente seulement sans sessions entre les deux.
    if (compTrees.length > 1 && sessTrees.length === 0) {
      RC = Math.max(RC, pairNeed(compTrees[compTrees.length - 1], compTrees[0]));
    }
    const STEP = STEP_MIN;
    // Profondeur max en nombre de sessions (la racine composant ne compte
    // pas — ses enfants démarrent au rayon RC + STEP comme les sessions
    // racines) : borne le rayon externe.
    const tdepth = (t: TNode): number =>
      1 + Math.max(0, ...t.kids.map(tdepth));
    const dmax = Math.max(
      1,
      ...sessTrees.map(tdepth),
      ...compTrees.map((t) => tdepth(t) - 1),
    );
    const place = (t: TNode, r: number, a0: number, span: number) => {
      pos.set(t.key, [r * Math.cos(a0 + span / 2), r * Math.sin(a0 + span / 2)]);
      let cur = a0;
      for (const k of t.kids) {
        const ks = (span * k.leaf) / t.leaf;
        place(k, r + STEP, cur, ks);
        cur += ks;
      }
    };
    let cur = -Math.PI / 2;
    for (const t of compTrees) {
      const s = spanU * compU.get(t.key)!;
      place(t, RC, cur, s);
      cur += s;
    }
    for (const t of sessTrees) {
      const s = spanU * t.leaf;
      place(t, RC + STEP, cur, s);
      cur += s;
    }

    // --- Arêtes ---
    // daemon→programme : « supervises » (kind process, cycle de vie géré)
    // ou « observes » (internal/external/tool — connu, pas de process).
    const edges: string[] = [];
    for (const c of cs) {
      const [x, y] = pos.get(`c:${c.name}`)!;
      const st = compCls(c.status?.state);
      const k = c.kind in KIND_ICON ? c.kind : "tool";
      const manage = c.kind === "process";
      edges.push(
        `<line class="e e-dc ${manage ? "e-manage" : "e-know"} k-${k} st-${st}` +
          `${st === "running" ? " flow" : ""}" ` +
          `x1="0" y1="0" x2="${x.toFixed(1)}" y2="${y.toFixed(1)}">` +
          `<title>${esc(manage ? t("map_leg_manage") : `${c.kind} · ${t("map_leg_know")}`)}</title></line>`,
      );
    }
    // parent→session : arête « spawned » courbe, couleur = état de
    // l'enfant, flux animé si la session travaille.
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
          `d="M ${px.toFixed(1)} ${py.toFixed(1)} Q ${cx.toFixed(1)} ${cy.toFixed(1)} ${x.toFixed(1)} ${y.toFixed(1)}">` +
          `<title>${esc(t("map_leg_spawn"))}</title></path>`,
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
    // Programme = chip rectangulaire (icône kind + nom dans la chip,
    // pastille d'état en coin) — visuellement distinct des agents.
    for (const c of cs) {
      const [x, y] = pos.get(`c:${c.name}`)!;
      const st = compCls(c.status?.state);
      const k = c.kind in KIND_ICON ? c.kind : "tool";
      const w = chipW.get(c.name)!;
      const label = trunc(c.name, 17);
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
          `<rect class="nc" x="${(-w / 2).toFixed(1)}" y="${-CHIP_H / 2}" ` +
          `width="${w.toFixed(1)}" height="${CHIP_H}" rx="9"/>` +
          nest(icon(KIND_ICON[k]), -w / 2 + 7, -8, 16) +
          `<text class="nt" x="${(-w / 2 + 28).toFixed(1)}" y="4.5">${esc(label)}</text>` +
          `<circle class="nd" cx="${(w / 2 - 2).toFixed(1)}" cy="${-CHIP_H / 2}" r="4.5"/></g>`,
      );
    }
    const prOf = new Map<string, number>();
    for (const p of prs) if (p.session && p.pr) prOf.set(p.session, p.pr);
    // Agent = cercle (icône d'état), titre sous le nœud + nom de l'agent
    // en sous-titre + badge PR éventuel.
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
          `</text>` +
          `<text class="nl nl-sub" y="${R_SESS + 26}">${esc(trunc(s.agent, 14))}</text></g>`,
      );
    }

    const M = Math.ceil(Math.max(RC + dmax * STEP + R_SESS + 58, 220));
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
    // Composants absents : l'expliciter dans la légende plutôt qu'un
    // anneau silencieusement vide — rien de configuré, ou démon trop
    // ancien pour les ReqKind Component* (gate capability). Sans
    // connexion, map-empty porte déjà le message.
    const nc = $("map-nocomps");
    nc.hidden = !conn || comps.length > 0;
    if (!nc.hidden) {
      nc.textContent = t(
        this.deps.componentsCapable(conn) ? "map_nocomps" : "map_nocomps_old",
      );
    }
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
    // Entrée labellée du sélecteur de vues — l'endroit où l'on cherche
    // les vues ; le drawer Android recouvrirait l'overlay (le fermer).
    $("sv-map").onclick = () => {
      nav.closeDrawer();
      this.toggle();
    };
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
