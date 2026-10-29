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
import type {
  CatalogEntry,
  ComponentHealthcheck,
  ComponentItem,
  ComponentKind,
  ComponentSpec,
} from "../core/types";
import type { ConnectionManager } from "./connections";

/** Sous-vues du panneau : les deux onglets + éditeur + logs. */
type Mode = "list" | "catalog" | "edit" | "logs";

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
const KNOWN_ST = new Set(["starting", "running", "degraded", "stopped", "failed"]);

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
  private mode: Mode = "list";
  private items: ComponentItem[] = [];
  private catalog: CatalogEntry[] = [];
  /** Première réponse list/catalog reçue — avant ça les vides affichent
   *  « chargement » et non « aucun composant » (le flash serait trompeur). */
  private listLoaded = false;
  private catalogLoaded = false;
  private pollTimer: ReturnType<typeof setInterval> | undefined;
  private pollBusy = false;
  /** Composant dont le log tail est affiché (mode logs). */
  private logsName = "";
  /** Nom du composant en édition ("" = nouvelle spec). */
  private editName = "";
  /** Suppression armée dans l'éditeur (2e tap = ComponentRemove) —
   *  timer de désarmement, même pattern que le danger-armed du ctxmenu. */
  private delArmed = false;
  private delTimer: ReturnType<typeof setTimeout> | undefined;

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
    if ($("conn-view").hidden && $("settings-view").hidden) this.setChatInert(false);
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

    const meta: string[] = [];
    if (st.pid) meta.push(`pid ${st.pid}`);
    if (it.kind === "process" && (st.uptime_s ?? 0) > 0)
      meta.push(`↑${fmtUp(st.uptime_s!)}`);
    if ((st.restarts ?? 0) > 0)
      meta.push(`↻${st.restarts} ${t("comp_restarts")}`);
    if (st.health) meta.push(st.health === "ok" ? "♥ ok" : "♥ fail");
    if (st.last_exit !== undefined && st.last_exit !== null)
      meta.push(`exit ${st.last_exit}`);
    if (it.description) meta.push(it.description);
    if (meta.length) {
      const line2 = document.createElement("div");
      line2.className = "comp-meta";
      line2.textContent = meta.join(" · ");
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
    // internal : déclaré en code → ni édition ni suppression (le démon
    // refuse ; n'afficher que les actions réellement jouables).
    act("edit", t("comp_edit"), () => this.openEditor(it), it.source !== "internal");
    row.appendChild(acts);
    return row;
  }

  private renderList() {
    const el = $("comp-list");
    el.innerHTML = "";
    if (!this.items.length) {
      el.appendChild(
        this.empty(!this.listLoaded ? t("loading") : t("comp_none")),
      );
      return;
    }
    for (const it of this.items) el.appendChild(this.compRow(it));
  }

  // -------------------------------------------------------------------
  // Onglet Catalogue
  // -------------------------------------------------------------------

  private async refreshCatalog(refresh = false) {
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

  /** « Intégrer » : exécute la recette catalogue (ComponentInstall),
   *  puis ouvre l'éditeur prérempli de la spec effectivement posée
   *  (ComponentInfo — la suggested_spec matérialisée, chemins expansés ;
   *  repli sur la suggested_spec brute si la spec n'a pas été posée). */
  private async integrate(e: CatalogEntry, btn: HTMLButtonElement) {
    btn.disabled = true;
    try {
      const r = await call<{ notes?: string[] }>("component_install", {
        name: e.name,
        conn: this.connParam(),
      });
      for (const n of r.notes ?? []) notify("info", n);
      notify("success", t("comp_cat_done", { name: e.name }));
      const info = await call<ComponentItem>("component_info", {
        name: e.name,
        conn: this.connParam(),
      }).catch(() => null);
      if (info) this.openEditor(info);
      else this.openEditor(undefined, e.suggested_spec);
      void this.refreshCatalog();
    } catch (err) {
      dlgErr("comp-err", String(err));
    } finally {
      btn.disabled = false;
    }
  }

  // -------------------------------------------------------------------
  // Éditeur de spec
  // -------------------------------------------------------------------

  /** Ouvre le formulaire : édition d'un item existant, nouvelle spec
   *  (item absent) ou préremplissage catalogue (prefill). */
  private openEditor(item?: ComponentItem, prefill?: ComponentSpec) {
    const spec = prefill ?? item?.spec ?? defaultSpec();
    this.editName = item?.name ?? "";
    $("comp-ed-title").textContent = t(this.editName ? "comp_edit_h" : "comp_new_h");
    ($("ce-name") as HTMLInputElement).value = spec.name;
    // Le nom est la clé de la spec — immuable en édition (renommer =
    // supprimer puis recréer, ce que ComponentRemove permet déjà).
    ($("ce-name") as HTMLInputElement).disabled = !!this.editName;
    ($("ce-kind") as HTMLSelectElement).value = spec.kind === "internal"
      ? "process"
      : spec.kind;
    ($("ce-desc") as HTMLInputElement).value = spec.description ?? "";
    ($("ce-cmd") as HTMLTextAreaElement).value = spec.command.join("\n");
    ($("ce-env") as HTMLTextAreaElement).value = Object.entries(spec.env)
      .map(([k, v]) => `${k}=${v}`)
      .join("\n");
    ($("ce-cwd") as HTMLInputElement).value = spec.cwd ?? "";
    ($("ce-autostart") as HTMLInputElement).checked = spec.autostart;
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
    ($("ce-del") as HTMLButtonElement).hidden = !this.editName;
    this.syncEditFields();
    this.setMode("edit");
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
   *  KEY=VALUE) pour échouer avant l'envoi plutôt que sur erreur RPC. */
  private collectSpec(): ComponentSpec {
    const name = ($("ce-name") as HTMLInputElement).value.trim();
    if (!/^[a-z0-9-]+$/.test(name)) throw new Error(t("comp_name_invalid"));
    const kind = ($("ce-kind") as HTMLSelectElement).value as ComponentKind;
    const command = LINES("ce-cmd");
    if (kind !== "external" && !command.length)
      throw new Error(t("comp_cmd_required", { kind }));
    const env: Record<string, string> = {};
    for (const l of LINES("ce-env")) {
      const i = l.indexOf("=");
      if (i <= 0) throw new Error(t("comp_env_invalid", { l }));
      env[l.slice(0, i).trim()] = l.slice(i + 1);
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
      cwd: ($("ce-cwd") as HTMLInputElement).value.trim() || null,
      autostart: ($("ce-autostart") as HTMLInputElement).checked,
      restart: ($("ce-restart") as HTMLSelectElement)
        .value as ComponentSpec["restart"],
      restart_sec: Math.max(
        0,
        Number(($("ce-rsec") as HTMLInputElement).value) || 0,
      ),
      healthcheck,
      description: ($("ce-desc") as HTMLInputElement).value.trim() || null,
    };
  }

  private async saveSpec() {
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
   *  de lui-même) recollée à chaque tick — le suivi reste en bas. */
  private async refreshLogs() {
    try {
      const r = await call<{
        data: string;
        size: number;
        truncated: boolean;
      }>("component_logs", { name: this.logsName, conn: this.connParam() });
      const pre = $("comp-logs");
      const txt = new TextDecoder().decode(b64dec(r.data));
      pre.textContent = txt;
      $("comp-logs-trunc").textContent = r.truncated ? t("comp_logs_trunc") : "";
      $("comp-logs-trunc").hidden = !r.truncated;
      pre.scrollTop = pre.scrollHeight;
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
      this.toggle();
    };
    $("comp-close").onclick = () => this.close();
    // Échap ferme le panneau — sauf si un dialogue est ouvert par-dessus.
    window.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && this.isOpen() && !document.querySelector("dialog[open]"))
        this.close();
    });
    $("comp-tab-list").onclick = () => {
      this.setMode("list");
      void this.refreshList(true);
    };
    $("comp-tab-cat").onclick = () => {
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
