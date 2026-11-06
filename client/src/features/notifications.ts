// ---------------------------------------------------------------------------
// Notifications OS natives — canal d'alerte indépendant des sons, alimenté
// par le même diff de transitions (state-diff.ts) : done / question / error.
// Plugin tauri-plugin-notification : la permission OS est demandée au
// premier besoin (envoi réel, bouton test, activation du toggle) ; un refus
// est remonté dans les réglages comme le hint « audio bloqué ».
// Clic sur la notif → focus de la fenêtre + ouverture de la session quand
// la plateforme rapporte la payload (extra) — sinon focus seul.
// Inerte en mode web (pas d'IPC plugin) : la section des réglages est
// masquée côté settings.ts.
// ---------------------------------------------------------------------------

import { invoke, isWeb } from "../core/bridge";
import { isAndroid } from "../core/chrome";
import { connLabel } from "../core/conn";
import { logUi } from "../core/dom";
import type { SessionMeta } from "../core/types";
import { t, type Key } from "../i18n";
import type { AlertKind } from "./state-diff";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  createChannel,
  Importance,
  isPermissionGranted,
  onAction,
  requestPermission,
  type ActionPerformed,
  type Options,
} from "@tauri-apps/plugin-notification";

/** Canal Android (créé au boot, importance heads-up) — ignoré ailleurs. */
const CHANNEL_ID = "agent-status";
/** Anti-empilement : une session ne produit pas deux alertes du même type
 *  en moins de 10 s (boucle working↔idle, restart — le shade se remplirait). */
const COOLDOWN_MS = 10_000;

// --- Réglages (localStorage) ------------------------------------------------

export const notifEnabled = () => localStorage.getItem("ac-notif") !== "0";
export const notifTypeOn = (k: AlertKind) =>
  localStorage.getItem(`ac-notif-${k}`) !== "0";
export const notifOnlyUnfocused = () =>
  localStorage.getItem("ac-notif-unfocused") === "1";

const EV_KEY: Record<AlertKind, Key> = {
  done: "notif_ev_done",
  question: "notif_ev_question",
  error: "notif_ev_error",
};

class OsNotifications {
  private deps = {
    /** Ouvrir la session au clic — même trajet qu'un clic sidebar. */
    openSession: (_id: string, _conn: string) => {},
    /** Plus d'une connexion bound → le nom de la conn complète le corps. */
    connCount: () => 1,
  };
  /** onAction armé : sans listener vivant, le plugin affiche les notifs
   *  desktop sans leurs actions (le tap n'est jamais rapporté). */
  private ready = false;
  /** Canal Android créé (sinon les envois passent sans channelId). */
  private channelOk = false;
  /** État de permission connu : undefined = pas encore sondé. */
  private granted: boolean | undefined;
  /** Dernier envoi par "conn:sid:kind" — borne l'empilement en rafale. */
  private lastSent = new Map<string, number>();
  /** Ids OS des notifs émises, indexés par "conn:sid:kind" — nécessaires
   *  au cancel() quand la condition se résout autrement que par un clic
   *  (#184 : le compteur DE/shade sinon ne redescend jamais). L'entrée
   *  garde ses composantes : la clé n'est PAS re-parsable (connKeyOf
   *  produit du multi-segments — "ssh:host:port"). */
  private ids = new Map<
    string,
    { id: number; conn: string; sid: string; kind: string }
  >();
  /** Prochain id alloué — posés nous-mêmes dans options.id : pas besoin
   *  de la valeur de retour de notify. Sur Android le ré-envoi même id
   *  remplace la notif précédente ; sur desktop elle empile, mais les
   *  ids suivis permettent au remove_active de fermer d'un coup toutes
   *  les notifs partageant l'id. Base aléatoire pour ne pas écraser les
   *  ids choisis par le plugin hors de ce canal. */
  private nextId = 1_000_000 + Math.floor(Math.random() * 1_000_000);
  /** Dernier balayage reconcile() — l'IPC get_active reste borné à
   *  ~1/30 s (le refresh sidebar tourne toutes les ~10 s). */
  private lastReconcile = 0;

  bind(deps: Partial<typeof this.deps>) {
    Object.assign(this.deps, deps);
  }

  /** Boot : arme le listener d'actions (obligatoire avant tout envoi
   *  desktop pour que le clic soit rapporté) et le canal Android. */
  init() {
    if (isWeb) return;
    void this.setup();
  }

  private async setup() {
    try {
      await onAction((e) => void this.clicked(e));
    } catch (e) {
      logUi("warn", `notif: listener d'actions indisponible: ${e}`);
      return; // pas de listener = notifs non cliquables → canal coupé
    }
    if (isAndroid()) {
      try {
        await createChannel({
          id: CHANNEL_ID,
          name: t("notif_channel"),
          description: t("notif_channel_desc"),
          importance: Importance.High,
        });
        this.channelOk = true;
      } catch (e) {
        // Canal absent → les envois retombent sur le canal « default »
        // du plugin (importance moindre, mais livré).
        logUi("warn", `notif: création du canal Android: ${e}`);
      }
    }
    this.ready = true;
  }

  /** Transition signalée par state-diff — gardes réglages + permission. */
  alert(kind: AlertKind, s: SessionMeta) {
    if (!this.ready || !notifEnabled() || !notifTypeOn(kind)) return;
    if (notifOnlyUnfocused() && document.hasFocus()) return;
    const key = `${s.conn ?? "default"}:${s.id}:${kind}`;
    const now = Date.now();
    if (now - (this.lastSent.get(key) ?? -COOLDOWN_MS) < COOLDOWN_MS) return;
    // Posé à la tentative : la garde tient pendant l'IPC et le prompt OS
    // de permission (deux transitions in-flight ne doublonnent pas), puis
    // relâché si l'envoi n'a pas eu lieu — le slot n'est consommé que par
    // une notif réellement partie.
    this.lastSent.set(key, now);
    void this.send(kind, s).then((sent) => {
      if (sent) {
        if (this.lastSent.size > 500) {
          for (const [k, ts] of this.lastSent) {
            if (now - ts > COOLDOWN_MS) this.lastSent.delete(k);
          }
        }
      } else if (this.lastSent.get(key) === now) {
        this.lastSent.delete(key);
      }
    });
  }

  /** « Tester » des réglages : envoi réel sans les gardes toggles/focus —
   *  geste explicite → la demande de permission est retentée même après
   *  un refus. sid vide → le clic focus seul, pas de session factice. */
  test() {
    void this.send(
      "done",
      { id: "", title: t("notif_test_title"), state: "idle" } as SessionMeta,
      true,
    );
  }

  /** Demande de permission — l'OS montre son prompt si elle n'est pas
   *  encore tranchée. Après un refus, seul un geste explicite (force)
   *  retente requestPermission : les alertes se contentent de re-sonder
   *  l'état (un déblocage côté OS est ainsi rattrapé sans re-prompt). */
  async ensurePermission(force = false): Promise<boolean> {
    if (this.granted === true) return true;
    try {
      if (await isPermissionGranted()) {
        this.granted = true;
        return true;
      }
      if (!force && this.granted === false) return false;
      this.granted = (await requestPermission()) === "granted";
    } catch (e) {
      logUi("warn", `notif: permission illisible: ${e}`);
      this.granted = false;
    }
    return this.granted;
  }

  /** Pour le hint des réglages : permission OS non accordée alors que le
   *  canal est utilisable (desktop/Android, jamais en web). */
  async permBlocked(): Promise<boolean> {
    if (isWeb) return false;
    try {
      return !(await isPermissionGranted());
    } catch {
      return false;
    }
  }

  /** Envoi effectif — true quand la notif est partie (permission accordée
   *  et IPC acceptée). */
  private async send(
    kind: AlertKind,
    s: SessionMeta,
    force = false,
  ): Promise<boolean> {
    if (!(await this.ensurePermission(force))) return false;
    const conn = s.conn ?? "";
    const where =
      conn && this.deps.connCount() > 1 ? ` — ${connLabel(conn)}` : "";
    const key = `${conn || "default"}:${s.id}:${kind}`;
    const id = this.ids.get(key)?.id ?? this.nextId++;
    const options: Options = {
      // Id posé nous-mêmes : remove_active ne dépend pas du retour de
      // notify ; sur Android le même id remplace la notif au lieu
      // d'empiler (le backend desktop envoie replaces_id=0 en dur).
      id,
      title: "AgentControl",
      body: t(EV_KEY[kind], { title: s.title || s.id }) + where,
      // Payload relue au clic pour rouvrir la bonne session — `kind`
      // sert aussi au balayage reconcile (conditions par type, #184).
      extra: { sid: s.id, conn, kind },
      autoCancel: true,
      group: `agentcontrol:${conn || "default"}`,
      ...(isAndroid() && this.channelOk ? { channelId: CHANNEL_ID } : {}),
    };
    // invoke direct plutôt que sendNotification() (fire-and-forget via le
    // shim window.Notification) : un échec de livraison remonte ici.
    try {
      await invoke("plugin:notification|notify", { options });
      this.ids.set(key, { id, conn: conn || "default", sid: s.id, kind });
      return true;
    } catch (e) {
      logUi("error", `notif: envoi impossible: ${e}`);
      return false;
    }
  }

  // --- Annulation à la résolution de la condition (#184) ---------------

  /** Retire des notifs OS par leurs ids — best-effort (une notif déjà
   *  partie/cliquée n'est pas une erreur). `remove_active` et non
   *  `cancel` : cancel ne vise que les notifs planifiées (jamais
   *  utilisées ici) et n'est pas enregistrée côté desktop ; remove_active
   *  matche l'id posé par nous dans le tracker du plugin puis émet
   *  CloseNotification en D-Bus (freedesktop) / removeActive (Android). */
  private async cancelIds(ids: number[]) {
    if (!ids.length) return;
    try {
      await invoke("plugin:notification|remove_active", {
        notifications: ids.map((id) => ({ id })),
      });
    } catch (e) {
      logUi("warn", `notif: cancel impossible: ${e}`);
    }
  }

  /** Retire la notif d'une clé suivie si émise par cette instance. */
  private cancelKey(key: string) {
    const e = this.ids.get(key);
    if (!e) return;
    this.ids.delete(key);
    void this.cancelIds([e.id]);
  }

  /** La session quitte `waiting_input` (question répondue dans l'app,
   *  permission résolue, état reparti) → la notif « question » n'a plus
   *  de raison de rester dans le shade. Appelé via onRawTransition. */
  questionResolved(s: SessionMeta) {
    this.cancelKey(`${s.conn ?? "default"}:${s.id}:question`);
  }

  /** L'utilisateur ouvre la session dans l'app (équivalent du clic sur
   *  la notif, sans le clic) → les notifs « done »/« error » sont
   *  vues — la « question » est conservée si encore waiting_input.
   *  Appelé depuis chat.open : tous les chemins d'ouverture (sidebar,
   *  backlog, map, lien peer, clic notif) y convergent. */
  sessionOpened(sid: string) {
    for (const [key, e] of [...this.ids]) {
      // sid unique — conn insensible (s-XXXXXXXX aléatoire par démon).
      if (e.sid === sid && (e.kind === "done" || e.kind === "error")) {
        this.cancelKey(key);
      }
    }
  }

  /** La session a disparu de la liste (rm/kill) — clé "conn:sid" :
   *  plus aucune notif ne la concerne. Appelé via onGone. */
  sessionGone(key: string) {
    for (const [k, e] of [...this.ids]) {
      if (`${e.conn}:${e.sid}` === key) this.cancelKey(k);
    }
  }

  /** Balayage de sécurité au refresh de la liste : retire les notifs
   *  suivies dont la condition a disparu (session rm, question sortie
   *  de waiting_input) puis réconcilie côté OS avec get_active — une
   *  notif émise par une instance précédente de l'app (index perdu)
   *  est reconnue par `extra.sid`/`conn` et soldée sur les mêmes
   *  règles. Jamais une notif sans `extra.sid` : elle appartient à
   *  d'autres usages de l'app. */
  async reconcile(list: SessionMeta[]) {
    if (!this.ready) return;
    const now = Date.now();
    if (now - this.lastReconcile < 30_000) return;
    this.lastReconcile = now;
    const find = (conn: string, sid: string) =>
      list.find((s) => s.id === sid && (s.conn ?? "default") === conn);
    // Règle de résolution par kind — retourne true si la notif doit
    // partir (condition résolue ou support disparu).
    const resolved = (conn: string, sid: string, kind: string | undefined) => {
      const s = find(conn, sid);
      if (!s) return true; // session disparue
      if (s.offline) return false; // état inconnu → conservatrice
      if (kind === "question") return s.state !== "waiting_input";
      return false; // done/error : soldées par clic / ouverture / rm
    };
    // Index local (instance courante) — composantes portées par
    // l'entrée, jamais re-parsées (conn est multi-segments).
    for (const [key, e] of [...this.ids]) {
      if (resolved(e.conn, e.sid, e.kind)) this.cancelKey(key);
    }
    // Balayage OS : notifs actives émises par nous (extra.sid présent)
    // dont l'index est perdu (restart app) — mêmes règles. Mobile-only :
    // freedesktop n'expose pas la liste des notifs actives (le rejet est
    // déjà attrapé — l'index local suffit côté desktop).
    try {
      const acts = await invoke<{ id?: number; extra?: { sid?: string; conn?: string; kind?: string } }[]>(
        "plugin:notification|get_active",
      );
      const ownIds = new Set([...this.ids.values()].map((e) => e.id));
      const stale: number[] = [];
      for (const a of acts ?? []) {
        if (typeof a.id !== "number" || ownIds.has(a.id)) continue;
        const sid = a.extra?.sid;
        if (typeof sid !== "string" || !sid) continue; // pas à nous
        const conn = a.extra?.conn ?? "default";
        if (resolved(conn || "default", sid, a.extra?.kind)) stale.push(a.id);
      }
      await this.cancelIds(stale);
    } catch {
      /* get_active indisponible sur la plateforme — l'index local suffit */
    }
  }

  /** Tap sur la notif : la fenêtre reprend le focus (dé-minimisée si
   *  besoin) puis la session s'ouvre quand la payload a survécu. */
  private async clicked(e: ActionPerformed) {
    try {
      const win = getCurrentWindow();
      if (await win.isMinimized()) await win.unminimize();
      await win.setFocus();
    } catch {
      /* Android / plateforme sans fenêtrage — l'OS a déjà rappelé l'app */
    }
    const extra = e.notification?.extra as
      | { sid?: unknown; conn?: unknown }
      | null
      | undefined;
    if (typeof extra?.sid === "string" && extra.sid) {
      this.deps.openSession(extra.sid, typeof extra.conn === "string" ? extra.conn : "");
    }
  }
}

export const notifications = new OsNotifications();
