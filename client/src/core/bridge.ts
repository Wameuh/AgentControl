// ---------------------------------------------------------------------------
// Pont d'exécution : Tauri (app desktop/Android) ou Web (SPA servie par le
// démon, AC#14). Tout accès plateforme des features passe par ici — un
// module n'importe JAMAIS @tauri-apps/* directement : le navigateur
// chargerait quand même le code, mais les commandes IPC seraient mortes —
// la sélection se fait UNE fois ici.
//
//   invoke(cmd, args)  → tauri ou webInvoke (table ci-dessous)
//   listen(ev, cb)     → events Tauri ou pushes WS (mêmes noms/payloads)
//   getVersion()       → app_version (identité compilée, suffixe -rc.K
//                        inclus sur les builds rc), ou version du démon
//                        en web
//
// Détection synchrone : __TAURI_INTERNALS__ est posé par le runtime Tauri
// avant tout script — fiable dès l'évaluation du module.
// ---------------------------------------------------------------------------

import { invoke as tauriInvoke, addPluginListener } from "@tauri-apps/api/core";
import { listen as tauriListen } from "@tauri-apps/api/event";
import { wsClient, WEB_CONN } from "./wsclient";

export const isWeb = !(window as { __TAURI_INTERNALS__?: unknown })
  .__TAURI_INTERNALS__;

/** Version "plateforme" rapportée en web — UA-based : le chrome mobile
 *  doit s'appliquer aussi dans un navigateur de téléphone. */
function webPlatform(): string {
  const ua = navigator.userAgent;
  if (/Android/i.test(ua)) return "android";
  if (/Windows/i.test(ua)) return "windows";
  return "web";
}

export async function invoke<T>(
  cmd: string,
  args?: Record<string, unknown>,
): Promise<T> {
  if (!isWeb) return tauriInvoke<T>(cmd, args as never);
  return webInvoke<T>(cmd, args ?? {});
}

/** `listen` : même contrat que Tauri — cb reçoit {payload}, renvoie un
 *  unlisten. En web : pushes WS relayés sous les mêmes noms. */
export function listen<T>(
  event: string,
  cb: (e: { payload: T }) => void,
): Promise<() => void> {
  if (!isWeb) return tauriListen<T>(event, cb) as Promise<() => void>;
  return Promise.resolve(wsClient.on(event, (p) => cb({ payload: p as T })));
}

/** `listenPlugin` : events d'un plugin mobile (Kotlin `trigger` →
 *  addPluginListener/channel). En web : pas de plugin → unlisten no-op
 *  (les surfaces concernées sont masquées de toute façon). */
export function listenPlugin<T>(
  plugin: string,
  event: string,
  cb: (payload: T) => void,
): Promise<() => void> {
  if (isWeb) return Promise.resolve(() => {});
  return addPluginListener<T>(plugin, event, cb)
    .then((l) => () => void l.unregister())
    .catch(() => () => {});
}

/** Version de l'app : Tauri → `app_version` (identité compilée — le
 *  suffixe -rc.K des builds rc ne peut pas entrer dans la version
 *  bundle MSI/plist, il vit dans le binaire) ; web → version du démon
 *  qui sert la page (le « client » web est livré par lui — la
 *  cohérence de version affichée suit le serveur). */
export async function getVersion(): Promise<string> {
  if (!isWeb) return invoke<string>("app_version");
  const st = await wsClient.call<Record<string, unknown>>("daemon_status", {
    conn: WEB_CONN,
  });
  return String(st?.version ?? "0.0.0");
}

// ---------------------------------------------------------------------------
// Commandes Tauri → équivalents web. Les commandes sans sens hors Tauri
// (install démon, clés SSH, update de l'app) échouent proprement — elles
// ne sont de toute façon pas atteignables : l'UI qui les déclenche est
// masquée en mode web (body.web).
// ---------------------------------------------------------------------------
async function webInvoke<T>(cmd: string, a: Record<string, unknown>): Promise<T> {
  switch (cmd) {
    case "call":
      return wsClient.call<T>(
        String(a.method),
        (a.params as Record<string, unknown>) ?? {},
      );
    case "subscribe":
      return wsClient.call<T>("subscribe", {
        session: a.session,
        after_seq: a.afterSeq ?? 0,
        conn: a.conn,
      });
    case "connect":
      return wsClient.connect() as Promise<T>;
    case "disconnect":
      wsClient.close();
      return undefined as T;
    case "saved_connections":
      // Une seule connexion : le démon qui sert la page. autoConnect()
      // l'ouvre au boot ; le dialogue de connexion reste caché en web.
      return [{ kind: "web" }] as T;
    case "save_connection":
    case "remove_connection":
      return undefined as T;
    case "ssh_hosts":
      return [] as T;
    case "ssh_pubkey":
      return "" as T;
    case "platform":
      return webPlatform() as T;
    case "sys_stats": {
      const r = wsClient.rates();
      return { rss_mb: 0, cpu: 0, rx_bps: r.rx_bps, tx_bps: r.tx_bps } as T;
    }
    case "log_tail":
      return "" as T;
    case "log_write":
      console.debug("[ui]", a.msg);
      return undefined as T;
    case "update_last_status":
      return null as T;
    case "set_app_icon":
      return undefined as T;
    case "plugin:opener|open_url":
      window.open(String(a.url), "_blank", "noopener");
      return undefined as T;
    case "plugin:ac-stt|stt_ping":
      // Spike ac-stt : le ping du squelette — pas de plugin Kotlin en
      // web, la forme de la réponse reste celle du plugin.
      return { pong: true, platform: "web" } as T;
    default:
      // check_update / update_client / ensure_daemon / write_download :
      // absents du monde navigateur — la surface UI qui les invoque est
      // désactivée (body.web) ; un appel direct échoue verbalement.
      throw new Error(`${cmd} indisponible en mode web`);
  }
}
