// ---------------------------------------------------------------------------
// Surveillance connexions : ok | reconnecting | down
// ---------------------------------------------------------------------------
// Mort détectée via `conn-dead` (Rust : EOF du reader, instantané) ou
// 2 entrées {error} consécutives de la sonde fusionnée → down → retry en
// backoff exponentiel + jitter ±25 % (2 s → 60 s de base, ~75 s réel
// max). Le retry est un `connect` simple : le démon est relancé par
// systemd (Restart=always) — jamais de réinstall en boucle.
// ---------------------------------------------------------------------------

import { invoke, listen, isWeb } from "../core/bridge";
import { $, esc, q, showErr, logUi } from "../core/dom";
import { call } from "../core/rpc";
import { app } from "../core/state";
import { connKeyOf, connLabel } from "../core/conn";
import { accentOf, isAndroid, type Accent } from "../core/chrome";
import type { ConnLive } from "../core/types";
import { t } from "../i18n";
import { setAppIcon } from "./status";
import { icon } from "../ui/icons";
import { hideMenu, menuBtn, placeMenu } from "../ui/ctxmenu";

// Conns déconnectées volontairement : ni alerte chip ni retry auto — un
// retry explicite (menu ctx / badge) les ré-active (delete dans retryConn).
const RETRY_MIN = 2_000, RETRY_MAX = 60_000;
/** Fenêtre de « trafic entrant récent » : un timeout de sonde avec
 *  silent_ms sous ce seuil = transport vivant (réponse lente, démon
 *  sous charge) — excusé. Au-delà = silence radio suspect (blackhole)
 *  → strike. C'est une fenêtre de fraîcheur, PAS le timeout de sonde :
 *  il vaut 8 s côté Tauri (`secs` dans session.rs) et CALL_TIMEOUT
 *  (60 s) côté web — les deux produisent silent_ms, comparé ici. */
const SILENT_KILL_MS = 8_000;
/** Timeouts de sonde excusés d'affilée avant de compter un strike
 *  quand même : borne l'exemption « transport vivant » — si le chemin
 *  RPC est durablement bloqué alors que les pushes continuent (voies
 *  différentes), la conn serait sinon injouable sans jamais être
 *  recyclée. 6 ≈ 1 min de sondes en échec à 10 s d'intervalle. */
const IGNORED_STRIKE_MAX = 6;

export class ConnectionManager {
  private live = new Map<string, ConnLive>();
  private cfg = new Map<string, Record<string, unknown>>();
  /** Status démon par conn (capabilities : self_update, first_post,
   *  terminal…). */
  private caps = new Map<string, Record<string, unknown>>();
  private timer = new Map<string, ReturnType<typeof setTimeout>>();
  private delay = new Map<string, number>();
  /** Horodatage du prochain essai auto (tooltip "réessai dans ~Ns"). */
  private nextAt = new Map<string, number>();
  /** Essais consécutifs depuis le dernier succès (tooltip). */
  private attempts = new Map<string, number>();
  /** Échecs de sonde consécutifs — une seule erreur ne déclare plus la
   *  conn down : un ralenti transitoire (tunnel congestionné mais vivant)
   *  ne doit pas déclencher un cycle de reconnexion complet. `conn-dead`
   *  (EOF réel du transport) reste instantané. */
  private strikes = new Map<string, number>();
  /** Timeouts de sonde excusés d'affilée par conn (transport vivant) —
   *  plafonnés par IGNORED_STRIKE_MAX : au-delà, le chemin RPC est
   *  durablement bloqué et l'échec compte malgré le trafic. */
  private ignored = new Map<string, number>();
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
    /** Marque des ids session supprimés localement → pas de toast au
     *  retour de leurs events session_deleted (cascade conn-delete). */
    notePendingDel: (_ids: string[]) => {},
    /** Démarque un id dont la suppression en cascade a échoué. */
    clearPendingDel: (_id: string) => {},
    /** Capabilities rafraîchies → gates UI dépendants du démon. */
    capsChanged: () => {},
    /** Chrome (accent + liseré + titre) de la connexion liée — null quand
     *  plus aucune conn n'est liée (suppression de la dernière). */
    connChrome: (_conn: string | null, _color?: Accent | null) => {},
    /** Transport d'une conn relâché alors que ses sessions démon vivent
     *  encore (éviction) : aucun event conn-dead/session_deleted ne
     *  viendra — les terminaux de cette conn doivent être marqués morts. */
    termDead: (_conn: string) => {},
    /** États live recalculés (renderState) — la vue connexion met à jour
     *  ses badges de statut et réarme son accueil auto. */
    liveChanged: () => {},
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
  /** Status démon brut de la conn (capabilities). Le fallback « seule
   *  entrée » n'est joué que pour la sentinelle "default" en mono-conn —
   *  une clé réelle inconnue ne doit jamais hériter des capabilities
   *  d'un autre démon (bouton affiché à tort). */
  cap(k: string): Record<string, unknown> | undefined {
    return this.caps.get(k) ??
      (k === "default" && this.caps.size === 1 ? [...this.caps.values()][0] : undefined);
  }

  /** Flags de capability = les champs booléens du daemon_status. La
   *  comparaison évite de notifier capsChanged à chaque tick du watchdog
   *  (updateHeader = RPC queue_list + rebuild des selects, toutes les
   *  10 s sinon — y compris dropdown ouvert). */
  private capFlags(v: Record<string, unknown>): string {
    return JSON.stringify(
      Object.fromEntries(Object.entries(v).filter(([, x]) => typeof x === "boolean")),
    );
  }
  private setCaps(k: string, v: Record<string, unknown>) {
    if (this.capFlags(this.caps.get(k) ?? {}) === this.capFlags(v)) {
      this.caps.set(k, v);
      return;
    }
    this.caps.set(k, v);
    this.deps.capsChanged();
  }

  /** Connexion établie → la surveillance/backoff la suit (fin de doConnect).
   *  `status` = daemon_status déjà en main (`connect` le retourne) —
   *  sinon probe immédiat pour les capabilities. */
  markConnected(cfg: Record<string, unknown>, status?: Record<string, unknown>) {
    const ck = this.keyOf(cfg);
    this.cfg.set(ck, cfg);
    this.gone.delete(ck); // reconnectée via le dialogue → ré-active
    this.markAlive(ck);
    if (status && !status.error) {
      this.setCaps(ck, status);
      return;
    }
    void call<Record<string, unknown>>("daemon_status", { conn: ck })
      .then((v) => {
        if (!v.error) this.setCaps(ck, v);
      })
      .catch(() => {});
  }

  /** Re-bind exclusif (modèle multi-instances : 1 instance = 1 connexion)
   *  — appelé après markConnected : toute autre connexion encore vivante
   *  est déconnectée et oubliée SANS toucher à ses sessions démon (elles
   *  restent pilotables par une autre instance) ni à ses données client
   *  (groupes persistés, entrée sauvegardée — valables à la prochaine
   *  liaison). Seule la conversation affichée d'une conn évincée se ferme
   *  (sa session n'appartient plus à ce monde). */
  async evictExcept(keep: string) {
    const others = [
      ...new Set([...this.cfg.keys(), ...this.live.keys()]),
    ].filter((k) => k !== keep);
    for (const k of others) {
      const t0 = this.timer.get(k);
      if (t0) clearTimeout(t0);
      this.timer.delete(k);
      this.live.delete(k);
      this.cfg.delete(k);
      this.caps.delete(k);
      this.delay.delete(k);
      this.nextAt.delete(k);
      this.attempts.delete(k);
      this.strikes.delete(k);
      this.ignored.delete(k);
      this.gone.delete(k);
      this.deps.dropConvsForConn(k);
      this.deps.closeIfConn(k);
      this.deps.termDead(k);
      await invoke("disconnect", { conn: k }).catch(() => {});
    }
    if (others.length) this.renderState();
    this.chromeForCurrent();
  }

  /** Accent + titre de la connexion liée courante (null = aucune). */
  private chromeForCurrent() {
    const k = this.cfg.keys().next().value ?? null;
    this.deps.connChrome(k, k ? (accentOf(this.cfg.get(k)?.color) ?? null) : null);
  }

  /** Ré-applique le chrome de la conn liée — après un changement de
   *  l'accent global (la couleur de la conn reprend le dessus si elle
   *  en a une). */
  reapplyChrome() {
    this.chromeForCurrent();
  }

  /** Tooltip du badge selon l'état (avec prochain essai / nb de
   *  tentatives). `conn` "gone" (déconnectée à la demande) → libellé
   *  dédié : elle ne réessaiera jamais toute seule. */
  tip(st: ConnLive, conn = ""): string {
    if (st === "ok") return t("conn_ok");
    if (conn && this.gone.has(conn)) return t("conn_off");
    if (st === "reconnecting") {
      const n = this.attempts.get(conn) ?? 0;
      return t("reconnecting") + (n > 1 ? ` (×${n})` : "");
    }
    const at = this.nextAt.get(conn);
    const s = at ? Math.max(0, Math.round((at - Date.now()) / 1000)) : 0;
    const wait = s > 0 ? ` — ${t("retry_in", { s })}` : "";
    return `${t("conn_offline")}${wait} — ${t("retry_now")}`;
  }

  markAlive(key: string) {
    const t0 = this.timer.get(key);
    if (t0) {
      clearTimeout(t0);
      this.timer.delete(key);
    }
    this.delay.delete(key);
    this.nextAt.delete(key);
    this.attempts.delete(key);
    this.strikes.delete(key);
    this.ignored.delete(key);
    const was = this.live.get(key);
    this.live.set(key, "ok");
    if (was !== "ok") {
      // « de retour » n'a de sens que sur une vraie panne — au premier
      // connect (was undefined) le log serait du bruit.
      if (was !== undefined)
        logUi("info", `conn ${key} de retour (était ${was})`);
      this.renderState();
    }
  }

  /** Échec de sonde (timeout / error dans l'entrée fusionnée) : la conn
   *  n'est déclarée down qu'au 2ᵉ échec consécutif — un hoquet du lien
   *  (flush lent d'un tunnel congestionné) ne coûte plus un cycle
   *  kill-tunnel/reconnect/replay qui le re-saturait aussitôt.
   *  Raffiné : un timeout avec transport vivant (silent_ms < fenêtre de
   *  sonde → du trafic entrant arrive encore) n'est pas une mort mais
   *  une lenteur démon — tuer la conn ajouterait reconnect+resubscribe
   *  à la surcharge causale (flap auto-entretenu). Le blackhole, lui,
   *  ne produit QUE du silence : ses timeouts comptent normalement.
   *  Retour : true si l'échec est excusé (transport vivant — la conn
   *  compte « up » pour l'icône d'état), false sinon. */
  private probeFailed(key: string, v: {
    error?: string; kind?: string; silent_ms?: number; rtt_ms?: number;
  }): boolean {
    if (!this.cfg.has(key) || this.gone.has(key)) return false;
    const err = v.error ?? "?";
    // kind champ structuré (fusion ≥ cette version) ; repli sur le
    // littéral "timeout" pour les vieux démons / bridges sans kind.
    const isTimeout = v.kind === "timeout" ||
      (v.kind == null && err === "timeout");
    if (isTimeout && (v.silent_ms ?? Infinity) < SILENT_KILL_MS) {
      const ni = (this.ignored.get(key) ?? 0) + 1;
      if (ni < IGNORED_STRIKE_MAX) {
        this.ignored.set(key, ni);
        logUi("warn",
          `conn ${key}: sonde timeout mais transport vivant ` +
          `(silent ${v.silent_ms} ms, rtt ${v.rtt_ms ?? "?"} ms) — ignoré ×${ni}`);
        return true;
      }
      // Plafond : le RPC est durablement bloqué malgré un transport
      // vivant — l'échec compte, faute de quoi la conn serait
      // injouable à jamais sans être recyclée.
      this.ignored.delete(key);
      logUi("warn",
        `conn ${key}: ${ni} timeouts d'affilée malgré un transport ` +
        `vivant — RPC bloqué durablement → strike`);
    }
    const n = (this.strikes.get(key) ?? 0) + 1;
    this.strikes.set(key, n);
    logUi("warn",
      `conn ${key}: sonde ko ×${n} (${err}` +
      `${v.silent_ms != null ? `, silent ${v.silent_ms} ms` : ""}` +
      `${v.rtt_ms != null ? `, rtt ${v.rtt_ms} ms` : ""})`);
    if (n >= 2) this.markDead(key, `sonde: ${err} ×${n}`);
    return false;
  }

  markDead(key: string, why = "conn-dead (EOF transport)") {
    // cfg n'est posée qu'à la fin d'un connect réussi — une conn dont
    // le tout premier connect a échoué en route n'est pas suivie (fenêtre
    // minime, pas de config retenue à rejouer de toute façon).
    if (!this.cfg.has(key) || this.gone.has(key)) return;
    const was = this.live.get(key);
    if (was !== "reconnecting") this.live.set(key, "down");
    // La transition est loguée une fois — chaque sonde ko refraîchit
    // son propre log ×N, re-crier « down » à chaque tick est du spam.
    if (was !== "down" && was !== "reconnecting")
      logUi("error", `conn ${key} down — ${why}`);
    this.renderState();
    this.scheduleRetry(key);
  }

  private scheduleRetry(key: string) {
    if (this.timer.has(key) || this.gone.has(key) || !this.cfg.has(key) ||
        this.live.get(key) === "reconnecting") return;
    const base = this.delay.get(key) ?? RETRY_MIN;
    this.delay.set(key, Math.min(base * 2, RETRY_MAX));
    // Jitter ±25 % : des retry cadencés pile sur le tick watchdog se
    // ré-alignaient en rafale (plusieurs conns/clients tombant ensemble
    // se réveillaient ensemble et re-saturaient le lien malade).
    const d = Math.round(base * (0.75 + Math.random() * 0.5));
    this.nextAt.set(key, Date.now() + d);
    this.timer.set(key, setTimeout(() => {
      this.timer.delete(key);
      this.nextAt.delete(key);
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
    this.attempts.set(key, (this.attempts.get(key) ?? 0) + 1);
    this.live.set(key, "reconnecting");
    this.renderState();
    const t0 = Date.now();
    try {
      const st = await invoke<Record<string, unknown>>("connect", { cfg });
      // Déconnectée/supprimée pendant le connect en vol → refermer aussitôt :
      // hors gone/cfg elle tournerait sans aucune surveillance.
      if (this.gone.has(key) || !this.cfg.has(key)) {
        await invoke("disconnect", { conn: key }).catch(() => {});
        if (this.gone.has(key)) this.live.set(key, "down"); else this.live.delete(key);
        this.renderState();
        return;
      }
      this.markAlive(key);
      logUi("info",
        `conn ${key}: reconnect #${this.attempts.get(key) ?? 1} ok (${Date.now() - t0} ms)`);
      // connect retourne déjà daemon_status → capabilities à jour sans RTT.
      if (st && !st.error) this.setCaps(key, st);
      // Réouvrir la session affichée si elle vivait sur cette connexion.
      this.deps.reopenIfConn(key);
      await this.deps.refresh();
    } catch (e) {
      if (this.live.get(key) === "reconnecting") this.live.set(key, "down");
      logUi("warn",
        `conn ${key}: reconnect #${this.attempts.get(key) ?? "?"} ko (${Date.now() - t0} ms) — ${e}`);
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
    this.nextAt.delete(key);
    void this.retryConn(key);
  }

  /** Déconnexion volontaire : ferme le transport (le tunnel/process ssh est
   *  tué au drop du client côté Rust). La cfg reste posée → la conn
   *  reste listée (badge rouge) et reconnectable, mais hors surveillance :
   *  gone la met à l'abri du retry auto et du chip d'alerte. Les sessions
   *  démon restent vivantes mais leur transport client est relâché → même
   *  cas qu'une éviction : aucun conn-dead ne viendra, les terminaux de
   *  cette conn sont marqués morts explicitement. */
  async disconnect(conn: string) {
    this.gone.add(conn);
    const t0 = this.timer.get(conn);
    if (t0) {
      clearTimeout(t0);
      this.timer.delete(conn);
    }
    this.nextAt.delete(conn);
    this.live.set(conn, "down");
    this.deps.termDead(conn);
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
      // Ids marqués locaux → leurs events session_deleted ne toastent pas.
      this.deps.notePendingDel(
        app.lastSessions
          .filter((x) => (x.conn ?? "default") === conn)
          .map((x) => x.id),
      );
      await Promise.all(
        app.lastSessions
          .filter((x) => (x.conn ?? "default") === conn)
          .map((s) => {
            app.spawnWatch.delete(s.id); // suppression voulue, pas un échec
            return call("delete_session", { session: s.id, conn }).catch((e) => {
              this.deps.clearPendingDel(s.id); // échec → la session existe encore
              showErr(String(e));
            });
          }),
      );
    }
    await this.disconnect(conn);
    this.gone.delete(conn);
    this.cfg.delete(conn);
    this.live.delete(conn);
    this.caps.delete(conn);
    this.delay.delete(conn);
    this.nextAt.delete(conn);
    this.attempts.delete(conn);
    this.strikes.delete(conn);
    this.ignored.delete(conn);
    this.caps.delete(conn);
    // La conn sort de cfg : renderState (sb-left/chip) la retire sans
    // attendre une transition d'état ultérieure.
    this.renderState();
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
    // Plus de conn liée (single-conn : c'était forcément la sienne) →
    // retour au chrome neutre tant que rien n'est reconnecté.
    this.chromeForCurrent();
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
    // Nouvelle instance liée à cette connexion (multi-instances : 1
    // processus = 1 connexion — desktop seulement, ni web ni Android).
    if (!isAndroid() && !isWeb) {
      const ni = menuBtn(ctx, t("ctx_new_instance"), "", () => {
        hide();
        invoke("spawn_instance", { conn }).catch((e) => showErr(String(e)));
      });
      ni.disabled = !this.cfg.has(conn);
    }
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

  /** Une conn bound est-elle joignable (« ok » ou « reconnecting ») ?
   *  Faux si rien n'est bound ou si tout est down/gone — la vue connexion
   *  devient alors l'écran d'accueil (maybeAutoShow). */
  anyUp() {
    return [...this.cfg.keys()].some((k) => {
      const s = this.gone.has(k) ? "down" : this.live.get(k);
      return s === "ok" || s === "reconnecting";
    });
  }

  /** État agrégé → chip header + résumé statusbar ; badges sidebar via
   *  refresh. Une seule conn down → son nom dans le chip ; plusieurs →
   *  le compte. Le tooltip liste le détail par connexion. */
  renderState() {
    // gone (déconnecté à la demande) n'est pas une alerte.
    const bad = [...this.live].filter(([k, s]) => s !== "ok" && !this.gone.has(k));
    const chip = $("conn-chip") as HTMLButtonElement;
    chip.hidden = !bad.length;
    if (bad.length) {
      const allRe = bad.every(([, s]) => s === "reconnecting");
      chip.innerHTML =
        `${icon(allRe ? "refresh" : "warn")} ` +
        esc(bad.length === 1 ? connLabel(bad[0][0])
          : allRe ? t("reconnecting") : t("conns_down", { n: bad.length }));
      chip.className = "badge " + (allRe ? "warn" : "st-dead");
      // tip() suffixe déjà « — reconnecter » sur les conns down ; le chip
      // n'ajoute son propre rappel que si tout est « en essai » (cliquer
      // reste un retry immédiat).
      chip.title =
        bad.map(([k, s]) => `${connLabel(k)}: ${this.tip(s, k)}`).join("\n") +
        (allRe ? ` — ${t("retry_now")}` : "");
    }
    // Statusbar : résumé des conns suivies (● ok, ⟳ essai, ● hors ligne,
    // ◌ déconnectée à la demande) — slot sb-left sinon inutilisé. Une conn
    // down/off est cliquable : retry immédiat (le tooltip le promet).
    const sb = $("sb-left");
    sb.innerHTML = [...this.cfg.keys()]
      .map((k) => {
        const st = this.gone.has(k) ? "off" : (this.live.get(k) ?? "ok");
        const cls = st === "ok" ? "ok" : st === "reconnecting" ? "warn" : st === "down" ? "bad" : "off";
        const ico = st === "reconnecting" ? icon("refresh") : icon("dot");
        const clk = st === "down" || st === "off" ? " clk" : "";
        // Attributs : esc() n'échappe pas '"' → q() (conn label issu
        // de la cfg sauvegardée, texte libre).
        return `<span class="conn-seg${clk}" data-conn="${q(esc(k))}" title="${q(esc(this.tip(st === "off" ? "down" : st, k)))}"><span class="conn-st ${cls}">${ico}</span> ${esc(connLabel(k))}</span>`;
      })
      .join('<span class="sb-sep"> · </span>');
    this.deps.refresh().catch(() => {});
    this.deps.liveChanged();
  }

  init() {
    $("conn-chip").addEventListener("click", () => {
      for (const [k, s] of this.live) if (s !== "ok" && !this.gone.has(k)) this.retry(k);
    });
    // Clic sur un segment conn de la statusbar → retry si elle est
    // hors ligne/déconnectée (les segments ok ne sont pas marqués clk).
    $("sb-left").addEventListener("click", (e) => {
      const seg = (e.target as HTMLElement).closest<HTMLElement>(".conn-seg.clk");
      const k = seg?.dataset.conn;
      if (k && (this.gone.has(k) || this.live.get(k) === "down")) this.retry(k);
    });
    // Mort instantanée du transport (EOF reader côté Rust).
    listen<string>("conn-dead", (e) => this.markDead(e.payload));

    // Watchdog : sonde fusionnée toutes les 10 s (timeout 8 s/conn côté Rust) —
    // filet de sécurité pour les blackholes réseau (pas d'EOF → pas de
    // conn-dead). Entrées {error} → probeFailed (2 strikes) → backoff.
    // __acWdMs : intervalle raccourci pour les harnais e2e.
    const wdMs =
      (window as unknown as { __acWdMs?: number }).__acWdMs ?? 10_000;
    setInterval(async () => {
      try {
        const st = await call<Record<string, unknown>>("daemon_status");
        let anyOk = false;
        for (const [k, v] of Object.entries(st)) {
          if (k === "servers" || k === "sessions") continue;
          if ((v as { error?: string }).error) {
            // Échec excusé (timeout, transport vivant) = la conn est
            // joignable — l'icône ne passe pas grise en même temps
            // que le segment reste vert.
            anyOk = this.probeFailed(k, v as {
              error?: string; kind?: string; silent_ms?: number; rtt_ms?: number;
            }) || anyOk;
          } else {
            anyOk = true;
            // Succès = la série d'échecs est rompue, même si la conn
            // n'avait jamais été déclarée down (1 strike effacé).
            this.strikes.delete(k);
            this.ignored.delete(k);
            this.setCaps(k, v as Record<string, unknown>);
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
    }, wdMs);
  }
}
