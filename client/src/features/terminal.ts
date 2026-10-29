// ---------------------------------------------------------------------------
// Terminal PTY : shell sur l'hôte du démon, cwd = workspace de la session.
// Panneau bas de la vue conversation — un shell par session, conservé en
// mémoire (détaché/réattaché au changement de session, scrollback intact).
// Flux : pushes daemon term_data/term_exit relayés par la pompe du bridge
// (call term_open) en events Tauri "term-data" / "term-exit".
// ---------------------------------------------------------------------------

import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import { listen } from "../core/bridge";
import { $, logUi, showErr } from "../core/dom";
import { call } from "../core/rpc";
import { app } from "../core/state";
import { isAndroid } from "../core/chrome";
import { t } from "../i18n";
import { nav } from "../ui/nav";

interface TermEntry {
  sid: string;
  conn: string;
  /** Id démon (t-xxxx) — null tant que term_open n'a pas répondu. */
  termId: string | null;
  /** Id choisi par le client et envoyé dans term_open : le démon récent
   *  le reprend tel quel → un open abandonné (timeout, conn coupée après
   *  spawn) reste fermable par term_close au lieu de fuiter en orphelin.
   *  Vieux démon : ignoré, l'id rendu est celui du serveur. */
  wantId: string;
  xterm: Terminal;
  fit: FitAddon;
  /** Conteneur du xterm — détaché/réattaché dans #term-box au switch. */
  box: HTMLElement;
  exited: boolean;
  /** Frappes reçues pendant "connecting…" — flushées à l'ouverture. */
  pending: string[];
}

function b64enc(u8: Uint8Array): string {
  let s = "";
  for (let i = 0; i < u8.length; i += 8192)
    s += String.fromCharCode(...u8.subarray(i, i + 8192));
  return btoa(s);
}

const b64dec = (s: string) => Uint8Array.from(atob(s), (c) => c.charCodeAt(0));

/** "default" = sentinelle de regroupement frontend — la passerelle attend
 *  conn absent/"" pour la connexion par défaut, pas la chaîne littérale. */
const apiConn = (c: string) => (c === "default" ? undefined : c);

/** Thème xterm calé sur les tokens courants (sombre/clair). */
function termTheme() {
  const v = (n: string) =>
    getComputedStyle(document.documentElement).getPropertyValue(n).trim();
  return {
    background: v("--panel-solid") || "#161c18",
    foreground: v("--ink") || "#ece6d8",
    cursor: v("--accent") || "#cdb68c",
    cursorAccent: v("--panel-solid") || "#161c18",
    selectionBackground: v("--line-strong") || "#555",
    black: "#1a1d16",
    red: v("--red") || "#e89486",
    green: v("--sage") || "#b8cfae",
    yellow: v("--gold") || "#e0c996",
    blue: "#8fb4d9",
    magenta: v("--violet") || "#c4a9dc",
    cyan: "#93c6ba",
    white: v("--ink") || "#ece6d8",
    brightBlack: v("--dim") || "#8f9689",
    brightRed: "#f2a89c",
    brightGreen: "#c8ddbe",
    brightYellow: "#ecd8a8",
    brightBlue: "#a5c4e4",
    brightMagenta: "#d3bde8",
    brightCyan: "#a8d4c8",
    brightWhite: "#f5f1e6",
  };
}

export class TermPanel {
  private terms = new Map<string, TermEntry>(); // "conn|session" → état
  /** Le panneau reste ouvert d'une session à l'autre : masqué seulement
   *  pour les sessions sans terminal. */
  private wantOpen = false;
  private capable: (conn: string) => boolean = () => false;
  /** Résout la sentinelle "default" → clé de conn réelle (mono-conn). */
  private defaultConn: () => string | undefined = () => undefined;

  bind(d: {
    termCapable: (conn: string) => boolean;
    defaultConn: () => string | undefined;
  }) {
    this.capable = d.termCapable;
    this.defaultConn = d.defaultConn;
  }

  private key(sid: string, conn: string) {
    return `${conn}|${sid}`;
  }
  private entryFor(sid: string, conn: string) {
    return this.terms.get(this.key(sid, conn));
  }
  private currentEntry() {
    const s = app.current;
    return s ? this.entryFor(s.id, s.conn ?? "default") : undefined;
  }
  private byTermId(tid: string) {
    // termId confirmé OU wantId : un démon récent émet term_data sous
    // l'id client dès le spawn — avant que la réponse term_open ne
    // pose termId. Sans ce fallback ces premiers octets tomberaient.
    for (const e of this.terms.values())
      if (e.termId === tid || e.wantId === tid) return e;
    return undefined;
  }

  /** ⌨ : le démon de la session sait-il faire term_open ? */
  supported(): boolean {
    const s = app.current;
    return !!s && this.capable(s.conn ?? "default");
  }

  /** Bouton ⌨ / entrée du menu ⋮ : ouvre ou ferme le panneau. */
  toggle() {
    const s = app.current;
    if (!s) return;
    // L'état réel du panneau fait foi : wantOpen survit aux switchs vers
    // une session sans terminal (panneau masqué) — se fier à lui ici
    // rendrait le premier clic sans effet visible.
    if ($("term-panel").hidden) {
      if (!this.supported()) {
        showErr(t("term_unsupported"));
        return;
      }
      this.wantOpen = true;
      $("term-panel").hidden = false;
      nav.push("term");
      void this.ensure();
    } else {
      this.hide();
    }
  }

  /** Masque le panneau (le shell continue de tourner côté démon). */
  hide() {
    this.wantOpen = false;
    $("term-panel").hidden = true;
    nav.close("term");
  }

  /** Changement de session affichée : le panneau suit la session —
   *  visible seulement si elle a un terminal vivant ou en cours. */
  onSessionSwitched() {
    const e = this.currentEntry();
    const panel = $("term-panel");
    if (this.wantOpen && e && app.current) {
      this.mount(e);
    } else {
      panel.hidden = true;
    }
  }

  /** Session supprimée : ferme son shell + le panneau s'il l'affichait. */
  dropSession(sid: string) {
    for (const [k, e] of this.terms) {
      if (e.sid !== sid) continue;
      // termId ?? wantId : un open encore en vol n'a pas de termId —
      // close quand même sous l'id client (le démon le reprend si le
      // spawn aboutit après : pending_close le tuera à l'insertion).
      const tid = e.termId ?? e.wantId;
      if (tid) {
        void call("term_close", { term: tid, conn: apiConn(e.conn) })
          .catch(() => {});
      }
      e.xterm.dispose();
      e.box.remove();
      this.terms.delete(k);
    }
    if (!this.currentEntry()) $("term-panel").hidden = true;
  }

  /** Connexion morte : les PTY meurent avec — terminaux marqués morts,
   *  rouvrables via ↻ (le spawn passe par la conn reconnectée). */
  markConnDead(conn: string) {
    const def = this.defaultConn();
    for (const e of this.terms.values()) {
      // "default" = conn par défaut côté démon — résoluble en clé réelle
      // qu'en mono-connexion (multi-serveurs : edge ignoré).
      const mine = e.conn === conn || (e.conn === "default" && def === conn);
      if (!mine || e.exited) continue;
      e.exited = true;
      e.termId = null;
      e.xterm.write(`\r\n\x1b[31m${t("term_conn_lost")}\x1b[0m\r\n`);
    }
  }

  /** Ré-applique les couleurs du thème courant aux terminaux ouverts. */
  refreshTheme() {
    for (const e of this.terms.values()) e.xterm.options.theme = termTheme();
  }

  /** Monte l'entrée de la session courante dans le panneau + fit. */
  private mount(e: TermEntry) {
    const box = $("term-box");
    // replaceChildren retire les boxes des autres sessions : sans ça elles
    // s'empilent dans #term-box et seule la première reste visible — le
    // terminal affiché ne suivrait plus la session courante.
    box.replaceChildren(e.box);
    $("term-panel").hidden = false;
    $("term-cwd").textContent = app.current?.cwd ?? "";
    requestAnimationFrame(() => {
      e.fit.fit();
      e.xterm.focus();
    });
  }

  /** Ouvre (ou rouvre après exit) le shell de la session courante. */
  private async ensure() {
    const s = app.current;
    if (!s) return;
    const key = this.key(s.id, s.conn ?? "default");
    const old = this.terms.get(key);
    if (old) {
      // Shell encore vivant → remonter l'existant ; sinon nettoyer +
      // respawn (restart explicite ou réouverture après exit).
      if (!old.exited && old.termId) {
        this.mount(old);
        return;
      }
      // wantId en fallback : un open encore en vol est fermé sous l'id
      // client — le démon tue le PTY à son insertion (pending_close).
      const tid = old.termId ?? old.wantId;
      if (tid) {
        void call("term_close", { term: tid, conn: apiConn(old.conn) })
          .catch(() => {});
      }
      old.xterm.dispose();
      old.box.remove();
      this.terms.delete(key);
    }

    const box = document.createElement("div");
    box.className = "term-inst";
    $("term-box").appendChild(box);
    const xterm = new Terminal({
      theme: termTheme(),
      fontSize: 13,
      cursorBlink: true,
      scrollback: 5000,
      allowProposedApi: true,
    });
    const fit = new FitAddon();
    xterm.loadAddon(fit);
    xterm.open(box);
    const entry: TermEntry = {
      sid: s.id,
      conn: s.conn ?? "default",
      termId: null,
      wantId: `t-ui-${Math.random().toString(36).slice(2, 10)}`,
      xterm,
      fit,
      box,
      exited: false,
      pending: [],
    };
    this.terms.set(key, entry);
    requestAnimationFrame(() => {
      fit.fit();
      xterm.focus();
    });
    xterm.writeln(`\x1b[2m${t("term_connecting")}\x1b[0m`);

    // Handlers posés AVANT term_open : les frappes pendant "connecting…"
    // sont bufferisées (bornées) puis flushées à l'ouverture — sans ça
    // elles tombaient dans le vide (pas de onData enregistré).
    const enc = new TextEncoder();
    xterm.onData((d) => {
      if (entry.exited) return;
      if (!entry.termId) {
        if (entry.pending.length < 256) entry.pending.push(d);
        return;
      }
      void call("term_input", {
        term: entry.termId,
        conn: apiConn(entry.conn), // pas de session → injecté via params
        data: b64enc(enc.encode(d)),
      }).catch(() => {});
    });
    xterm.onResize(({ cols, rows }) => {
      if (!entry.termId || entry.exited) return;
      void call("term_resize", {
        term: entry.termId,
        conn: apiConn(entry.conn),
        cols,
        rows,
      }).catch(() => {});
    });

    const dims = fit.proposeDimensions();
    // Liaison lente (tunnel SSH distant) : term_open peut prendre des
    // secondes sans que rien ne bouge — le hint évite l'écran "mort".
    const slowHint = setTimeout(() => {
      // terms.get(key) : l'entrée a pu être remplacée/supprimée pendant
      // l'attente (restart, dropSession) — writeln sur un xterm disposé.
      if (this.terms.get(key) === entry && !entry.termId && !entry.exited)
        xterm.writeln(`\x1b[2m${t("term_slow")}\x1b[0m`);
    }, 6000);
    const t0 = performance.now();
    try {
      const r = await call<{ term: string; cwd: string }>("term_open", {
        session: s.id,
        conn: apiConn(s.conn ?? "default"),
        term: entry.wantId,
        cols: dims?.cols ?? 120,
        rows: dims?.rows ?? 30,
      });
      clearTimeout(slowHint);
      logUi("debug", `term_open ${s.id}: ${Math.round(performance.now() - t0)} ms`);
      // Entrée supprimée/remplacée pendant l'ouverture (session_deleted,
      // restart, second ensure) : le shell démon n'a plus de référence
      // client — term_close sinon il fuiterait jusqu'à la déconnexion.
      if (this.terms.get(key) !== entry) {
        void call("term_close", { term: r.term, conn: apiConn(entry.conn) })
          .catch(() => {});
        return;
      }
      entry.termId = r.term;
      $("term-cwd").textContent = r.cwd;
      $("term-cwd").title = r.cwd;
      // La taille a pu changer pendant l'ouverture (fit post-montage).
      const d2 = fit.proposeDimensions();
      if (d2 && (d2.cols !== dims?.cols || d2.rows !== dims?.rows)) {
        void call("term_resize", {
          term: r.term,
          conn: apiConn(entry.conn),
          cols: d2.cols,
          rows: d2.rows,
        }).catch(() => {});
      }
      for (const d of entry.pending.splice(0)) {
        void call("term_input", {
          term: r.term,
          conn: apiConn(entry.conn),
          data: b64enc(enc.encode(d)),
        }).catch(() => {});
      }
      xterm.focus();
    } catch (err) {
      clearTimeout(slowHint);
      logUi(
        "error",
        `term_open ${s.id}: ${err} (${Math.round(performance.now() - t0)} ms)`,
      );
      entry.exited = true;
      xterm.writeln(`\x1b[31m${String(err)}\x1b[0m`);
      // L'échec/timeout peut avoir eu lieu APRÈS le spawn démon (réponse
      // perdue, conn coupée) : le PTY tourne sans référence client →
      // close sous l'id client. No-op sur un vieux démon (id ignoré).
      void call("term_close", {
        term: entry.wantId,
        conn: apiConn(entry.conn),
      }).catch(() => {});
    }
  }

  /** ↻ dans le header du panneau : shell neuf dans le même workspace. */
  private restart() {
    const e = this.currentEntry();
    if (!e) return;
    e.exited = true; // ensure() nettoie + respawn
    void this.ensure();
  }

  init() {
    $("btn-term").onclick = () => this.toggle();
    $("btn-term-hide").onclick = () => this.hide();
    $("btn-term-restart").onclick = () => this.restart();

    void listen<{ term: string; data: string }>("term-data", (m) => {
      const e = this.byTermId(m.payload.term);
      if (e) e.xterm.write(b64dec(m.payload.data));
    });
    void listen<{ term: string; code: number | null }>("term-exit", (m) => {
      const e = this.byTermId(m.payload.term);
      if (!e || e.exited) return;
      e.exited = true;
      e.termId = null;
      e.xterm.write(
        `\r\n\x1b[2m${t("term_exited", { code: m.payload.code ?? "?" })}\x1b[0m\r\n`,
      );
    });

    // Resize fenêtre → refit si le panneau est visible.
    window.addEventListener("resize", () => {
      if (!$("term-panel").hidden) this.currentEntry()?.fit.fit();
    });
    if (!isAndroid()) {
      // Poignée haute du panneau : hauteur réglable, persistée.
      const panel = $("term-panel");
      const hd = $("term-resize");
      hd.addEventListener("mousedown", (e) => {
        e.preventDefault();
        const move = (ev: MouseEvent) => {
          // Bouton relâché hors fenêtre → mouseup perdu : auto-relâcher.
          if (ev.buttons === 0) {
            up();
            return;
          }
          const h = Math.min(
            Math.max(window.innerHeight - ev.clientY, 120),
            Math.round(window.innerHeight * 0.8),
          );
          panel.style.height = `${h}px`;
          localStorage.setItem("ac-termpanel-h", String(h));
        };
        const up = () => {
          window.removeEventListener("mousemove", move);
          window.removeEventListener("mouseup", up);
          this.currentEntry()?.fit.fit();
        };
        window.addEventListener("mousemove", move);
        window.addEventListener("mouseup", up);
      });
      const saved = Number(localStorage.getItem("ac-termpanel-h"));
      if (saved > 120) panel.style.height = `${saved}px`;
    }
  }
}
