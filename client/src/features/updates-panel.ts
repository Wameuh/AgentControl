// ---------------------------------------------------------------------------
// Fenêtre « Mises à jour » : point unique pour l'app ET le démon de la
// connexion liée — versions avant → après, bouton par cible, canal
// stable / pré-versions, notes de version. Ouverte depuis la pastille de
// version de la titlebar, le menu Options, Paramètres → Général et À
// propos (qui n'affiche plus que les versions). Les chemins d'install
// restent ceux de ./update.ts (performClientUpdate / performDaemonUpdate).
// ---------------------------------------------------------------------------

import { getVersion, isWeb } from "../core/bridge";
import { $ } from "../core/dom";
import { isAndroid, shellOpen } from "../core/chrome";
import { cmpVer, connLabel } from "../core/conn";
import { call } from "../core/rpc";
import { t } from "../i18n";
import { nav } from "../ui/nav";
import {
  daemonCfgFor,
  performClientUpdate,
  performDaemonUpdate,
  rcChannel,
  remoteVersion,
} from "./update";

const RELEASES = "https://github.com/Wameuh/AgentControl/releases";

export class UpdatesPanel {
  /** Dernière release connue (lien « Notes de version »). */
  private remote = "";
  /** Jeton de rafraîchissement : un check plus récent invalide l'ancien. */
  private gen = 0;

  constructor(private connKey: () => string | null) {}

  private pop() {
    return $("upd-pop");
  }

  isOpen() {
    return !this.pop().hidden;
  }

  init() {
    $("upd-close").onclick = () => this.close();
    $("upd-check").onclick = () => void this.refresh();
    $("upd-notes").onclick = () =>
      shellOpen(this.remote ? `${RELEASES}/tag/v${this.remote}` : RELEASES);
    $("upd-chan").addEventListener("click", (e) => {
      const b = (e.target as HTMLElement).closest<HTMLButtonElement>("button[data-v]");
      if (!b) return;
      // Action directe (pas de staging ici) : le canal change la version
      // proposée — re-vérification immédiate.
      localStorage.setItem("ac-rcchannel", b.dataset.v === "rc" ? "1" : "0");
      this.syncChannel();
      void this.refresh();
    });
    // Clic hors de la fenêtre / Échap : fermeture (pas de modal — la
    // fenêtre reste un panneau ancré, comme un menu).
    document.addEventListener("mousedown", (e) => {
      if (!this.isOpen()) return;
      const tgt = e.target as Node;
      if (this.pop().contains(tgt)) return;
      if ((tgt as HTMLElement).closest?.(".tb-update, #tb-options, #tb-menu-pop")) return;
      this.close();
    });
    window.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && this.isOpen() && !document.querySelector("dialog[open]")) {
        e.stopPropagation();
        this.close();
      }
    }, true);
  }

  /** Ouvre la fenêtre, ancrée sous `anchor` (aligné à droite) ou en
   *  haut à droite de la fenêtre. Un second appel la referme. */
  open(anchor?: HTMLElement | null) {
    const pop = this.pop();
    if (this.isOpen()) {
      this.close();
      return;
    }
    pop.hidden = false;
    nav.push("updates");
    if (!isAndroid()) {
      const r = anchor?.getBoundingClientRect();
      const w = pop.getBoundingClientRect().width;
      const right = r ? r.right : window.innerWidth - 12;
      pop.style.left = `${Math.max(8, Math.min(right - w, window.innerWidth - w - 8))}px`;
      pop.style.top = `${(r ? r.bottom : 34) + 6}px`;
    }
    this.syncChannel();
    void this.refresh();
    ($("upd-check") as HTMLButtonElement).focus({ preventScroll: true });
  }

  close() {
    if (!this.isOpen()) return;
    this.pop().hidden = true;
    nav.close("updates");
  }

  private syncChannel() {
    const rc = rcChannel();
    for (const b of $("upd-chan").querySelectorAll<HTMLButtonElement>("button[data-v]")) {
      const on = (b.dataset.v === "rc") === rc;
      b.setAttribute("aria-checked", on ? "true" : "false");
    }
  }

  /** Versions locale, distante et du démon lié → lignes + boutons.
   *  Une ligne en échec affiche « — » : jamais d'état bloquant. */
  async refresh() {
    const my = ++this.gen;
    const status = $("upd-status");
    const appGo = $("upd-app-go") as HTMLButtonElement;
    const dGo = $("upd-daemon-go") as HTMLButtonElement;
    $("upd-app-row").hidden = isWeb;
    appGo.hidden = true;
    dGo.hidden = true;
    status.textContent = t("update_checking");
    status.classList.remove("ok", "err");
    const ck = this.connKey();
    const [lv, remote, dst] = await Promise.all([
      isWeb ? Promise.resolve("") : getVersion().catch(() => ""),
      remoteVersion().catch(() => ""),
      ck
        ? call<Record<string, unknown>>("daemon_status", { conn: ck }).catch(() => null)
        : Promise.resolve(null),
    ]);
    if (my !== this.gen || !this.isOpen()) return;
    this.remote = remote;
    const v = (x: string) => (x ? `v${x}` : "—");
    const arrow = (a: string, b: string) => `${v(a)} → ${v(b)}`;

    // Application (desktop / Android — la SPA web suit le démon).
    const appNewer = !isWeb && !!(lv && remote && cmpVer(remote, lv) > 0);
    $("upd-app-v").textContent = appNewer ? arrow(lv, remote) : v(lv);
    if (appNewer) {
      appGo.hidden = false;
      appGo.onclick = async () => {
        appGo.disabled = true;
        const prev = appGo.textContent;
        appGo.textContent = t("update_installing", { v: remote });
        try {
          await performClientUpdate(remote);
        } finally {
          appGo.disabled = false;
          appGo.textContent = prev;
        }
      };
    }

    // Démon de la connexion liée : cible = la version vers laquelle tout
    // converge (client à jour → sa version ; en retard → la release).
    const dv = typeof dst?.version === "string" ? dst.version : "";
    $("upd-daemon-row").hidden = !ck;
    let daemonNewer = false;
    if (ck) {
      $("upd-daemon-host").textContent =
        ck !== "web" ? ` · ${connLabel(ck)}` : "";
      const target = isWeb || !lv ? remote : appNewer ? remote : lv;
      daemonNewer = !!(dv && target && cmpVer(target, dv) > 0);
      $("upd-daemon-v").textContent = daemonNewer ? arrow(dv, target) : v(dv);
      if (daemonNewer) {
        dGo.hidden = false;
        dGo.onclick = async () => {
          dGo.disabled = true;
          try {
            await performDaemonUpdate({
              conn: ck,
              target,
              current: dv,
              cfg: await daemonCfgFor(ck),
            });
          } finally {
            dGo.disabled = false;
          }
          await this.waitDaemon(ck);
        };
      }
    }

    if (!remote) {
      status.textContent = t("update_failed");
      status.classList.add("err");
    } else if (appNewer || daemonNewer) {
      status.textContent = t("upd_available", { v: remote });
    } else {
      status.textContent = t("update_uptodate", { v: lv || dv || remote });
      status.classList.add("ok");
    }
  }

  /** Après un update démon : le restart coupe la socket — « redémarrage… »
   *  tant que daemon_status ne répond pas, puis re-vérification. Boucle
   *  bornée ; la fenêtre refermée arrête les polls. */
  private async waitDaemon(ck: string) {
    for (let i = 0; i < 15 && this.isOpen(); i++) {
      const st = await call<Record<string, unknown>>("daemon_status", { conn: ck })
        .catch(() => null);
      if (typeof st?.version === "string" && st.version) break;
      $("upd-daemon-v").textContent = t("about_daemon_restarting");
      await new Promise((r) => setTimeout(r, 3000));
    }
    if (this.isOpen()) await this.refresh();
  }
}
