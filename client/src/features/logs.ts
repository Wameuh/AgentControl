// ---------------------------------------------------------------------------
// Panneau « Logs » live : overlay plein #chat (même mécanisme que
// settings-view/conn-view/components-view — inert + nav + Échap). Deux
// onglets — Client (client.log local : Tauri `log_read`, tampon UI en
// web) et Démon (`daemon_log_tail` du démon de la connexion choisie,
// offset + nom de fichier → suit la rotation quotidienne). Poll ~1,5 s
// tant que le panneau est ouvert et non en pause ; auto-scroll en bas
// sauf si l'utilisateur a remonté lire ; filtre niveau + pause.
// ---------------------------------------------------------------------------

import { invoke } from "../core/bridge";
import { isAndroid } from "../core/chrome";
import { $, esc, logUi } from "../core/dom";
import { logLineLevel, logRank } from "../core/logview";
import { call } from "../core/rpc";
import { app } from "../core/state";
import { t } from "../i18n";
import { icon } from "../ui/icons";
import { nav } from "../ui/nav";
import type { ConnectionManager } from "./connections";

const POLL_MS = 1_500;
const PAGE = 96 * 1024; // fenêtre par poll (bornes côté démon : 4-256 Kio)
/** Lignes brutes conservées par onglet — au-delà, purge du tiers ancien
 *  et re-rendu complet (évite une croissance DOM sans borne). */
const MAX_LINES = 6_000;
/** "default" = sentinelle de regroupement frontend — la passerelle
 *  attend conn absent/"" pour la connexion par défaut (cf. components). */
const apiConn = (c: string) => (c === "default" ? undefined : c);

type Tab = "client" | "daemon";

/** État du flux suivi d'un onglet : offset opaque du protocole + lignes
 *  brutes conservées + compte de lignes déjà reflétées dans le DOM. */
interface Tail {
  /** daemon : nom du fichier courant (rotation → reset côté démon). */
  file: string;
  /** null = prochain appel = dernière page (ouverture/onglet switch). */
  offset: number | null;
  lines: string[];
  rendered: number;
  /** Vrai après le 1er fetch réussi de CE flux — distingue « (vide) »
   *  de « pas encore chargé » (par onglet, pas global). */
  loaded: boolean;
}

const freshTail = (): Tail => ({
  file: "",
  offset: null,
  lines: [],
  rendered: 0,
  loaded: false,
});

export class LogsPanel {
  private conns!: ConnectionManager;
  /** L'autre overlay de #chat doit se fermer avant ouverture (injecté). */
  onOpening: () => void = () => {};

  private tab: Tab = "client";
  private paused = false;
  private pollTimer: ReturnType<typeof setInterval> | undefined;
  private pollBusy = false;
  private tails: Record<Tab, Tail> = { client: freshTail(), daemon: freshTail() };

  bind(conns: ConnectionManager) {
    this.conns = conns;
  }

  private isOpen() {
    return !($("logs-view") as HTMLElement).hidden;
  }

  /** Connexion pilotée : celle de la session affichée, sinon la conn liée
   *  (modèle multi-instances : une instance = une connexion). */
  private connKey(): string {
    return app.current?.conn ?? this.conns.knownConns()[0] ?? "default";
  }

  /** Capability gate : le démon connaît ReqKind::DaemonLogTail —
   *  l'onglet Démon est masqué sinon (vieux démon → erreur « req
   *  inconnue » à chaque poll). */
  daemonCapable(k: string): boolean {
    return this.conns.cap(k)?.daemon_logs === true;
  }

  private setChatInert(v: boolean) {
    for (const el of [...$("chat").children]) {
      if (el.id !== "logs-view") (el as HTMLElement).inert = v;
    }
  }

  open(tab?: Tab) {
    if (this.isOpen()) {
      if (tab) this.setTab(tab);
      return;
    }
    this.onOpening();
    const view = $("logs-view") as HTMLElement;
    view.hidden = false;
    this.setChatInert(true);
    view.focus({ preventScroll: true });
    nav.push("logs");
    this.paused = false;
    this.syncPauseBtn();
    this.fillConnSelect();
    this.setTab(tab ?? this.tab);
    this.pollTimer = setInterval(() => void this.tick(), POLL_MS);
  }

  close() {
    const was = this.isOpen();
    ($("logs-view") as HTMLElement).hidden = true;
    if (this.pollTimer) {
      clearInterval(this.pollTimer);
      this.pollTimer = undefined;
    }
    // L'inert ne tombe que si les autres overlays #chat sont fermés —
    // sinon #input serait ré-exposé derrière eux (frappes invisibles).
    if (
      $("conn-view").hidden &&
      $("settings-view").hidden &&
      $("map-view").hidden &&
      $("components-view").hidden
    )
      this.setChatInert(false);
    nav.close("logs");
    if (!was) return;
    // Réouverture = flux frais (la queue a pu défiler pendant la fermeture).
    this.tails = { client: freshTail(), daemon: freshTail() };
    if (!isAndroid()) ($("input") as HTMLTextAreaElement).focus();
  }

  toggle() {
    if (this.isOpen()) this.close();
    else this.open();
  }

  // -------------------------------------------------------------------
  // Onglets / contrôles
  // -------------------------------------------------------------------

  private setTab(t: Tab) {
    this.tab = t;
    $("logs-tab-client").classList.toggle("on", t === "client");
    $("logs-tab-daemon").classList.toggle("on", t === "daemon");
    // Le sélecteur de connexion ne sert qu'à l'onglet Démon.
    const sel = $("logs-conn") as HTMLSelectElement;
    sel.hidden = t !== "daemon";
    if (t === "daemon") {
      this.syncDaemonNote();
    } else {
      $("logs-daemon-note").hidden = true;
    }
    this.renderAll();
    void this.tick(); // premier fetch immédiat à l'ouverture de l'onglet
  }

  /** Sélecteur de connexion pour l'onglet Démon — une entrée par conn
   *  connue (bindée), marquée de son état live ; la conn pilotée est
   *  présélectionnée. */
  private fillConnSelect() {
    const sel = $("logs-conn") as HTMLSelectElement;
    const want = this.connKey();
    const keys = this.conns.knownConns();
    const list = keys.length ? keys : ["default"];
    sel.innerHTML = "";
    for (const k of list) {
      const o = document.createElement("option");
      o.value = k;
      const st = this.conns.state(k);
      o.textContent = st && st !== "ok" ? `${k} (${st})` : k;
      sel.appendChild(o);
    }
    sel.value = list.includes(want) ? want : list[0];
  }

  private selectedConn(): string {
    const v = ($("logs-conn") as HTMLSelectElement).value;
    return v || this.connKey();
  }

  /** Note « démon trop vieux » — ré-évaluée au switch d'onglet ET au
   *  changement de connexion (capability par conn, pas globale). */
  private syncDaemonNote() {
    $("logs-daemon-note").hidden = this.daemonCapable(this.selectedConn());
  }

  private syncPauseBtn() {
    const b = $("logs-pause") as HTMLButtonElement;
    b.innerHTML = icon(this.paused ? "play" : "stop");
    b.title = t(this.paused ? "logs_resume" : "logs_pause");
    $("logs-out").classList.toggle("paused", this.paused);
  }

  // -------------------------------------------------------------------
  // Poll
  // -------------------------------------------------------------------

  private async tick() {
    if (this.pollBusy || !this.isOpen() || this.paused) return;
    this.pollBusy = true;
    try {
      if (this.tab === "client") await this.fetchClient();
      else await this.fetchDaemon();
    } finally {
      this.pollBusy = false;
    }
  }

  /** client.log : Tauri `log_read` (offset octet, rotation gérée côté
   *  client-core) ; web : le même contrat servi par le tampon UI. */
  private async fetchClient() {
    const st = this.tails.client;
    try {
      const r = await invoke<{
        offset: number;
        data: string;
        size: number;
      }>("log_read", { offset: st.offset, limit: PAGE });
      st.offset = r.offset;
      this.appendData(st, r.data);
      if (this.tails[this.tab] === st) st.loaded = true;
    } catch (e) {
      logUi("debug", `log_read client: ${e}`);
    }
  }

  /** daemon.log : RPC `daemon_log_tail` du démon de la conn choisie —
   *  `file` suit la rotation quotidienne (nom différent → le démon
   *  taille le nouveau fichier). */
  private async fetchDaemon() {
    const st = this.tails.daemon;
    const conn = this.selectedConn();
    if (!this.daemonCapable(conn)) {
      // Vieux démon : pas de RPC — note affichée, pas de spam d'erreur.
      return;
    }
    try {
      const r = await call<{
        file: string;
        offset: number;
        data: string;
        size: number;
      }>("daemon_log_tail", {
        conn: apiConn(conn),
        file: st.file || undefined,
        offset: st.offset ?? 0,
        limit: PAGE,
      });
      st.file = r.file;
      st.offset = r.offset;
      this.appendData(st, r.data);
      if (this.tails[this.tab] === st) st.loaded = true;
    } catch (e) {
      logUi("debug", `daemon_log_tail ${conn}: ${e}`);
    }
  }

  /** Ajoute les lignes complètes de `data` au flux de l'onglet puis
   *  met à jour le DOM (append incrémental ou re-rendu si purge). */
  private appendData(st: Tail, data: string) {
    if (!data) return;
    const parts = data.split("\n");
    // data se termine toujours par '\n' (lignes complètes) → dernier
    // élément vide à écarter.
    if (parts[parts.length - 1] === "") parts.pop();
    if (!parts.length) return;
    const before = st.rendered;
    st.lines.push(...parts);
    // Flux inactif (changement d'onglet/conn pendant le fetch) : on
    // accumule mais on ne touche pas au DOM — setTab/renderAll
    // repeindra tout au retour sur cet onglet.
    const active = this.tails[this.tab] === st;
    if (!active) {
      st.rendered = st.lines.length;
      return;
    }
    // Purge du tiers ancien quand le tampon déborde — re-rendu complet.
    if (st.lines.length > MAX_LINES) {
      st.lines.splice(0, Math.floor(MAX_LINES / 3));
      this.renderAll();
      return;
    }
    // Premier rendu (before == 0) : le <pre> ne contient que le
    // placeholder « … »/« aucune ligne » — appendChild le laisserait
    // collé en tête du flux → re-rendu complet à la place.
    st.rendered = st.lines.length;
    if (before === 0) this.renderAll();
    else this.appendLines(st, before);
  }

  // -------------------------------------------------------------------
  // Rendu
  // -------------------------------------------------------------------

  private pre(): HTMLElement {
    return $("logs-out");
  }

  /** Re-rendu complet des lignes conservées, filtrées par le niveau
   *  minimum choisi. */
  private renderAll() {
    const st = this.tails[this.tab];
    const pre = this.pre();
    const min = logRank(($("logs-level") as HTMLSelectElement).value);
    const atBottom =
      pre.scrollTop + pre.clientHeight >= pre.scrollHeight - 24;
    const rows: string[] = [];
    for (const line of st.lines) {
      if (logRank(logLineLevel(line) ?? "info") < min) continue;
      rows.push(this.lineHtml(line));
    }
    pre.innerHTML = rows.length
      ? rows.join("\n")
      : `<span class="ll ll-meta">${esc(st.loaded ? t("logs_empty") : "…")}</span>`;
    st.rendered = st.lines.length;
    if (atBottom) pre.scrollTop = pre.scrollHeight;
  }

  /** Append incrémental : seules les lignes ajoutées depuis `from`
   *  (index source) sont rendues — le filtre niveau s'applique pareil. */
  private appendLines(st: Tail, from: number) {
    const pre = this.pre();
    const min = logRank(($("logs-level") as HTMLSelectElement).value);
    const atBottom =
      pre.scrollTop + pre.clientHeight >= pre.scrollHeight - 24;
    const frag = document.createDocumentFragment();
    for (let i = from; i < st.lines.length; i++) {
      const line = st.lines[i];
      const lvl = logLineLevel(line) ?? "info";
      if (logRank(lvl) < min) continue;
      const span = document.createElement("span");
      span.className = `ll ll-${lvl === "trace" ? "debug" : lvl}`;
      span.innerHTML = this.lineInner(line);
      frag.appendChild(span);
      frag.appendChild(document.createTextNode("\n"));
    }
    if (frag.childNodes.length) pre.appendChild(frag);
    if (atBottom) pre.scrollTop = pre.scrollHeight;
  }

  /** Une ligne de log → `<span class="ll ll-niveau">` coloré par
   *  sévérité, pour les deux formats (client.log `[iso] [LVL] f:N` et
   *  journal daemon `ISO  LVL cible: msg`). */
  private lineHtml(line: string): string {
    const lvl = logLineLevel(line) ?? "info";
    const cls = lvl === "trace" ? "debug" : lvl;
    return `<span class="ll ll-${cls}">${this.lineInner(line)}</span>`;
  }

  private lineInner(line: string): string {
    // client.log : `[iso] [LVL]` (+ `[pid N]` + `f.rs:N`) en meta,
    // reste en corps — même gabarit que renderClientLog.
    const cli = line.match(
      /^(\[[^\]]*\] \[(?:TRACE|DEBUG|INFO|WARN|ERROR)\])(?: (\[pid \d+\]))?(?: (\S+:\d+))?/i,
    );
    if (cli) {
      let body = `<span class="ll-meta">${esc(cli[1])}</span>`;
      if (cli[2]) body += ` <span class="ll-meta">${esc(cli[2])}</span>`;
      if (cli[3]) body += ` <span class="ll-meta">${esc(cli[3])}</span>`;
      return body + esc(line.slice(cli[0].length));
    }
    // daemon : `ISO  LVL` en meta, `cible: msg` en corps.
    const dmn = line.match(
      /^(\d{4}-\d{2}-\d{2}T\S+\s+(?:TRACE|DEBUG|INFO|WARN|ERROR))\s*(.*)$/i,
    );
    if (dmn) {
      return `<span class="ll-meta">${esc(dmn[1])}</span> ${esc(dmn[2] ?? "")}`;
    }
    return esc(line);
  }

  // -------------------------------------------------------------------
  // Init
  // -------------------------------------------------------------------

  init() {
    $("logs-close").onclick = () => this.close();
    $("logs-tab-client").onclick = () => this.setTab("client");
    $("logs-tab-daemon").onclick = () => this.setTab("daemon");
    ($("logs-level") as HTMLSelectElement).onchange = () => this.renderAll();
    ($("logs-pause") as HTMLButtonElement).onclick = () => {
      this.paused = !this.paused;
      this.syncPauseBtn();
      if (!this.paused) void this.tick();
    };
    ($("logs-conn") as HTMLSelectElement).onchange = () => {
      // Changement de conn → flux daemon frais (autre machine, autre
      // fichier) : offset/file repartent à zéro.
      this.tails.daemon = freshTail();
      this.syncDaemonNote();
      this.renderAll();
      void this.tick();
    };
    // Échap ferme le panneau — sauf si un dialogue est ouvert par-dessus.
    window.addEventListener("keydown", (e) => {
      if (
        e.key === "Escape" &&
        this.isOpen() &&
        !document.querySelector("dialog[open]")
      )
        this.close();
    });
    // Bouton titlebar.
    $("tb-logs").onclick = () => this.toggle();
  }
}
