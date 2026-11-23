// ---------------------------------------------------------------------------
// Panneau « Composants » : programmes managés par le démon de la connexion
// liée (docs/components-integration.md §2.6) — overlay plein #chat comme
// settings-view/conn-view (inert + nav + Échap). Onglets Installés/Catalogue,
// éditeur de spec (ComponentUpsert/Remove) et viewer de logs en sous-vues.
// Poll doux ~3 s seulement tant que le panneau est ouvert — pas de canal
// d'events daemon-wide (Subscribe est par-session). Capability-gated :
// le bouton titlebar n'apparaît que si daemon_status.components === true.
// ---------------------------------------------------------------------------

import { $, dlgErr, esc, logUi, notify, q } from "../core/dom";
import { call } from "../core/rpc";
import { app } from "../core/state";
import { isAndroid, shellOpen } from "../core/chrome";
import { t, type Key } from "../i18n";
import { icon } from "../ui/icons";
import { nav } from "../ui/nav";
import { catalogFor, ensureCatalog } from "./modelcatalog";
import type {
  AgentOpt,
  CatalogEntry,
  ComponentHealthcheck,
  ComponentItem,
  ComponentKind,
  ComponentSpec,
  ParamSpec,
} from "../core/types";
import type { ConnectionManager } from "./connections";
import { askUnsaved } from "../core/unsaved";
import {
  ParamField,
  canonJson,
  paramBool,
  paramJson,
  paramList,
  secLabel,
  validateParam,
} from "./params-form";
import { ORCH_COMPONENT, ParamsView } from "./params-view";

export { ORCH_COMPONENT };

/** Sous-vues du panneau : les deux onglets + éditeur + logs. */
type Mode = "list" | "catalog" | "edit" | "logs" | "params";

const POLL_MS = 3_000;
/** "default" = sentinelle de regroupement frontend — la passerelle attend
 *  conn absent/"" pour la connexion par défaut, pas la chaîne littérale. */
const apiConn = (c: string) => (c === "default" ? undefined : c);
const b64dec = (s: string) => Uint8Array.from(atob(s), (c) => c.charCodeAt(0));

/** Uptime compact : 42s / 5m / 2h / 3d (uptime_s du démon). */
const fmtUp = (s: number) =>
  s >= 86400 ? `${Math.floor(s / 86400)}d`
  : s >= 3600 ? `${Math.floor(s / 3600)}h`
  : s >= 60 ? `${Math.floor(s / 60)}m`
  : `${s}s`;

/** États connus du proto — whitelist pour la classe CSS et la clé i18n :
 *  un état inattendu retombe sur le libellé brut, jamais interpolé. */
const KNOWN_ST = new Set([
  "starting", "running", "degraded", "stopped", "failed",
  // États d'observation des `tool` (pas de cycle de vie) — proto ≥0.8.
  "installed", "absent",
]);

const LINES = (id: string) =>
  ($(id) as HTMLTextAreaElement).value
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean);

/** Spec vierge d'un nouveau composant (défauts du démon : on-failure/5 s). */
function defaultSpec(): ComponentSpec {
  return {
    name: "",
    kind: "process",
    command: [],
    env: {},
    cwd: null,
    autostart: false,
    restart: "on-failure",
    restart_sec: 5,
    healthcheck: { kind: "none" },
    description: null,
  };
}

export class ComponentsPanel {
  private conns!: ConnectionManager;
  /** Fermer les autres overlays de #chat avant l'ouverture (injecté). */
  onOpening: () => void = () => {};
  /** Ouvrir la vue plugin d'un composant (injecté — feature
   *  plugin-view, évite l'import circulaire). */
  openView: (name: string, conn: string) => void = () => {};
  private mode: Mode = "list";
  /** Vue pleine page « Configurer » (paramètres déclarés). */
  private params = new ParamsView({
    connParam: () => this.connParam(),
    pool: (cur, ps) => this.poolEditor(cur, ps, this.paramsCurrent),
    loadPool: () => this.loadPoolAgents(),
    statusLine: (it) => this.metaText(it),
    reload: async (name) => {
      await this.refreshList(true);
      return this.items.find((x) => x.name === name) ?? null;
    },
    back: () => {
      this.setMode("list");
      void this.refreshList(true);
    },
  });
  /** Valeurs courantes de l'item configuré (widget équipe : max_working). */
  private paramsCurrent: Record<string, string> = {};
  private items: ComponentItem[] = [];
  private catalog: CatalogEntry[] = [];
  /** Première réponse list/catalog reçue — avant ça les vides affichent
   *  « chargement » et non « aucun composant » (le flash serait trompeur). */
  private listLoaded = false;
  private catalogLoaded = false;
  private pollTimer: ReturnType<typeof setInterval> | undefined;
  private pollBusy = false;
  /** Fingerprint de la dernière liste rendue — le poll ne rebâtit le DOM
   *  que si la structure a bougé (voir renderList/fpOf). */
  private renderedFp = "";
  /** Composant dont le log tail est affiché (mode logs). */
  private logsName = "";
  /** Nom du composant en édition ("" = nouvelle spec). */
  private editName = "";
  /** Composant adossé au catalogue (item.catalog du démon) : l'édition
   *  est bornée aux params déclarés + autostart/description — la spec
   *  interne est scellée (upsert_edit côté démon) et affichée en
   *  lecture seule. false → spec custom, éditeur complet legacy. */
  private editBound = false;
  /** Spec chargée à l'ouverture de l'éditeur — source des valeurs
   *  params et du dump internals en mode borné. */
  private editSpec: ComponentSpec | null = null;
  /** Suppression armée dans l'éditeur (2e tap = ComponentRemove) —
   *  timer de désarmement, même pattern que le danger-armed du ctxmenu. */
  private delArmed = false;
  private delTimer: ReturnType<typeof setTimeout> | undefined;
  /** Caps des profils agents pour le widget « worker_pool » — chargé
   *  une fois par connexion à l'ouverture du formulaire (dropdowns
   *  modèle/effort/mode par profil choisi dans la ligne). */
  private poolAgents = new Map<string, AgentOpt>();
  private poolAgentsConn = "";

  bind(conns: ConnectionManager) {
    this.conns = conns;
  }

  private isOpen() {
    return !$("components-view").hidden;
  }

  /** Connexion pilotée : celle de la session affichée, sinon la conn liée
   *  (modèle multi-instances : une instance = une connexion). */
  private connKey(): string {
    return app.current?.conn ?? this.conns.knownConns()[0] ?? "default";
  }
  private connParam() {
    return apiConn(this.connKey());
  }
  /** Capability gate : le démon connaît les ReqKind Component*. */
  capable(): boolean {
    return this.conns.cap(this.connKey())?.components === true;
  }
  /** Visibilité du bouton titlebar — rejouée sur chaque capsChanged. */
  syncBtn() {
    $("tb-components").hidden = !this.capable();
  }

  /** Rend les éléments recouverts de #chat inertes — même mécanisme que
   *  settings-view/conn-view (un <div> ne capte pas le focus comme un
   *  <dialog> modal). */
  private setChatInert(v: boolean) {
    for (const el of [...$("chat").children]) {
      if (el.id !== "components-view") (el as HTMLElement).inert = v;
    }
  }

  open() {
    if (this.isOpen()) return;
    if (!this.capable()) {
      notify("warn", t("comp_unsupported"));
      return;
    }
    this.onOpening();
    const view = $("components-view");
    view.hidden = false;
    this.setChatInert(true);
    // Focus au panneau : un focus résiduel dans #input enverrait des
    // frappes invisibles à l'agent (même motif que settings-view).
    view.focus({ preventScroll: true });
    nav.push("components");
    this.setMode("list");
    // Premier chargement « bruyant » : une conn down doit s'afficher
    // (comp-err), pas rester invisible dans client.log.
    void this.refreshList();
    this.pollTimer = setInterval(() => void this.tick(), POLL_MS);
  }

  close() {
    const was = this.isOpen();
    $("components-view").hidden = true;
    if (this.pollTimer) {
      clearInterval(this.pollTimer);
      this.pollTimer = undefined;
    }
    // L'inert ne tombe que si les autres overlays #chat sont fermés —
    // sinon #input serait ré-exposé derrière eux (frappes invisibles).
    if ($("conn-view").hidden && $("settings-view").hidden && $("map-view").hidden && $("logs-view").hidden && $("plugin-view").hidden)
      this.setChatInert(false);
    nav.close("components");
    if (!was) return;
    if (!isAndroid()) ($("input") as HTMLTextAreaElement).focus();
  }

  toggle() {
    if (this.isOpen()) this.close();
    else this.open();
  }

  /** Bascule de sous-vue : liste / catalogue / éditeur / logs — les
   *  onglets restent visibles dans les sous-vues (cliquer = quitter). */
  private setMode(m: Mode) {
    this.mode = m;
    $("comp-list").hidden = m !== "list";
    $("comp-catalog").hidden = m !== "catalog";
    $("comp-editor").hidden = m !== "edit";
    $("comp-logs-view").hidden = m !== "logs";
    $("comp-params").hidden = m !== "params";
    $("components-view").classList.toggle("cv-params", m === "params");
    if (m !== "params") this.params.reset();
    $("comp-tab-list").classList.toggle("on", m === "list");
    $("comp-tab-cat").classList.toggle("on", m === "catalog");
    const browsing = m === "list" || m === "catalog";
    $("comp-add").hidden = m !== "list";
    $("comp-refresh").hidden = !browsing;
    this.disarmDel();
    dlgErr("comp-err", null);
  }

  /** Poll doux : liste en onglet Installés, tail du log en vue logs —
   *  rien en édition (écraserait la saisie) ni au catalogue (données
   *  quasi-statiques, refresh explicite). */
  private async tick() {
    if (this.pollBusy || !this.isOpen()) return;
    this.pollBusy = true;
    try {
      if (this.mode === "list") await this.refreshList(true);
      else if (this.mode === "logs") await this.refreshLogs();
    } finally {
      this.pollBusy = false;
    }
  }

  private refresh() {
    if (this.mode === "catalog") void this.refreshCatalog(true);
    else void this.refreshList();
  }

  // -------------------------------------------------------------------
  // Onglet Installés
  // -------------------------------------------------------------------

  private async refreshList(silent = false) {
    // Render avant le fetch : tant que rien n'a répondu la zone affiche
    // « chargement… » plutôt qu'un blanc.
    if (this.mode === "list") this.renderList();
    try {
      this.items = await call<ComponentItem[]>("list_components", {
        conn: this.connParam(),
      });
      this.listLoaded = true;
      if (this.mode === "list") {
        dlgErr("comp-err", null);
        this.renderList();
      }
    } catch (e) {
      // Poll : silencieux (conn down = retries déjà gérés par conns) ;
      // action explicite : erreur visible dans le panneau.
      if (!silent) dlgErr("comp-err", String(e));
      else logUi("debug", `list_components: ${e}`);
    }
  }

  private async lifecycle(name: string, action: "start" | "stop" | "restart") {
    try {
      await call(`component_${action}`, { name, conn: this.connParam() });
    } catch (e) {
      dlgErr("comp-err", String(e));
    }
    await this.refreshList(true);
  }

  private empty(txt: string): HTMLElement {
    const d = document.createElement("div");
    d.className = "comp-none";
    d.textContent = txt;
    return d;
  }

  /** Ligne composant : dot d'état animé, nom, badge kind, métadonnées
   *  (uptime/restarts/health/last_exit) et actions contextuelles selon
   *  kind — cycle de vie et logs : `process` seulement (les autres kinds
   *  sont refusés côté démon). */
  private compRow(it: ComponentItem): HTMLElement {
    const st = it.status;
    const state = st.state;
    const row = document.createElement("div");
    row.className = "comp";

    const dot = document.createElement("span");
    dot.className = `comp-dot cst-${KNOWN_ST.has(state) ? state : "stopped"}`;
    dot.title = KNOWN_ST.has(state) ? t(`comp_st_${state}` as Key) : state;
    row.appendChild(dot);

    const main = document.createElement("div");
    main.className = "comp-main";
    const line1 = document.createElement("div");
    line1.className = "comp-l1";
    line1.innerHTML =
      `<span class="comp-name">${esc(it.name)}</span>` +
      `<span class="comp-kind k-${it.kind}">${esc(it.kind)}</span>` +
      (it.source === "internal"
        ? `<span class="comp-src">${esc(t("comp_internal"))}</span>`
        : "") +
      (it.autostart && it.kind === "process"
        ? `<span class="comp-src" title="${q(esc(t("comp_f_autostart")))}">auto</span>`
        : "");
    line1.title = it.description ?? "";
    main.appendChild(line1);

    const meta = this.metaText(it);
    if (meta) {
      const line2 = document.createElement("div");
      line2.className = "comp-meta";
      line2.textContent = meta;
      main.appendChild(line2);
    }
    row.appendChild(main);

    const acts = document.createElement("div");
    acts.className = "comp-acts";
    const act = (
      ic: Parameters<typeof icon>[0],
      title: string,
      fn: () => void,
      show = true,
    ) => {
      if (!show) return;
      const b = document.createElement("button");
      b.type = "button";
      b.innerHTML = icon(ic);
      b.title = title;
      b.onclick = (e) => {
        e.stopPropagation();
        fn();
      };
      acts.appendChild(b);
    };
    const managed = it.kind === "process";
    const alive = state === "running" || state === "starting" || state === "degraded";
    act("play", t("comp_start"), () => void this.lifecycle(it.name, "start"),
      managed && !alive);
    act("close", t("comp_stop"), () => void this.lifecycle(it.name, "stop"),
      managed && alive);
    act("refresh", t("comp_restart"), () => void this.lifecycle(it.name, "restart"),
      managed && alive);
    act("terminal", t("comp_logs"), () => this.openLogs(it.name), managed);
    // Vue plugin (option B) : iframe sandboxée servie par le démon —
    // flag `view` calculé côté démon (spec.view_dir ou convention
    // components/<name>/web/ existante).
    act("panel", t("comp_view"),
      () => this.openView(it.name, this.connKey()), it.view === true);
    // Params déclarés = la seule surface de config utilisateur
    // (catalogue) — « Configurer » ouvre le formulaire schema-driven,
    // surtout utile quand config_required bloque le démarrage.
    act("settings", t("comp_config"),
      () => void this.configure(it),
      (it.spec.params_schema?.length ?? 0) > 0);
    // internal : déclaré en code → ni édition ni suppression (le démon
    // refuse ; n'afficher que les actions réellement jouables).
    act("edit", t("comp_edit"), () => this.openEditor(it), it.source !== "internal");
    row.appendChild(acts);
    return row;
  }

  /** Ligne méta d'un item (pid/uptime/restarts/health/exit/description)
   *  — extraite pour être rafraîchie en place au poll. */
  private metaText(it: ComponentItem): string {
    const st = it.status;
    const meta: string[] = [];
    if (st.pid) meta.push(`pid ${st.pid}`);
    if (it.kind === "process" && (st.uptime_s ?? 0) > 0)
      meta.push(`↑${fmtUp(st.uptime_s!)}`);
    if ((st.restarts ?? 0) > 0)
      meta.push(`↻${st.restarts} ${t("comp_restarts")}`);
    if (st.config_required?.length)
      meta.push(`⚙ ${t("comp_cfg_missing", { l: st.config_required.join(", ") })}`);
    if (st.health)
      meta.push(
        st.health === "ok" ? "♥ ok"
        : st.health === "outdated" ? `♥ ${t("comp_health_outdated")}`
        : "♥ fail",
      );
    if (st.last_exit !== undefined && st.last_exit !== null)
      meta.push(`exit ${st.last_exit}`);
    // Version sondée de l'outil managé (status.version — process,
    // external ou tool) + badge « mise à jour requise » sous
    // min_version déclaré (status.version_outdated/version_min).
    if (st.version) meta.push(st.version);
    if (st.version_outdated)
      meta.push(`⚠ ${t("comp_outdated", { v: st.version_min ?? "?" })}`);
    if (it.description) meta.push(it.description);
    return meta.join(" · ");
  }

  /** Fingerprint structurel d'un item — tout sauf l'uptime qui tick à la
   *  seconde : identique → le poll ne rebâtit pas le DOM (un clic qui
   *  chevauche le re-render serait avalé, hover/focus perdus ; le même
   *  problème était résolu dans map-view par sa signature « pas de
   *  changement → pas de re-render »). */
  private fpOf(it: ComponentItem): string {
    const st = it.status;
    return [
      it.name, it.kind, st.state, st.pid, st.restarts, st.health,
      st.last_exit, st.version, st.version_min, st.version_outdated,
      it.autostart, it.source, it.description, it.view,
      st.config_required?.join(","),
    ].join("|");
  }

  private renderList() {
    const el = $("comp-list");
    if (!this.items.length) {
      this.renderedFp = "";
      el.innerHTML = "";
      el.appendChild(
        this.empty(!this.listLoaded ? t("loading") : t("comp_none")),
      );
      return;
    }
    const fp = this.items.map((it) => this.fpOf(it)).join(";");
    if (fp === this.renderedFp && el.childElementCount === this.items.length) {
      // Même structure : seule la ligne méta (uptime) change — update en
      // place, les lignes gardent hover/focus/handlers. Indexation par
      // ligne (une row sans méta ne décale pas la suivante).
      this.items.forEach((it, i) => {
        const m = el.children[i]?.querySelector<HTMLElement>(".comp-meta");
        if (m) m.textContent = this.metaText(it);
      });
      return;
    }
    this.renderedFp = fp;
    el.innerHTML = "";
    for (const it of this.items) el.appendChild(this.compRow(it));
  }

  // -------------------------------------------------------------------
  // Onglet Catalogue
  // -------------------------------------------------------------------

  private async refreshCatalog(refresh = false) {
    if (this.mode === "catalog") this.renderCatalog();
    try {
      this.catalog = await call<CatalogEntry[]>("component_catalog", {
        refresh,
        conn: this.connParam(),
      });
      this.catalogLoaded = true;
      if (this.mode === "catalog") {
        dlgErr("comp-err", null);
        this.renderCatalog();
      }
    } catch (e) {
      // Jamais appelée par le poll → chaque échec est une action
      // utilisateur (onglet, bouton refresh, retour post-install) :
      // toujours visible.
      dlgErr("comp-err", String(e));
    }
  }

  private renderCatalog() {
    const el = $("comp-catalog");
    el.innerHTML = "";
    if (!this.catalog.length) {
      el.appendChild(
        this.empty(!this.catalogLoaded ? t("loading") : t("comp_cat_none")),
      );
      return;
    }
    for (const e of this.catalog) {
      const card = document.createElement("div");
      card.className = "cat-card";
      const badges: string[] = [];
      if (e.installed)
        badges.push(
          `<span class="cat-inst">${icon("check")} ${esc(
            e.version ? t("comp_cat_installed", { v: e.version }) : t("comp_cat_inst"),
          )}</span>`,
        );
      else badges.push(`<span class="cat-abs">${esc(t("agent_missing"))}</span>`);
      if (e.latest_version)
        badges.push(
          `<span class="cat-latest">${esc(t("comp_cat_latest", { v: e.latest_version }))}</span>`,
        );
      card.innerHTML =
        `<div class="cat-l1"><span class="comp-name">${esc(e.name)}</span>${badges.join("")}</div>` +
        `<div class="cat-desc">${esc(e.description)}</div>` +
        `<div class="cat-acts"></div>`;
      if (e.latest_notes) card.title = e.latest_notes;
      const acts = card.querySelector<HTMLElement>(".cat-acts")!;
      if (e.homepage) {
        const hp = document.createElement("button");
        hp.type = "button";
        hp.className = "cat-link";
        hp.innerHTML = `${icon("external")} ${esc(e.repo ?? e.homepage)}`;
        hp.title = e.homepage;
        hp.onclick = () => shellOpen(e.homepage!);
        acts.appendChild(hp);
      }
      const go = document.createElement("button");
      go.type = "button";
      go.className = "cat-go";
      go.textContent = t("comp_cat_integrate");
      go.onclick = () => void this.integrate(e, go);
      acts.appendChild(go);
      el.appendChild(card);
    }
  }

  /** « Intégrer » : si l'entrée déclare des params, on les collecte
   *  d'abord dans le dialogue schema-driven (annulé → rien n'est posé) ;
   *  sans params, comportement historique — recette puis éditeur
   *  prérempli de la spec posée. */
  private async integrate(e: CatalogEntry, btn: HTMLButtonElement) {
    btn.disabled = true;
    try {
      let params: Record<string, string> | undefined;
      if (e.params?.length) {
        const vals = await this.paramsDialog(
          t("comp_params_install", { name: e.name }),
          e.params,
          {},
        );
        if (vals === null) return;
        params = vals;
      }
      const r = await call<{
        notes?: string[];
        config_required?: string[];
      }>("component_install", {
        name: e.name,
        params,
        conn: this.connParam(),
      });
      for (const n of r.notes ?? []) notify("info", n);
      if (r.config_required?.length)
        notify("warn", t("comp_cfg_missing", { l: r.config_required.join(", ") }));
      else notify("success", t("comp_cat_done", { name: e.name }));
      if (!params) {
        const info = await call<ComponentItem>("component_info", {
          name: e.name,
          conn: this.connParam(),
        }).catch(() => null);
        if (info) this.openEditor(info);
        else this.openEditor(undefined, e.suggested_spec);
      }
      void this.refreshCatalog();
      this.refresh();
    } catch (err) {
      dlgErr("comp-err", String(err));
    } finally {
      btn.disabled = false;
    }
  }

  /** « Configurer » : vue pleine page des params déclarés (onglets par
   *  groupe, filtre, récapitulatif avant `component_set_params`). */
  private configure(it: ComponentItem) {
    if (!(it.spec.params_schema?.length ?? 0)) return;
    this.paramsCurrent = it.spec.params ?? {};
    this.setMode("params");
    void this.params.open(it);
  }

  /** Ouvre directement la configuration d'un composant (menu Options,
   *  rail des Paramètres → Orchestrateur). */
  async openParamsOf(name: string) {
    this.open();
    if (!this.isOpen()) return;
    await this.refreshList(true);
    const it = this.items.find((x) => x.name === name);
    if (it && (it.spec.params_schema?.length ?? 0) > 0) this.configure(it);
    else notify("warn", t("comp_params_missing", { name }));
  }

  /** Fermeture demandée par l'utilisateur (✕, Échap, back, titlebar) :
   *  des paramètres modifiés non appliqués → Appliquer / Ignorer / Rester. */
  async requestClose(): Promise<boolean> {
    if (this.mode === "params" && !(await this.params.confirmLeave())) return false;
    this.close();
    return true;
  }

  /** Caps `list_agents` pour les dropdowns du widget worker_pool —
   *  cache par connexion, rechargé à chaque ouverture du formulaire
   *  (les profils bougent : install d'adaptateur, rescan). */
  private async loadPoolAgents() {
    const conn = this.connKey();
    if (this.poolAgentsConn === conn && this.poolAgents.size) return;
    try {
      const c = apiConn(conn);
      const list = await call<AgentOpt[]>(
        "list_agents",
        c ? { conn: c } : {},
      );
      this.poolAgents = new Map(list.map((a) => [a.id, a]));
      this.poolAgentsConn = conn;
    } catch {
      // Caps injoignables : les champs restent en texte libre —
      // la validation vocabulaire reste côté démon (warn, pas refus).
      this.poolAgents = new Map();
      this.poolAgentsConn = conn;
    }
  }

  /** Options d'une colonne du widget pool pour un profil donné :
   *  modèles = caps.models + extra_models + catalogue du profil
   *  (model_catalog — variantes hors annonce ACP, ex. devin) ;
   *  modes = caps.modes ; effort = config_option
   *  « thought_level »/« effort » si annoncée.
   *  Vide → champ texte libre (valeur hors vocabulaire tolérée). */
  private poolOpts(
    profile: string,
    field: "model" | "effort" | "mode",
  ): { v: string; n: string }[] {
    const a = this.poolAgents.get(profile);
    if (field === "model") {
      const out = new Map<string, string>();
      for (const m of a?.caps?.models ?? []) out.set(m.value, m.name || m.value);
      for (const e of a?.extra_models ?? [])
        if (!out.has(e.value)) out.set(e.value, e.label || e.value);
      // Variantes catalogue-only (catalogue_cmd côté démon) — badgées
      // comme dans le sélecteur de session ; appliquées au spawn via le
      // side-channel/env du profil, pas par set_config_option. Union même
      // sans caps moissonnées : un profil jamais spawné garde son
      // catalogue.
      const cat = catalogFor(apiConn(this.connKey()) ?? "", profile);
      if (cat)
        for (const e of Object.values(cat))
          if (!out.has(e.value))
            out.set(e.value, `${e.label} · ${t("catalog_badge")}`);
      return [...out].map(([v, n]) => ({ v, n }));
    }
    if (!a?.caps) return [];
    if (field === "mode")
      return (a.caps.modes ?? []).map((o) => ({ v: o.value, n: o.name || o.value }));
    const co = (a.caps.config_options ?? []).find((o) =>
      /thought|effort|thinking/i.test(o.id),
    );
    return (co?.options ?? []).map((o) => ({ v: o.value, n: o.name || o.value }));
  }

  /** Widget « worker_pool » : une ligne par entrée
   *  `{profile,count,model,effort,mode}` — profil en select (choices
   *  = profils déclarés), count numérique, model/effort/mode en
   *  input + datalist alimentée par les caps DU PROFIL de la ligne.
   *  La valeur sérialisée vit dans `hidden` (JSON compact — même
   *  forme que le textarea json historique). Renvoie le wrap +
   *  l'input porteur de `data-pkey`. */
  private poolEditor(
    cur: string,
    ps: ParamSpec,
    current: Record<string, string>,
  ): { wrap: HTMLElement; hidden: HTMLInputElement } {
    interface Row {
      profile: HTMLSelectElement;
      count: HTMLInputElement;
      model: HTMLInputElement;
      effort: HTMLInputElement;
      mode: HTMLInputElement;
      dls: HTMLDataListElement[];
    }
    const wrap = document.createElement("div");
    wrap.className = "wp";
    const rows = document.createElement("div");
    rows.className = "wp-rows";
    const hidden = document.createElement("input");
    hidden.type = "hidden";
    const profiles = ps.choices ?? [];
    const maxWorking = parseInt(current["max_working"] ?? "", 10);
    const totalEl = document.createElement("div");
    totalEl.className = "wp-total";
    let uid = 0;

    const sync = () => {
      const out: Record<string, string | number>[] = [];
      let total = 0;
      for (const r of rowEls) {
        const count = Math.max(0, parseInt(r.count.value, 10) || 0);
        total += count;
        out.push({
          profile: r.profile.value,
          count,
          model: r.model.value.trim(),
          effort: r.effort.value.trim(),
          mode: r.mode.value.trim(),
        });
      }
      hidden.value = JSON.stringify(out);
      // Le porteur masqué n'émet rien seul : la vue pleine page suit
      // les modifications (pastille « modifié », récapitulatif).
      hidden.dispatchEvent(new Event("change"));
      totalEl.textContent =
        maxWorking > 0
          ? t("wp_total_active", { n: total, m: maxWorking })
          : t("wp_total", { n: total });
    };

    const fillDatalist = (r: Row) => {
      const profile = r.profile.value;
      // Réchauffe le catalogue du profil (RPC model_catalog, cache
      // client 10 min) — le re-fill rejoue poolOpts quand les variantes
      // arrivent ; profil sans catalogue_cmd → rien (datalist caps).
      if (profile)
        ensureCatalog(apiConn(this.connKey()) ?? "", profile, () =>
          fillDatalist(r),
        );
      (["model", "effort", "mode"] as const).forEach((f, i) => {
        const dl = r.dls[i];
        dl.innerHTML = "";
        for (const o of this.poolOpts(profile, f)) {
          const opt = document.createElement("option");
          opt.value = o.v;
          opt.label = o.n;
          dl.appendChild(opt);
        }
      });
      sync();
    };

    const rowEls: Row[] = [];
    const addRow = (e?: {
      profile?: string; count?: number; model?: string;
      effort?: string; mode?: string;
    }) => {
      const tr = document.createElement("div");
      tr.className = "wp-row";
      const mk = (cls: string) => {
        const i = document.createElement("input");
        i.className = cls;
        return i;
      };
      const profile = document.createElement("select");
      profile.className = "wp-profile";
      const cur0 = e?.profile ?? "";
      const known = profiles.length ? profiles : [cur0].filter(Boolean);
      for (const p of known) {
        const o = document.createElement("option");
        o.value = p;
        o.textContent = p;
        profile.appendChild(o);
      }
      if (cur0 && !known.includes(cur0)) {
        const o = document.createElement("option");
        o.value = cur0;
        o.textContent = `${cur0} (?)`;
        profile.appendChild(o);
      }
      profile.value = cur0 || known[0] || "";
      const count = mk("wp-count");
      count.type = "number";
      count.min = "0";
      count.step = "1";
      count.value = String(e?.count ?? 1);
      const dlIds = ["model", "effort", "mode"].map((f) => `wp-dl-${uid++}-${f}`);
      const dls = dlIds.map((id) => {
        const dl = document.createElement("datalist");
        dl.id = id;
        document.body.appendChild(dl);
        return dl;
      });
      const fields = (["model", "effort", "mode"] as const).map((f, i) => {
        const inp = mk(`wp-${f}`);
        inp.placeholder = f === "model" ? t("wp_ph_model")
          : f === "effort" ? t("wp_ph_effort") : t("wp_ph_mode");
        inp.setAttribute("list", dlIds[i]);
        inp.value = (e?.[f] as string | undefined) ?? "";
        inp.addEventListener("input", sync);
        return inp;
      });
      const del = document.createElement("button");
      del.type = "button";
      del.className = "wp-del";
      del.title = t("wp_remove");
      del.textContent = "✕";
      del.onclick = () => {
        tr.remove();
        dls.forEach((d) => d.remove());
        const i = rowEls.indexOf(r);
        if (i >= 0) rowEls.splice(i, 1);
        sync();
      };
      profile.onchange = () => fillDatalist(r);
      count.addEventListener("input", sync);
      tr.append(profile, count, ...fields, del);
      rows.appendChild(tr);
      const r: Row = { profile, count, model: fields[0], effort: fields[1], mode: fields[2], dls };
      rowEls.push(r);
      fillDatalist(r);
    };

    // Valeur courante : JSON `[{profile,count,model,effort,mode}]`
    // (params_values sérialise worker_pool ainsi) — parsage tolérant,
    // forme inattendue → une ligne vide pré-remplie.
    let entries: Record<string, unknown>[] = [];
    try {
      const j = JSON.parse(cur || "[]");
      if (Array.isArray(j)) entries = j.filter((x) => x && typeof x === "object");
    } catch { /* chaîne non-JSON (saisie shorthand) → éditeur vide */ }
    // Pool vide = mode legacy (worker_agent + max_workers) : aucune
    // ligne pré-créée — sinon un OK sans toucher injecterait une
    // entrée {profile,count:1} qui n'existait pas.
    for (const e of entries)
      addRow({
          profile: String(e.profile ?? ""),
          count: typeof e.count === "number" ? e.count : parseInt(String(e.count ?? "1"), 10) || 1,
          model: String(e.model ?? ""),
          effort: String(e.effort ?? ""),
          mode: String(e.mode ?? ""),
        });

    const add = document.createElement("button");
    add.type = "button";
    add.className = "wp-add";
    add.textContent = `+ ${t("wp_add")}`;
    add.onclick = () => addRow();
    wrap.append(rows, add, totalEl);
    sync();
    return { wrap, hidden };
  }

  /** Dialogue générique piloté par un `ParamSpec[]` (intégration depuis
   *  le catalogue — petits schémas ; « Configurer » passe par la vue
   *  pleine page). Les `secret` viennent masqués (« •••••• ») : champ
   *  laissé vide = valeur conservée (clé non envoyée). Renvoie les
   *  saisies ({clé: valeur}, vide = retrait) ou null si annulé. */
  private async paramsDialog(
    title: string,
    schema: ParamSpec[],
    current: Record<string, string>,
  ): Promise<Record<string, string> | null> {
    const dlg = $("dlg-cparams") as HTMLDialogElement;
    const fields = $("cp-fields");
    fields.innerHTML = "";
    $("cp-title").textContent = title;
    dlgErr("cp-err", null);
    if (schema.some((p) => p.key === "worker_pool")) await this.loadPoolAgents();
    this.paramsCurrent = current;
    const inputs: ParamField[] = [];
    let lastGroup: string | null = null;
    for (const ps of schema) {
      // Section : titre quand le groupe change (le démon trie le
      // schéma par section — groupes contigus garantis).
      const grp = ps.group ?? null;
      if (grp !== lastGroup) {
        lastGroup = grp;
        if (grp) {
          const h = document.createElement("div");
          h.className = "cp-sec";
          h.textContent = secLabel(grp);
          fields.appendChild(h);
        }
      }
      const f = new ParamField(ps, current, {
        pool: (cur, p) => this.poolEditor(cur, p, current),
      });
      const lab = document.createElement("label");
      const head = document.createElement("span");
      head.textContent = ps.label + (ps.required ? " *" : "");
      if (ps.kind === "bool") {
        lab.classList.add("chk-row");
        lab.append(f.control, head);
      } else {
        lab.append(head, f.control);
      }
      if (ps.description) {
        const d = document.createElement("small");
        d.className = "cp-desc";
        d.textContent = ps.description;
        lab.appendChild(d);
      }
      fields.appendChild(lab);
      inputs.push(f);
    }
    return await new Promise((resolve) => {
      let decided = false;
      const close = (v: Record<string, string> | null) => {
        if (decided) return;
        decided = true;
        dlg.close();
        resolve(v);
      };
      ($("cp-cancel") as HTMLButtonElement).onclick = () => close(null);
      ($("cp-ok") as HTMLButtonElement).onclick = (ev) => {
        ev.preventDefault();
        const out: Record<string, string> = {};
        for (const f of inputs) {
          const err = validateParam(f, f.ps.label);
          if (err) {
            dlgErr("cp-err", err);
            return;
          }
          const v = f.read();
          // secret posé, non retapé → clé absente = conserver la valeur.
          if (!v && f.ps.secret && f.has) continue;
          out[f.ps.key] = v;
        }
        close(out);
      };
      dlg.onclose = () => close(null);
      dlg.showModal();
    });
  }

  // -------------------------------------------------------------------
  // Éditeur de spec
  // -------------------------------------------------------------------

  /** Ouvre le formulaire : édition d'un item existant, nouvelle spec
   *  (item absent) ou préremplissage catalogue (prefill). Un item
   *  adossé au catalogue (`item.catalog` du démon) ouvre l'éditeur
   *  borné — params déclarés + autostart/description, spec interne en
   *  lecture seule ; tout le reste (custom, préremplissage) garde
   *  l'éditeur de spec complet. */
  private openEditor(item?: ComponentItem, prefill?: ComponentSpec) {
    const spec = prefill ?? item?.spec ?? defaultSpec();
    this.editName = item?.name ?? "";
    // Composant adossé au catalogue : flag `item.catalog` (démon ≥0.10,
    // nom couvert par une entrée) ou `spec.catalog` (nom d'entrée posé
    // à l'install par le write-path params) — l'un ou l'autre suffit.
    // item requis : un prefill portant spec.catalog sans item garderait
    // l'éditeur legacy (fillBoundEditor lit item.status).
    this.editBound = item != null && (item.catalog === true || !!spec.catalog);
    this.editSpec = spec;
    $("comp-ed-title").textContent = t(this.editName ? "comp_edit_h" : "comp_new_h");
    ($("ce-name") as HTMLInputElement).value = spec.name;
    // Le nom est la clé de la spec — immuable en édition (renommer =
    // supprimer puis recréer, ce que ComponentRemove permet déjà).
    ($("ce-name") as HTMLInputElement).disabled = !!this.editName;
    ($("ce-desc") as HTMLInputElement).value = spec.description ?? "";
    ($("ce-autostart") as HTMLInputElement).checked = spec.autostart;
    ($("ce-del") as HTMLButtonElement).hidden = !this.editName;
    if (this.editBound) this.fillBoundEditor(item!);
    else this.fillSpecEditor(spec);
    this.setMode("edit");
  }

  /** Éditeur de spec complet — composants customs (sans entrée
   *  catalogue : le formulaire legacy reste la surface d'édition,
   *  choix documenté) et préremplissage d'une suggested_spec quand
   *  ComponentInfo n'a pas pu être relu après install. */
  private fillSpecEditor(spec: ComponentSpec) {
    $("ce-spec-fields").hidden = false;
    $("ce-params").hidden = true;
    $("ce-internals").hidden = true;
    $("ce-config-warn").hidden = true;
    ($("ce-kind") as HTMLSelectElement).value = spec.kind === "internal"
      ? "process"
      : spec.kind;
    ($("ce-cmd") as HTMLTextAreaElement).value = spec.command.join("\n");
    ($("ce-env") as HTMLTextAreaElement).value = Object.entries(spec.env)
      .map(([k, v]) => `${k}=${v}`)
      .join("\n");
    ($("ce-cwd") as HTMLInputElement).value = spec.cwd ?? "";
    ($("ce-restart") as HTMLSelectElement).value = spec.restart;
    ($("ce-rsec") as HTMLInputElement).value = String(spec.restart_sec);
    const hc = spec.healthcheck;
    ($("ce-hc") as HTMLSelectElement).value = hc.kind;
    ($("ce-hc-addr") as HTMLInputElement).value =
      hc.kind === "tcp" ? hc.addr : "";
    ($("ce-hc-url") as HTMLInputElement).value =
      hc.kind === "http" ? hc.url : "";
    ($("ce-hc-argv") as HTMLTextAreaElement).value =
      hc.kind === "command" ? hc.argv.join("\n") : "";
    this.syncEditFields();
  }

  /** Éditeur borné d'un composant catalogue : seuls les params
   *  déclarés (`spec.params_schema`), autostart (process) et
   *  description sont mutables ; la spec interne est reléguée dans le
   *  panneau « internals » en lecture seule (choix : visible pour le
   *  debug — le démon la scelle de toute façon via upsert_edit). */
  private fillBoundEditor(item: ComponentItem) {
    const spec = item.spec;
    $("ce-spec-fields").hidden = true;
    // autostart n'a de sens que pour un `process` managé.
    document.querySelector<HTMLElement>(".ce-f-autostart")!.style.display =
      spec.kind === "process" ? "" : "none";
    const missing = item.status.config_required ?? [];
    const warn = $("ce-config-warn");
    warn.hidden = !missing.length;
    warn.textContent = missing.length
      ? t("comp_cfg_missing", { l: missing.join(", ") })
      : "";
    this.renderParams(spec);
    $("ce-internals").hidden = false;
    $("ce-internals-json").textContent = JSON.stringify(
      {
        kind: spec.kind,
        command: spec.command,
        env: spec.env,
        cwd: spec.cwd,
        restart: spec.restart,
        restart_sec: spec.restart_sec,
        healthcheck: spec.healthcheck,
        queries: spec.queries ?? {},
      },
      null,
      2,
    );
  }

  /** Rend un champ par param déclaré : string/path/duration → texte,
   *  int → nombre, bool → case, enum → select `choices`, list →
   *  textarea (une entrée par ligne — CSV sur le fil), json → textarea
   *  JSON (pretty-printé, re-compacté à la collecte), secret →
   *  password jamais prérempli (vide = conserver la valeur masquée).
   *  `data-pkey` relie chaque input à sa déclaration pour
   *  collectParams. `params`/`default` sont en forme chaîne — castés
   *  selon kind. */
  private renderParams(spec: ComponentSpec) {
    const zone = $("ce-params");
    zone.innerHTML = "";
    zone.hidden = false;
    const note = document.createElement("div");
    note.className = "ce-bound-note";
    note.textContent = t("comp_bound_note");
    zone.appendChild(note);
    for (const p of spec.params_schema ?? []) {
      const cur = spec.params?.[p.key] ?? p.default ?? "";
      const label = document.createElement("label");
      label.className = p.kind === "bool" ? "chk-row" : "ce-p";
      if (p.description) label.title = p.description;
      const span = document.createElement("span");
      span.textContent = (p.label || p.key) + (p.required ? " *" : "");
      let input: HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement;
      switch (p.kind) {
        case "bool": {
          const b = document.createElement("input");
          b.type = "checkbox";
          b.checked = paramBool(cur);
          input = b;
          label.append(b, span);
          break;
        }
        case "enum": {
          const sel = document.createElement("select");
          const set = !!(spec.params?.[p.key] ?? "");
          // « — » offert : non requis, ou requis encore non positionné
          // sans défaut (l'utilisateur doit trancher explicitement).
          if (!p.required || (!set && !p.default)) {
            const opt = document.createElement("option");
            opt.value = "";
            opt.textContent = "—";
            sel.appendChild(opt);
          }
          (p.choices ?? []).forEach((c, i) => {
            const opt = document.createElement("option");
            opt.value = c;
            opt.textContent = p.choice_labels?.[i] || c;
            sel.appendChild(opt);
          });
          // Valeur stockée hors `choices` (schéma évolué) : la voir
          // plutôt que selectedIndex=-1 → "" (retrait silencieux).
          if (cur && !(p.choices ?? []).includes(cur)) {
            const opt = document.createElement("option");
            opt.value = cur;
            opt.textContent = `${cur} (?)`;
            sel.appendChild(opt);
          }
          sel.value = cur;
          input = sel;
          label.append(span, sel);
          break;
        }
        case "list": {
          const ta = document.createElement("textarea");
          ta.rows = 3;
          ta.spellcheck = false;
          ta.value = paramList(cur).join("\n");
          input = ta;
          label.append(span, ta);
          break;
        }
        case "json": {
          const ta = document.createElement("textarea");
          ta.rows = 5;
          ta.spellcheck = false;
          ta.value = paramJson(cur);
          input = ta;
          label.append(span, ta);
          break;
        }
        default: {
          const inp = document.createElement("input");
          inp.autocomplete = "off";
          inp.spellcheck = false;
          if (p.secret) {
            inp.type = "password";
            inp.placeholder = "••••••";
          } else {
            inp.type = p.kind === "int" ? "number" : "text";
            if (p.kind === "int") inp.step = "1";
            inp.value = cur;
            if (p.default) inp.placeholder = p.default;
          }
          input = inp;
          label.append(span, inp);
        }
      }
      input.dataset.pkey = p.key;
      zone.appendChild(label);
    }
  }

  /** Form params → patch {clé: chaîne} des seules clés modifiées
   *  (`component_set_params` : valeurs en forme chaîne — "" retire la
   *  clé ; un champ secret vide conserve la valeur masquée, jamais
   *  renvoyée). Contrôles alignés sur la déclaration (mêmes règles que
   *  le dialogue params d'install) : required sans défaut non vide,
   *  int/duration/enum au format, regex `validation` (indicative — le
   *  démon valide en dernier ressort). */
  private collectParams(): Record<string, string> {
    const spec = this.editSpec!;
    const patch: Record<string, string> = {};
    for (const p of spec.params_schema ?? []) {
      const el = $("ce-params").querySelector<HTMLElement>(
        `[data-pkey="${p.key}"]`,
      );
      if (!el) continue;
      const cur = spec.params?.[p.key] ?? p.default ?? "";
      const need = () => {
        // default:"" = pas de défaut effectif — retirer la clé ferait
        // retomber le param required sur config_required.
        if (p.required && !p.default)
          throw new Error(t("comp_param_required", { k: p.label || p.key }));
      };
      switch (p.kind) {
        case "bool": {
          const v = (el as HTMLInputElement).checked;
          if (v !== paramBool(cur)) patch[p.key] = v ? "true" : "false";
          break;
        }
        case "enum": {
          const v = (el as HTMLSelectElement).value;
          if (v !== cur) {
            if (v && !p.open_enum && !(p.choices ?? []).includes(v))
              throw new Error(t("comp_param_badenum", { k: p.label || p.key }));
            patch[p.key] = v;
          }
          break;
        }
        case "list": {
          const v = (el as HTMLTextAreaElement).value
            .split("\n")
            .map((l) => l.trim())
            .filter(Boolean);
          if (!v.length) need();
          if (v.join(",") !== paramList(cur).join(","))
            patch[p.key] = v.join(",");
          break;
        }
        case "json": {
          const v = (el as HTMLTextAreaElement).value.trim();
          if (!v) {
            need();
            // Champ vidé = retrait, comme les autres kinds.
            if (cur) patch[p.key] = "";
            break;
          }
          // Canons comparés (clés triées) : la valeur déjà posée est
          // stockée en canon démon — une resaisie équivalente (ordre,
          // indentation) ne doit pas envoyer de patch.
          const canon = canonJson(v);
          if (!canon) {
            throw new Error(t("comp_param_badjson", { k: p.label || p.key }));
          }
          if (canon !== canonJson(cur ?? "")) patch[p.key] = canon;
          break;
        }
        default: {
          const inp = el as HTMLInputElement;
          if (p.secret) {
            if (inp.value !== "") patch[p.key] = inp.value;
            break;
          }
          const v = inp.value.trim();
          if (!v) need();
          if (v && p.kind === "int" && !/^-?\d+$/.test(v))
            throw new Error(t("comp_param_badint", { k: p.label || p.key }));
          if (
            v &&
            p.kind === "duration" &&
            !/^(\d+(ms|min|s|m|h|d|w)\s*)+$/.test(v)
          )
            throw new Error(t("comp_param_baddur", { k: p.label || p.key }));
          if (v && p.validation) {
            try {
              if (!new RegExp(p.validation).test(v))
                throw new Error(t("comp_param_badfmt", { k: p.label || p.key }));
            } catch (e) {
              if (!(e instanceof SyntaxError)) throw e; // regex Rust non JS — le démon tranchera
            }
          }
          if (v !== cur) patch[p.key] = v;
        }
      }
    }
    return patch;
  }

  /** Champs conditionnels selon kind : cycle de vie (autostart/restart/
   *  cwd/env) réservé aux `process` managés ; `command` = argv pour
   *  process, sonde --version pour tool, inutile pour external ;
   *  healthcheck=process exige un pid managé (refus démon sinon). */
  private syncEditFields() {
    const kind = ($("ce-kind") as HTMLSelectElement).value as ComponentKind;
    const proc = kind === "process";
    for (const cls of ["cmd", "env", "cwd", "autostart", "restart", "rsec"]) {
      const el = document.querySelector<HTMLElement>(`.ce-f-${cls}`)!;
      el.style.display =
        cls === "cmd" ? (kind === "external" ? "none" : "")
        : proc ? ""
        : "none";
    }
    const hcSel = $("ce-hc") as HTMLSelectElement;
    const hcProc = hcSel.querySelector<HTMLOptionElement>('option[value="process"]')!;
    hcProc.disabled = !proc;
    if (!proc && hcSel.value === "process") hcSel.value = "none";
    const hc = hcSel.value;
    for (const cls of ["hc-tcp", "hc-http", "hc-cmd"]) {
      const want =
        cls === "hc-tcp" ? "tcp" : cls === "hc-http" ? "http" : "command";
      document.querySelector<HTMLElement>(`.ce-f-${cls}`)!.style.display =
        hc === want ? "" : "none";
    }
  }

  /** Form → ComponentSpec — validation minimale alignée sur les gardes
   *  du démon (nom [a-z0-9-]+, commande exigée pour process/tool, env
   *  KEY=VALUE) pour échouer avant l'envoi plutôt que sur erreur RPC.
   *  Les champs masqués selon le kind sont normalisés, pas lus : sinon
   *  un process re-basculé en external persisterait un argv/autostart
   *  caducs dans components.toml. */
  private collectSpec(): ComponentSpec {
    const name = ($("ce-name") as HTMLInputElement).value.trim();
    if (!/^[a-z0-9-]+$/.test(name)) throw new Error(t("comp_name_invalid"));
    const kind = ($("ce-kind") as HTMLSelectElement).value as ComponentKind;
    const proc = kind === "process";
    const command = kind === "external" ? [] : LINES("ce-cmd");
    if (kind !== "external" && !command.length)
      throw new Error(t("comp_cmd_required", { kind }));
    const env: Record<string, string> = {};
    if (proc) {
      for (const l of LINES("ce-env")) {
        const i = l.indexOf("=");
        if (i <= 0) throw new Error(t("comp_env_invalid", { l }));
        env[l.slice(0, i).trim()] = l.slice(i + 1);
      }
    }
    let healthcheck: ComponentHealthcheck = { kind: "none" };
    const hcKind = ($("ce-hc") as HTMLSelectElement).value;
    if (hcKind === "process") healthcheck = { kind: "process" };
    else if (hcKind === "tcp") {
      const addr = ($("ce-hc-addr") as HTMLInputElement).value.trim();
      if (!addr) throw new Error(t("comp_hc_required", { f: "addr" }));
      healthcheck = { kind: "tcp", addr };
    } else if (hcKind === "http") {
      const url = ($("ce-hc-url") as HTMLInputElement).value.trim();
      if (!url) throw new Error(t("comp_hc_required", { f: "url" }));
      healthcheck = { kind: "http", url };
    } else if (hcKind === "command") {
      const argv = LINES("ce-hc-argv");
      if (!argv.length) throw new Error(t("comp_hc_required", { f: "argv" }));
      healthcheck = { kind: "command", argv };
    }
    return {
      name,
      kind,
      command,
      env,
      cwd: proc ? ($("ce-cwd") as HTMLInputElement).value.trim() || null : null,
      autostart: proc && ($("ce-autostart") as HTMLInputElement).checked,
      restart: proc
        ? (($("ce-restart") as HTMLSelectElement).value as ComponentSpec["restart"])
        : "on-failure",
      restart_sec: proc
        ? Math.max(0, Number(($("ce-rsec") as HTMLInputElement).value) || 0)
        : 5,
      healthcheck,
      description: ($("ce-desc") as HTMLInputElement).value.trim() || null,
      // queries/version_argv/min_version ne sont pas exposés au
      // formulaire — les valeurs déclarées (catalogue ou à la main)
      // sont reprises telles quelles, sinon l'upsert les effacerait
      // à chaque sauvegarde.
      queries: this.editSpec?.queries ?? {},
      version_argv: this.editSpec?.version_argv ?? null,
      min_version: this.editSpec?.min_version ?? null,
    };
  }

  private async saveSpec() {
    if (this.editBound) return this.saveBound();
    let spec: ComponentSpec;
    try {
      spec = this.collectSpec();
    } catch (e) {
      dlgErr("comp-err", String(e));
      return;
    }
    try {
      await call("component_upsert", {
        name: spec.name,
        spec,
        conn: this.connParam(),
      });
      notify("success", t("comp_saved", { name: spec.name }));
      this.setMode("list");
      await this.refreshList(true);
    } catch (e) {
      dlgErr("comp-err", String(e));
    }
  }

  /** Sauvegarde bornée (composant catalogue) : params via
   *  `component_set_params` — le démon re-rend la config et relance
   *  si besoin ; autostart/description via `component_upsert` (seuls
   *  champs utilisateurs acceptés par la spec scellée). Les champs
   *  params_* sont omis de l'upsert : ils ont leur write-path dédié,
   *  et renvoyer les valeurs lues écraserait les secrets masqués. */
  private async saveBound() {
    const spec = this.editSpec!;
    let patch: Record<string, string>;
    try {
      patch = this.collectParams();
    } catch (e) {
      dlgErr("comp-err", String(e));
      return;
    }
    const autostart = ($("ce-autostart") as HTMLInputElement).checked;
    const description = ($("ce-desc") as HTMLInputElement).value.trim() || null;
    try {
      if (Object.keys(patch).length) {
        await call("component_set_params", {
          name: spec.name,
          params: patch,
          conn: this.connParam(),
        });
      }
      if (
        autostart !== spec.autostart ||
        description !== (spec.description ?? null)
      ) {
        const upd = { ...spec, autostart, description };
        delete upd.params;
        delete upd.params_schema;
        delete upd.catalog;
        delete upd.render;
        await call("component_upsert", {
          name: spec.name,
          spec: upd,
          conn: this.connParam(),
        });
      }
      notify("success", t("comp_saved", { name: spec.name }));
      this.setMode("list");
      await this.refreshList(true);
    } catch (e) {
      dlgErr("comp-err", String(e));
    }
  }

  /** Danger-armed (pattern ctxmenu) : 1er tap arme le bouton et affiche
   *  la confirmation, 2e tap = ComponentRemove ; l'armement expire seul. */
  private disarmDel() {
    this.delArmed = false;
    if (this.delTimer) {
      clearTimeout(this.delTimer);
      this.delTimer = undefined;
    }
    const del = $("ce-del") as HTMLButtonElement;
    del.classList.remove("armed");
    del.textContent = t("delete");
  }

  private async removeSpec() {
    const del = $("ce-del") as HTMLButtonElement;
    if (!this.delArmed) {
      this.delArmed = true;
      del.classList.add("armed");
      del.textContent = t("ctx_confirm");
      this.delTimer = setTimeout(() => this.disarmDel(), 2500);
      return;
    }
    this.disarmDel();
    const name = this.editName;
    try {
      await call("component_remove", { name, conn: this.connParam() });
      notify("success", t("comp_removed", { name }));
      this.setMode("list");
      await this.refreshList(true);
    } catch (e) {
      dlgErr("comp-err", String(e));
    }
  }

  // -------------------------------------------------------------------
  // Viewer de logs (tail + auto-refresh par le poll)
  // -------------------------------------------------------------------

  private openLogs(name: string) {
    this.logsName = name;
    $("comp-logs-name").textContent = name;
    $("comp-logs").textContent = "";
    $("comp-logs-trunc").hidden = true;
    this.setMode("logs");
    void this.refreshLogs();
  }

  /** Tail du log managé : dernière page (offset absent → le démon taille
   *  de lui-même) recollée à chaque tick — suivi en bas seulement si
   *  l'utilisateur n'a pas remonté lire (sinon le tick arracherait le
   *  scroll toutes les 3 s). */
  private async refreshLogs() {
    try {
      const r = await call<{
        data: string;
        size: number;
        truncated: boolean;
      }>("component_logs", { name: this.logsName, conn: this.connParam() });
      const pre = $("comp-logs");
      const txt = new TextDecoder().decode(b64dec(r.data));
      const atBottom =
        pre.scrollTop + pre.clientHeight >= pre.scrollHeight - 24;
      pre.textContent = txt;
      $("comp-logs-trunc").textContent = r.truncated ? t("comp_logs_trunc") : "";
      $("comp-logs-trunc").hidden = !r.truncated;
      if (atBottom) pre.scrollTop = pre.scrollHeight;
    } catch (e) {
      // Pas de log (jamais lancé) ou refus : la vue affiche l'état vide,
      // l'erreur reste dans client.log — le poll réessaiera.
      $("comp-logs").textContent = t("comp_logs_none");
      $("comp-logs-trunc").hidden = true;
      logUi("debug", `component_logs ${this.logsName}: ${e}`);
    }
  }

  // -------------------------------------------------------------------

  init() {
    // Titlebar : drawer mobile éventuel d'abord — la vue s'ouvrirait
    // derrière le tiroir (même motif que btn-connect).
    $("tb-components").onclick = () => {
      nav.closeDrawer();
      if (this.isOpen()) void this.requestClose();
      else this.open();
    };
    $("comp-close").onclick = () => void this.requestClose();
    // Échap ferme le panneau — sauf si un dialogue est ouvert par-dessus.
    window.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && this.isOpen() && !document.querySelector("dialog[open]"))
        void this.requestClose();
    });
    // Back Android avec des paramètres modifiés : même question que le ✕.
    nav.addBackGuard((id) => {
      if (id !== "components" || this.mode !== "params" || !this.params.dirty()) return true;
      void this.requestClose();
      return false;
    });
    $("comp-tab-list").onclick = async () => {
      if (this.mode === "params" && !(await this.params.confirmLeave())) return;
      this.setMode("list");
      void this.refreshList(true);
    };
    $("comp-tab-cat").onclick = async () => {
      if (this.mode === "params" && !(await this.params.confirmLeave())) return;
      this.setMode("catalog");
      // Toujours re-fetch (refresh=false : détection locale rejouée,
      // sondes upstream cachées) — sinon le badge « installé » resterait
      // périmé après un ComponentInstall passé par l'éditeur.
      void this.refreshCatalog();
    };
    $("comp-add").onclick = () => this.openEditor();
    $("comp-refresh").onclick = () => this.refresh();
    ($("ce-kind") as HTMLSelectElement).onchange = () => this.syncEditFields();
    ($("ce-hc") as HTMLSelectElement).onchange = () => this.syncEditFields();
    $("ce-save").onclick = () => void this.saveSpec();
    $("ce-cancel").onclick = () => this.setMode("list");
    $("ce-del").onclick = () => void this.removeSpec();
    $("comp-logs-back").onclick = () => {
      this.setMode("list");
      void this.refreshList(true);
    };
  }
}
