// ---------------------------------------------------------------------------
// Vue plugin (option B — docs/plugin-views-exploration.md §5) : overlay
// plein #chat contenant une <iframe sandbox="allow-scripts"> (origine
// opaque — PAS allow-same-origin) dont les assets sont servis par le
// démon sous /plugin/<name>/ (token de bridge éphémère, TTL ~5 min,
// scopé au composant — jamais le web.token global).
//
// Bridge postMessage borné : le plugin ne peut appeler QUE les queries
// déclarées de SON composant (`component_query` côté client), notifier,
// naviguer vers une session/composant, redimensionner et ouvrir un
// lien https externe. Jamais d'invoke/RPC libre, jamais les autres
// composants, sessions, fichiers ou secrets.
//
// Validation stricte des messages entrants : event.source = l'iframe
// ouverte, event.origin = "null" (origine opaque du sandbox), schéma
// `ac` en whitelist + `component`/`token` du bootstrap `ready`.
// ---------------------------------------------------------------------------

import { $, notify } from "../core/dom";
import { call, callBg, bgErr } from "../core/rpc";
import { b64decode } from "../core/encoding";
import { isWeb } from "../core/bridge";
import { shellOpen } from "../core/chrome";
import { t } from "../i18n";
import { nav } from "../ui/nav";
import type { ConnectionManager } from "./connections";

/** Réponse `component_query` (miroir backlog.ts). */
interface QueryResp {
  code: number | null;
  data?: string;
  size?: number;
  truncated?: boolean;
  stderr?: string;
}

/** "default" = sentinelle mono-conn — la passerelle attend conn absent.
 *  (même convention que components.ts). */
const apiConn = (c: string) => (c === "default" ? undefined : c);

const b64text = (s: string | undefined) =>
  s ? new TextDecoder().decode(b64decode(s)) : "";

export class PluginView {
  private conns!: ConnectionManager;
  /** Fermer les autres overlays de #chat avant l'ouverture (injecté). */
  onOpening: () => void = () => {};
  /** Navigation « session:<id> » du bridge — injectée (chat.open). */
  openSession: (id: string, conn?: string) => void = () => {};
  /** Navigation « component:<name> » du bridge — injectée (panneau
   *  composants). */
  openComponents: () => void = () => {};

  private name = "";
  private connKey = "";
  private token = "";
  private opened = false;

  bind(conns: ConnectionManager) {
    this.conns = conns;
  }

  private isOpen() {
    return !$("plugin-view").hidden;
  }

  /** Capability gate : le démon connaît PluginViewToken + sert
   *  /plugin/<name>/ (daemon_status.plugin_views). */
  capable(ck: string): boolean {
    return this.conns.cap(ck)?.plugin_views === true;
  }

  /** Base HTTP du démon pour cette connexion : en mode web la SPA est
   *  servie par le démon → same-origin ; sinon l'URL web déclarée dans
   *  daemon_status.web (bind loopback — joignable pour une conn locale,
   *  pas pour un démon distant en ssh : la vue reste proposée, l'iframe
   *  affichera l'erreur de chargement — POC). */
  private webBase(ck: string): string | null {
    if (isWeb) return location.origin;
    const web = this.conns.cap(ck)?.web as
      | { bind?: string; url?: string }
      | undefined;
    if (web?.bind) return `http://${web.bind}`;
    if (web?.url) {
      try {
        return new URL(web.url).origin;
      } catch {
        return null;
      }
    }
    return null;
  }

  async open(name: string, connKey: string) {
    if (this.isOpen()) this.close();
    if (!this.capable(connKey)) {
      notify("warn", t("pv_unsupported"));
      return;
    }
    const base = this.webBase(connKey);
    if (!base) {
      notify("warn", t("pv_noweb"));
      return;
    }
    const conn = apiConn(connKey);
    let tok: { token?: string };
    try {
      // Action utilisateur (clic « vue ») → call strict : une conn
      // hors-ligne échoue tout de suite au lieu d'attendre sa réparation.
      tok = await call<{ token?: string }>("plugin_view_token", {
        name,
        conn,
      });
    } catch (e) {
      bgErr(t("comp_view"), e, connKey);
      return;
    }
    if (!tok?.token) {
      notify("warn", t("pv_noweb"));
      return;
    }
    this.name = name;
    this.connKey = connKey;
    this.token = tok.token;
    this.onOpening();
    const view = $("plugin-view");
    view.hidden = false;
    $("pv-title").textContent = name;
    const frame = $("pv-frame") as HTMLIFrameElement;
    frame.src = `${base}/plugin/${encodeURIComponent(name)}/?token=${encodeURIComponent(tok.token)}`;
    // Sandbox SANS allow-same-origin : origine opaque, pas d'accès au
    // DOM/storage/token de la page parente — tout passe par le bridge.
    frame.setAttribute("sandbox", "allow-scripts");
    frame.setAttribute("title", name);
    nav.push("plugin");
    window.addEventListener("message", this.onMsg);
    this.opened = true;
  }

  close() {
    this.opened = false;
    window.removeEventListener("message", this.onMsg);
    const frame = $("pv-frame") as HTMLIFrameElement;
    // src="about:blank" démonte le document plugin (et son JS) — pas de
    // iframe zombie gardant son token éphémère en mémoire.
    frame.removeAttribute("src");
    frame.src = "about:blank";
    $("plugin-view").hidden = true;
    nav.close("plugin");
    this.name = "";
    this.connKey = "";
    this.token = "";
  }

  toggle() {
    if (this.isOpen()) this.close();
  }

  /** Thème courant → `theme` au plugin (envoyé à l'ouverture, rejoué
   *  quand les settings changent — hook main.ts). */
  private dark(): boolean {
    return !document.documentElement.classList.contains("light");
  }

  private post(m: Record<string, unknown>) {
    const frame = $("pv-frame") as HTMLIFrameElement;
    frame.contentWindow?.postMessage({ ...m, component: this.name }, "*");
  }

  /** Changement de thème settings → resynchronise le plugin. */
  onThemeChanged() {
    if (this.isOpen()) this.post({ ac: "theme", dark: this.dark() });
  }

  // -- Bridge postMessage ----------------------------------------------------

  /** Messages plugin → client. Whitelist stricte : source = l'iframe
   *  ouverte, origine opaque ("null"), `ac` connu, component+token du
   *  bootstrap `ready` — un message forgé par un autre iframe ou une
   *  page tierce est ignoré. */
  private onMsg = (ev: MessageEvent) => {
    if (!this.isOpen()) return;
    const frame = $("pv-frame") as HTMLIFrameElement;
    if (ev.source !== frame.contentWindow) return;
    // sandbox sans allow-same-origin → origine opaque : "null" attendu.
    if (ev.origin !== "null") return;
    let m: Record<string, unknown>;
    try {
      m = JSON.parse(typeof ev.data === "string" ? ev.data : "");
    } catch {
      return;
    }
    if (!m || typeof m !== "object") return;
    if (m.component !== this.name || m.token !== this.token) return;
    switch (m.ac) {
      case "query":
        void this.answerQuery(m);
        break;
      case "notify":
        this.onNotify(m);
        break;
      case "navigate":
        this.onNavigate(m);
        break;
      case "resize":
        this.onResize(m);
        break;
      case "open-external":
        this.onOpenExternal(m);
        break;
      // Messages client→plugin (ready/data/theme) reçus en écho ou
      // hors contrat : ignorés.
      default:
        break;
    }
  };

  /** Le plugin demande une query déclarée de SON composant — exécutée
   *  via `component_query` (argv fixe audité, sortie bornée — le même
   *  canal que la vue Backlog native), jamais de RPC libre. */
  private async answerQuery(m: Record<string, unknown>) {
    const id = m.id;
    const q = typeof m.query === "string" ? m.query : "";
    const reply = (ok: boolean, data?: string, error?: string) =>
      this.post({ ac: "data", id, ok, data, error });
    if (!q) {
      reply(false, undefined, "missing query");
      return;
    }
    try {
      const r = await callBg<QueryResp>("component_query", {
        name: this.name,
        query: q,
        conn: apiConn(this.connKey),
      });
      if (r.code !== 0) {
        const tail = b64text(r.stderr).trim();
        reply(
          false,
          undefined,
          `exit ${r.code ?? "?"}${tail ? ` — ${tail.slice(0, 140)}` : ""}`,
        );
        return;
      }
      reply(true, b64text(r.data));
    } catch (e) {
      reply(false, undefined, String(e).slice(0, 200));
    }
  }

  private onNotify(m: Record<string, unknown>) {
    const level =
      m.level === "warn" || m.level === "error" ? m.level : "info";
    const text = typeof m.text === "string" ? m.text.slice(0, 500) : "";
    if (text) notify(level, text);
  }

  private onNavigate(m: Record<string, unknown>) {
    const target = typeof m.target === "string" ? m.target : "";
    const [kind, id] = target.split(":", 2);
    if (!id) return;
    if (kind === "session") {
      const conn = apiConn(this.connKey);
      this.close();
      this.openSession(id, conn);
    } else if (kind === "component") {
      this.close();
      this.openComponents();
    } else {
      notify("warn", `navigate: cible inconnue « ${target} »`);
    }
  }

  private onResize(m: Record<string, unknown>) {
    const h = typeof m.height === "number" ? m.height : 0;
    if (h > 0) {
      ($("pv-frame") as HTMLIFrameElement).style.height = `${Math.min(Math.max(h, 120), 4000)}px`;
    }
  }

  private onOpenExternal(m: Record<string, unknown>) {
    // Allowlist https seul — un plugin ne doit pas pouvoir ouvrir
    // javascript:/file:/http: (mixed content + phishing).
    const url = typeof m.url === "string" ? m.url : "";
    try {
      if (new URL(url).protocol === "https:") shellOpen(url);
    } catch {
      /* URL invalide : ignorée */
    }
  }

  init() {
    $("pv-close").onclick = () => this.close();
    // Bootstrap : le plugin ne reçoit son token que via `ready` —
    // jamais dans une URL pérenne ni dans un message non sollicité.
    const frame = $("pv-frame") as HTMLIFrameElement;
    frame.addEventListener("load", () => {
      if (!this.isOpen()) return;
      this.post({ ac: "ready", token: this.token });
      this.post({ ac: "theme", dark: this.dark() });
    });
    // Échap ferme la vue — sauf si un dialogue est ouvert par-dessus.
    window.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && this.isOpen() && !document.querySelector("dialog[open]"))
        this.close();
    });
  }
}
