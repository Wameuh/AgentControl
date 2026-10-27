// ---------------------------------------------------------------------------
// Surveillance connexions : ok | reconnecting | down
// ---------------------------------------------------------------------------
// Mort détectée via `conn-dead` (Rust : EOF du reader, instantané) ou une
// entrée {error} de la sonde fusionnée → down → retry en backoff
// exponentiel (2 s → 60 s max). Le retry est un `connect` simple : le démon
// est relancé par systemd (Restart=always) — jamais de réinstall en boucle.
// ---------------------------------------------------------------------------

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { $, esc, showErr } from "../core/dom";
import { call } from "../core/rpc";
import { app } from "../core/state";
import { connKeyOf } from "../core/conn";
import { isAndroid } from "../core/chrome";
import type { ConnLive } from "../core/types";
import { t } from "../i18n";
import { setAppIcon } from "./status";
import { hideMenu, menuBtn, placeMenu } from "../ui/ctxmenu";

// Conns déconnectées volontairement : ni alerte chip ni retry auto — un
// retry explicite (menu ctx / badge) les ré-active (delete dans retryConn).
const RETRY_MIN = 2_000, RETRY_MAX = 60_000;

export class ConnectionManager {
  private live = new Map<string, ConnLive>();
  private cfg = new Map<string, Record<string, unknown>>();
  private timer = new Map<string, ReturnType<typeof setTimeout>>();
  private delay = new Map<string, number>();
  private gone = new Set<string>();

  private deps = {
    /** Re-render de la sidebar (sessions + badges). */
    refresh: async () => {},
    /** Réouvre la session affichée si elle vivait sur cette connexion. */
    reopenIfConn: (_conn: string) => {},
    /** Ferme la conversation si la session affichée vivait sur cette conn. */
    closeIfConn: (_conn: string) => {},
    /** Purge les conversations cachées des sessions de cette conn. */
    dropConvsForConn: (_conn: string) => {},
    /** Purge les entrées sauvegardées matchant la conn (connections.json). */
    purgeSaved: async (_conn: string) => {},
    /** Purge les groupes nommés persistés de la conn. */
    clearGroups: (_conn: string) => {},
  };

  bind(d: Partial<typeof this.deps>) {
    Object.assign(this.deps, d);
  }

  keyOf(cfg: Record<string, unknown>) {
    return connKeyOf(cfg, isAndroid());
  }
  state(k: string): ConnLive | undefined {
    return this.live.get(k);
  }
  hasCfg(k: string) {
    return this.cfg.has(k);
  }
  isGone(k: string) {
    return this.gone.has(k);
  }
  knownConns(): string[] {
    return [...this.cfg.keys()];
  }
  liveEntries(): [string, ConnLive][] {
    return [...this.live];
  }

  /** Connexion établie → la surveillance/backoff la suit (fin de doConnect). */
  markConnected(cfg: Record<string, unknown>) {
    const ck = this.keyOf(cfg);
    this.cfg.set(ck, cfg);
    this.gone.delete(ck); // reconnectée via le dialogue → ré-active
    this.markAlive(ck);
  }

  /** Tooltip du badge selon l'état. */
  tip(st: ConnLive): string {
    if (st === "ok") return t("conn_ok");
    return `${t(st === "reconnecting" ? "reconnecting" : "conn_offline")} — ${t("retry_now")}`;
  }

  markAlive(key: string) {
    const t0 = this.timer.get(key);
    if (t0) {
      clearTimeout(t0);
      this.timer.delete(key);
    }
    this.delay.delete(key);
    const was = this.live.get(key);
    this.live.set(key, "ok");
    if (was !== "ok") this.renderState();
  }

  markDead(key: string) {
    // cfg n'est posée qu'à la fin d'un connect réussi — une conn dont
    // le tout premier connect a échoué en route n'est pas suivie (fenêtre
    // minime, pas de config retenue à rejouer de toute façon).
    if (!this.cfg.has(key) || this.gone.has(key)) return;
    if (this.live.get(key) !== "reconnecting") this.live.set(key, "down");
    this.renderState();
    this.scheduleRetry(key);
  }

  private scheduleRetry(key: string) {
    if (this.timer.has(key) || this.gone.has(key) || !this.cfg.has(key) ||
        this.live.get(key) === "reconnecting") return;
    const d = this.delay.get(key) ?? RETRY_MIN;
    this.delay.set(key, Math.min(d * 2, RETRY_MAX));
    this.timer.set(key, setTimeout(() => {
      this.timer.delete(key);
      void this.retryConn(key);
    }, d));
  }

  /** Reconnexion simple (connect, pas d'ensure_daemon) — auto + bouton. */
  private async retryConn(key: string) {
    const cfg = this.cfg.get(key);
    // Pas de retry parallèle : un connect déjà en vol suffit — un second
    // tunnel SSH orphelin resterait vivant après remplacement.
    if (!cfg || this.live.get(key) === "reconnecting") return;
    this.gone.delete(key); // un retry ré-active une conn déconnectée à la demande
    this.live.set(key, "reconnecting");
    this.renderState();
    try {
      await invoke("connect", { cfg });
      // Déconnectée/supprimée pendant le connect en vol → refermer aussitôt :
      // hors gone/cfg elle tournerait sans aucune surveillance.
      if (this.gone.has(key) || !this.cfg.has(key)) {
        await invoke("disconnect", { conn: key }).catch(() => {});
        if (this.gone.has(key)) this.live.set(key, "down"); else this.live.delete(key);
        this.renderState();
        return;
      }
      this.markAlive(key);
      // Réouvrir la session affichée si elle vivait sur cette connexion.
      this.deps.reopenIfConn(key);
      await this.deps.refresh();
    } catch {
      if (this.live.get(key) === "reconnecting") this.live.set(key, "down");
      this.scheduleRetry(key);
      this.renderState();
    }
  }

  /** Bouton "reconnecter" : annule l'attente backoff, retry immédiat. */
  retry(key: string) {
    const t0 = this.timer.get(key);
    if (t0) {
      clearTimeout(t0);
      this.timer.delete(key);
    }
    void this.retryConn(key);
  }

  /** Déconnexion volontaire : ferme le transport (le tunnel/process ssh est
   *  tué au drop du client côté Rust). La cfg reste posée → la conn
   *  reste listée (badge rouge) et reconnectable, mais hors surveillance :
   *  gone la met à l'abri du retry auto et du chip d'alerte. */
  async disconnect(conn: string) {
    this.gone.add(conn);
    const t0 = this.timer.get(conn);
    if (t0) {
      clearTimeout(t0);
      this.timer.delete(conn);
    }
    this.live.set(conn, "down");
    await invoke("disconnect", { conn }).catch((e) => showErr(String(e)));
    this.renderState();
  }

  /** Suppression d'une connexion : tue ses sessions côté démon (si encore
   *  joignable — sur une conn morte elles restent côté serveur), déconnecte
   *  puis oublie la cfg, les entrées sauvegardées et les groupes stockés. */
  async delete(conn: string) {
    const st = this.live.get(conn);
    // Hors ligne mais reconnectable : reconnexion éphémère pour pouvoir
    // tuer les sessions côté démon — sinon elles survivraient à la
    // suppression. "reconnecting" = connect déjà en vol, pas de second
    // en parallèle (tunnel SSH orphelin).
    if (st !== "ok" && st !== "reconnecting" && this.cfg.has(conn)) {
      const t0 = this.timer.get(conn);
      if (t0) { clearTimeout(t0); this.timer.delete(conn); }
      await invoke("connect", { cfg: this.cfg.get(conn) })
        .then(() => { this.live.set(conn, "ok"); this.gone.delete(conn); })
        .catch(() => {});
    }
    if ((this.live.get(conn) ?? "ok") === "ok" && !this.gone.has(conn)) {
      // Parallèle : N sessions à supprimer ne doivent pas coûter N RTT.
      await Promise.all(
        app.lastSessions
          .filter((x) => (x.conn ?? "default") === conn)
          .map((s) =>
            call("delete_session", { session: s.id, conn }).catch((e) => showErr(String(e))),
          ),
      );
    }
    await this.disconnect(conn);
    this.gone.delete(conn);
    this.cfg.delete(conn);
    this.live.delete(conn);
    this.delay.delete(conn);
    // Re-fetch + purge des entrées sauvegardées (une conn sauvegardée pendant
    // la session courante resterait sinon dans connections.json et
    // ressusciterait à l'autoconnect).
    await this.deps.purgeSaved(conn);
    this.deps.clearGroups(conn);
    // Conversations cachées de sessions de cette conn → purge (elles sont
    // supprimées côté démon quand il était joignable).
    this.deps.dropConvsForConn(conn);
    this.deps.closeIfConn(conn);
    await this.deps.refresh();
  }

  /** Menu contextuel connexion (clic droit / appui long sur le header de
   *  groupe) : reconnecter / déconnecter / supprimer — même pattern que
   *  le menu session. */
  openCtx(x: number, y: number, conn: string) {
    const ctx = $("sctx");
    ctx.innerHTML = `<div class="fctx-path">${esc(conn)}</div>`;
    const hide = () => hideMenu(ctx, "sctx");
    // "reconnecting" = connect en vol → déconnexion/suppression laisseraient
    // une conn vivante hors surveillance ; actions gelées le temps du retry.
    const busy = this.live.get(conn) === "reconnecting";
    const rec = menuBtn(ctx, t("ctx_reconnect"), "", () => {
      hide();
      this.retry(conn);
    });
    rec.disabled = !this.cfg.has(conn) || busy;
    const dis = menuBtn(ctx, t("ctx_disconnect"), "", () => {
      hide();
      void this.disconnect(conn);
    });
    dis.disabled = !this.cfg.has(conn) || this.gone.has(conn) || busy;
    const del = menuBtn(ctx, t("ctx_delete_conn"), "danger", (b) => {
      if (!b.dataset.armed) {
        b.dataset.armed = "1";
        const n = app.lastSessions.filter((x) => (x.conn ?? "default") === conn).length;
        b.textContent = t("ctx_del_conn_armed", { n });
        return;
      }
      hide();
      void this.delete(conn);
    });
    del.disabled = !this.cfg.has(conn) || busy;
    placeMenu(ctx, "sctx", x, y);
  }

  /** État agrégé → chip header ; re-render des badges de la sidebar. */
  renderState() {
    // gone (déconnecté à la demande) n'est pas une alerte.
    const bad = [...this.live].some(([k, s]) => s !== "ok" && !this.gone.has(k));
    const chip = $("conn-chip") as HTMLButtonElement;
    chip.hidden = !bad;
    if (bad) {
      const rc = [...this.live.values()].some((s) => s === "reconnecting");
      chip.textContent = rc ? `⟳ ${t("reconnecting")}` : `⚠ ${t("conn_offline")}`;
      chip.className = "badge " + (rc ? "warn" : "st-dead");
      chip.title = t("retry_now");
    }
    this.deps.refresh().catch(() => {});
  }

  init() {
    $("conn-chip").addEventListener("click", () => {
      for (const [k, s] of this.live) if (s !== "ok" && !this.gone.has(k)) this.retry(k);
    });
    // Mort instantanée du transport (EOF reader côté Rust).
    listen<string>("conn-dead", (e) => this.markDead(e.payload));

    // Watchdog : sonde fusionnée toutes les 10 s (timeout 8 s/conn côté Rust) —
    // filet de sécurité pour les blackholes réseau (pas d'EOF → pas de
    // conn-dead). Entrées {error} → markDead → backoff de reconnexion.
    setInterval(async () => {
      try {
        const st = await call<Record<string, unknown>>("daemon_status");
        let anyOk = false;
        for (const [k, v] of Object.entries(st)) {
          if (k === "servers" || k === "sessions") continue;
          if ((v as { error?: string }).error) this.markDead(k);
          else {
            anyOk = true;
            if ((this.live.get(k) ?? "ok") !== "ok") this.markAlive(k);
          }
        }
        // Aucune conn saine → démon(s) injoignable(s), icône grise.
        if (!anyOk) setAppIcon("grey");
      } catch {
        // L'appel fusionné lui-même a échoué — le backoff gère les retry.
        setAppIcon("grey");
      }
      this.deps.refresh().catch(() => {});
    }, 10_000);
  }
}
