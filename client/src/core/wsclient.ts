// ---------------------------------------------------------------------------
// Transport web : une WebSocket = une connexion démon. Le navigateur parle
// le protocole NDJSON exact des sockets TCP/Unix, chaque ligne JSON dans
// une frame texte — pendant navigateur de client_core::Client.
//
//   {id, req: "<méthode>", ...params}   → requête (idem ReqKind)
//   {id, ok, result|error}              ← réponse, routée par id
//   {push: "event"|"term_data"|"term_exit", …} ← relayé aux listeners
//
// La clé de connexion unique en mode web est "web" — l'équivalent mono
// serveur des clés ssh/tcp du client desktop (list_sessions/daemon_status
// sont émulés dans leur forme fusionnée pour réutiliser le watchdog).
// ---------------------------------------------------------------------------

import { t } from "../i18n";
import { $, esc } from "./dom";

type Pending = {
  resolve: (v: unknown) => void;
  reject: (e: Error) => void;
  timer: ReturnType<typeof setTimeout>;
};
type Listener = (payload: unknown) => void;

const CALL_TIMEOUT = 60_000;
const TOKEN_KEY = "ac-web-token";
/** Clé de conn unique du mode web (connKeyOf({kind:"web"}) côté client). */
export const WEB_CONN = "web";

class WsClient {
  private ws: WebSocket | null = null;
  private seq = 1;
  private pending = new Map<number, Pending>();
  private handlers = new Map<string, Set<Listener>>();
  private connectPromise: Promise<Record<string, unknown>> | null = null;
  private rxTotal = 0;
  private txTotal = 0;
  private lastNet = { at: 0, rx: 0, tx: 0 };
  /** Promesse de token partagée : une seule modale à la fois. */
  private authGate: Promise<string> | null = null;
  private authResolve: ((t: string) => void) | null = null;

  // -- Token -----------------------------------------------------------------

  /** Token courant : `?token=` d'URL (nettoyé de la barre) puis storage. */
  token(): string | null {
    const url = new URL(location.href);
    const fromUrl = url.searchParams.get("token");
    if (fromUrl) {
      sessionStorage.setItem(TOKEN_KEY, fromUrl);
      url.searchParams.delete("token");
      // Le token quitte l'URL visible : historique, captures, partage.
      history.replaceState(null, "", url);
      return fromUrl;
    }
    return sessionStorage.getItem(TOKEN_KEY);
  }

  /** Token garanti : storage, sinon modale de saisie (validée sur /auth). */
  private ensureToken(): Promise<string> {
    const tok = this.token();
    if (tok) return Promise.resolve(tok);
    if (!this.authGate) {
      this.authGate = new Promise<string>((res) => (this.authResolve = res));
      this.showAuthOverlay();
    }
    return this.authGate;
  }

  /** Vérifie un token auprès du démon (`/auth?token=` → 204). Erreur
   *  réseau → `true` optimiste : la WS échouera d'elle-même si le démon
   *  est down, inutile de bloquer la saisie sur l'état réseau. */
  private async checkToken(tok: string): Promise<boolean> {
    try {
      const r = await fetch(`/auth?token=${encodeURIComponent(tok)}`);
      return r.status !== 401;
    } catch {
      return true;
    }
  }

  /** Après un échec de connexion : re-vérifie le token stocké — 401 →
   *  oubli + ré-affichage de la modale à la prochaine tentative. */
  private async reverifyToken() {
    const tok = sessionStorage.getItem(TOKEN_KEY);
    if (!tok) return;
    if (!(await this.checkToken(tok))) {
      sessionStorage.removeItem(TOKEN_KEY);
      this.authGate = null;
    }
  }

  private submitToken(tok: string) {
    const done = (ok: boolean) => {
      if (!ok) {
        const err = $("wa-err");
        err.textContent = t("web_token_err");
        err.hidden = false;
        return;
      }
      sessionStorage.setItem(TOKEN_KEY, tok);
      const gate = this.authResolve;
      $("web-auth")!.hidden = true;
      // La prochaine invalidation recréera une promesse + modale.
      this.authResolve = null;
      gate?.(tok);
    };
    void this.checkToken(tok).then(done);
  }

  /** Modale de saisie du token — injectée à la demande (i18n déjà prêt,
   *  le boot l'appelle après initLang). */
  private showAuthOverlay() {
    let ov = $("web-auth") as HTMLElement | null;
    if (!ov) {
      ov = document.createElement("div");
      ov.id = "web-auth";
      ov.innerHTML = `
        <div class="wa-card">
          <div class="wa-title">${esc(t("web_token_h"))}</div>
          <div class="wa-msg">${esc(t("web_token_msg"))}</div>
          <input id="wa-input" class="wa-input" type="password" autocomplete="off"
                 placeholder="${esc(t("web_token_ph"))}" />
          <div class="wa-err" id="wa-err" hidden></div>
          <button id="wa-go" class="wa-go">${esc(t("web_token_go"))}</button>
        </div>`;
      document.body.appendChild(ov);
      const go = () => {
        const inp = $("wa-input") as HTMLInputElement;
        if (inp.value.trim()) this.submitToken(inp.value.trim());
      };
      ov.querySelector("#wa-go")!.addEventListener("click", go);
      ov.querySelector("#wa-input")!.addEventListener("keydown", (e) => {
        if ((e as KeyboardEvent).key === "Enter") go();
      });
    }
    ov.hidden = false;
    (ov.querySelector("#wa-input") as HTMLInputElement).focus();
  }

  // -- Connexion --------------------------------------------------------------

  /** Établit le transport (équivalent `invoke("connect")` desktop :
   *  renvoie le daemon_status pour les capabilities). Idempotent — un
   *  appel en vol est partagé par tous les demandeurs. */
  connect(): Promise<Record<string, unknown>> {
    this.connectPromise ??= this.doConnect().finally(
      () => (this.connectPromise = null),
    );
    return this.connectPromise;
  }

  private async doConnect(): Promise<Record<string, unknown>> {
    if (this.ws?.readyState === WebSocket.OPEN) {
      return this.rawCall("daemon_status", {}) as Promise<Record<string, unknown>>;
    }
    const tok = await this.ensureToken();
    await this.open(tok);
    return this.rawCall("daemon_status", {}) as Promise<Record<string, unknown>>;
  }

  /** Ferme le transport (`disconnect`) — les requêtes en vol sont
   *  rejetées, la conn reste marquée down jusqu'au prochain connect. */
  close() {
    const ws = this.ws;
    this.ws = null;
    if (ws) {
      ws.onclose = null;
      ws.onmessage = null;
      try {
        ws.close();
      } catch {
        /* déjà morte */
      }
    }
    this.failAll(new Error(t("conn_offline")));
  }

  private open(tok: string): Promise<void> {
    return new Promise((resolve, reject) => {
      const proto = location.protocol === "https:" ? "wss" : "ws";
      const ws = new WebSocket(
        `${proto}://${location.host}/ws?token=${encodeURIComponent(tok)}`,
      );
      this.ws = ws;
      let opened = false;
      ws.onopen = () => {
        opened = true;
        resolve();
      };
      ws.onmessage = (m) => this.onMessage(m);
      ws.onclose = () => {
        this.onDead(ws);
        if (!opened) {
          // Refus d'auth probable → re-vérifier le token pour la suite.
          void this.reverifyToken();
          reject(new Error(t("conn_offline")));
        }
      };
      ws.onerror = () => {
        // onclose suit systématiquement — là que reject/purge arrivent.
      };
    });
  }

  /** Socket morte : rejette les requêtes en vol, prévient le manager
   *  (même sémantique que `conn-dead` du bridge Tauri → backoff retry). */
  private onDead(ws: WebSocket) {
    if (ws !== this.ws) return;
    this.ws = null;
    this.failAll(new Error(t("conn_offline")));
    this.emit("conn-dead", WEB_CONN);
    void this.reverifyToken();
  }

  private failAll(e: Error) {
    for (const [, p] of this.pending) {
      clearTimeout(p.timer);
      p.reject(e);
    }
    this.pending.clear();
  }

  // -- RPC --------------------------------------------------------------------

  /** Appel démon — file derrière la connexion si elle n'est pas prête. */
  async call<T = unknown>(
    method: string,
    params: Record<string, unknown> = {},
  ): Promise<T> {
    // conn est un routage multi-serveurs desktop — un seul démon ici.
    const p = { ...params };
    const conn = p.conn;
    delete p.conn;
    // Formes fusionnées du bridge Tauri, émulées en mono-serveur :
    if (method === "daemon_status" && conn === undefined) {
      try {
        const v = await this.callInner<Record<string, unknown>>(method, p);
        return { [WEB_CONN]: v, servers: 1, sessions: 1 } as T;
      } catch (e) {
        return {
          [WEB_CONN]: { error: String(e) },
          servers: 1,
          sessions: 0,
        } as T;
      }
    }
    if ((method === "list_sessions" || method === "pr_list") && conn === undefined) {
      try {
        const arr = await this.callInner<Record<string, unknown>[]>(method, p);
        return arr.map((x) => ({ ...x, conn: WEB_CONN })) as T;
      } catch (e) {
        console.error(`[ws] ${method}:`, e);
        return [{ conn: WEB_CONN, conn_error: true }] as T;
      }
    }
    return this.callInner<T>(method, p);
  }

  private async callInner<T>(method: string, params: Record<string, unknown>): Promise<T> {
    if (this.ws?.readyState !== WebSocket.OPEN) await this.connect();
    return this.rawCall<T>(method, params);
  }

  private rawCall<T>(method: string, params: Record<string, unknown>): Promise<T> {
    const ws = this.ws;
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      return Promise.reject(new Error(t("conn_offline")));
    }
    const id = this.seq++;
    const body = JSON.stringify({ id, req: method, ...params });
    const p = new Promise<T>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error("timeout démon"));
      }, CALL_TIMEOUT);
      this.pending.set(id, {
        resolve: resolve as (v: unknown) => void,
        reject,
        timer,
      });
    });
    this.txTotal += body.length;
    ws.send(body);
    return p;
  }

  private onMessage(m: MessageEvent) {
    const data = typeof m.data === "string" ? m.data : "";
    this.rxTotal += data.length;
    let v: Record<string, unknown>;
    try {
      v = JSON.parse(data);
    } catch {
      return;
    }
    if (v.push) {
      // Push démon → nom d'event Tauri équivalent (le payload conserve
      // la forme brute : {session, event} / {term, data} / {term, code}).
      const ev =
        v.push === "event"
          ? "session-event"
          : v.push === "term_data"
            ? "term-data"
            : v.push === "term_exit"
              ? "term-exit"
              : null;
      if (ev) this.emit(ev, v);
      return;
    }
    const id = v.id as number;
    const p = this.pending.get(id);
    if (!p) return;
    this.pending.delete(id);
    clearTimeout(p.timer);
    if (v.ok) p.resolve(v.result ?? null);
    else p.reject(new Error(String(v.error ?? "?")));
  }

  /** Listener d'event (mêmes noms/payloads que les events Tauri). */
  on(name: string, cb: Listener): () => void {
    let set = this.handlers.get(name);
    if (!set) this.handlers.set(name, (set = new Set()));
    set.add(cb);
    return () => set.delete(cb);
  }

  private emit(name: string, payload: unknown) {
    for (const cb of this.handlers.get(name) ?? []) {
      try {
        cb(payload);
      } catch (e) {
        console.error(`[ws] listener ${name}:`, e);
      }
    }
  }

  // -- Statistiques lien (barre d'état — équivalent de sys_stats) --------------

  /** Débits rx/tx depuis le dernier appel (sondé périodiquement). */
  rates(): { rx_bps: number; tx_bps: number } {
    const now = Date.now();
    const dt = Math.max(1, (now - this.lastNet.at) / 1000);
    const r = {
      rx_bps: (this.rxTotal - this.lastNet.rx) / dt,
      tx_bps: (this.txTotal - this.lastNet.tx) / dt,
    };
    this.lastNet = { at: now, rx: this.rxTotal, tx: this.txTotal };
    return r;
  }
}

/** Singleton — une seule connexion démon par onglet (le multi-serveur
 *  n'existe pas en mode web : le démon qui sert la page EST le démon). */
export const wsClient = new WsClient();
