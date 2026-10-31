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

import { isWeb } from "../core/bridge";
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
  sendNotification,
  type ActionPerformed,
} from "@tauri-apps/plugin-notification";

/** Canal Android (créé au boot, importance heads-up) — ignoré ailleurs. */
const CHANNEL_ID = "agent-status";

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
    void this.send(kind, s);
  }

  /** « Tester » des réglages : envoi réel sans les gardes toggles/focus —
   *  le geste déclenche aussi la demande de permission si nécessaire.
   *  sid vide → le clic focus seul, pas de session factice à ouvrir. */
  test() {
    void this.send("done", {
      id: "",
      title: t("notif_test_title"),
      state: "idle",
    } as SessionMeta);
  }

  /** Demande explicite (toggle maître activé, premier envoi) — l'OS montre
   *  son prompt si la permission n'est pas encore tranchée. */
  async ensurePermission(): Promise<boolean> {
    if (this.granted === true) return true;
    try {
      if (await isPermissionGranted()) {
        this.granted = true;
        return true;
      }
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

  private async send(kind: AlertKind, s: SessionMeta) {
    if (!(await this.ensurePermission())) return;
    const conn = s.conn ?? "";
    const where =
      conn && this.deps.connCount() > 1 ? ` — ${connLabel(conn)}` : "";
    try {
      sendNotification({
        title: "AgentControl",
        body: t(EV_KEY[kind], { title: s.title || s.id }) + where,
        // Payload relue au clic pour rouvrir la bonne session.
        extra: { sid: s.id, conn },
        autoCancel: true,
        group: `agentcontrol:${conn || "default"}`,
        ...(isAndroid() && this.channelOk ? { channelId: CHANNEL_ID } : {}),
      });
    } catch (e) {
      logUi("error", `notif: envoi impossible: ${e}`);
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
