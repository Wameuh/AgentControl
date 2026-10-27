// ---------------------------------------------------------------------------
// Dialogue de connexion : formulaire SSH/TCP/local, connexions récentes
// sauvegardées, bootstrap/install du démon distant, garde-fous de version.
// ---------------------------------------------------------------------------

import { invoke } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import { $, showErr } from "../core/dom";
import { call } from "../core/rpc";
import { cmpVer, connKeyOf, daemonParams, minorOf } from "../core/conn";
import { isAndroid } from "../core/chrome";
import type { SavedConn, SessionMeta } from "../core/types";
import { t } from "../i18n";
import { showOutdated } from "./update";
import type { ConnectionManager } from "./connections";

export class ConnectDialog {
  private conns!: ConnectionManager;
  /** Rafraîchit la sidebar après connexion (injecté). */
  refresh: () => Promise<void> = async () => {};
  private saved: SavedConn[] = [];

  bind(conns: ConnectionManager) {
    this.conns = conns;
  }

  /** Dialogue modal pendant install/update du démon distant. */
  private daemonDlg(msg: string, open: boolean) {
    const d = $("dlg-daemon") as HTMLDialogElement;
    if (open) {
      ($("dlg-daemon-msg")).textContent = msg;
      if (!d.open) d.showModal();
    } else if (d.open) d.close();
  }

  /** Démon plus vieux que le client (minor ≠) : l'update redémarre le démon
   *  et tue les agents en cours → popup de confirmation. Le message compte
   *  les sessions vivantes de cette connexion (0 si la requête échoue). */
  private async confirmDaemonUpdate(
    dv: string, lv: string, host: string, conn: string,
  ): Promise<boolean> {
    const dlg = $("dlg-dupdate") as HTMLDialogElement;
    if (dlg.open) return false;
    // list_sessions agrège toutes les conns (params.conn ignoré côté Rust)
    // → filtre client sur s.conn. "Vivant" = ni dead ni interrupted (idle
    // inclus : le restart tue son process aussi). Échec → compteur inconnu.
    const sessions = await call<SessionMeta[]>("list_sessions", { conn })
      .catch(() => null);
    const n = sessions?.filter((s) =>
      s.conn === conn && s.state !== "dead" && s.state !== "interrupted").length;
    $("dupdate-msg").textContent = n == null
      ? t("dupdate_msg_unk", { dv, lv, host })
      : t("dupdate_msg", { dv, lv, host, n });
    return new Promise((resolve) => {
      ($("dupdate-go") as HTMLButtonElement).onclick = () => {
        resolve(true);
        dlg.close();
      };
      dlg.addEventListener("close", () => resolve(false), { once: true });
      dlg.showModal();
    });
  }

  async connect(cfg: Record<string, unknown>) {
    let status: unknown;
    let newerDaemon = "";
    try {
      // Toujours établir la connexion — daemon_status fusionné renvoie Ok
      // même sans connexions, il ne peut pas servir de pré-vérification.
      status = await invoke("connect", { cfg });
    } catch (e) {
      // SSH : échec → démon absent ou arrêté → bootstrap depuis la release.
      if (cfg.kind !== "ssh" || !cfg.host) throw e;
      const ver = await getVersion().catch(() => "0.0.0");
      this.daemonDlg(t("installing_daemon") + " — " + String(cfg.host), true);
      try {
        const res = await invoke<string>("ensure_daemon", daemonParams(cfg, ver));
        // Démon plus récent : le script distant refuse le downgrade.
        if (res.startsWith("newer:")) newerDaemon = res.slice(6);
        else if (res.startsWith("installed:") && res.slice(10) !== ver) {
          showErr(t("dupdate_stale", { host: String(cfg.host), want: ver, got: res.slice(10) }));
        }
      } finally {
        this.daemonDlg("", false);
      }
      status = await invoke("connect", { cfg }).catch((e2) => {
        if (newerDaemon) showOutdated(ver, newerDaemon);
        throw e2;
      });
    }
    // Incompatibilité de version mineure (x.y) — jamais à chaque patch.
    const dv = (status as { version?: string }).version || newerDaemon;
    const lv = await getVersion().catch(() => "0.0.0");
    if (dv && lv && minorOf(dv) !== minorOf(lv)) {
      if (cmpVer(minorOf(dv), minorOf(lv)) > 0) {
        // Démon plus récent → client outdated : popup, démon non modifié.
        showOutdated(lv, dv);
      } else if (cfg.kind === "ssh" && cfg.host) {
        // Redémarrer le démon tue les agents en cours → validation utilisateur.
        if (await this.confirmDaemonUpdate(dv, lv, String(cfg.host), this.conns.keyOf(cfg))) {
          this.daemonDlg(t("updating_daemon", { dv, lv }) + " — " + String(cfg.host), true);
          try {
            const res = await invoke<string>("ensure_daemon", daemonParams(cfg, lv))
              .catch((e) => {
                showErr(String(e));
                return "";
              });
            // Un autre client a pu upgrader le démon entre-temps → même garde-fou.
            if (res.startsWith("newer:")) showOutdated(lv, res.slice(6));
            else {
              if (res.startsWith("installed:") && res.slice(10) !== lv) {
                showErr(t("dupdate_stale", { host: String(cfg.host), want: lv, got: res.slice(10) }));
              }
              status = await invoke("connect", { cfg }).catch(() => status);
            }
          } finally {
            this.daemonDlg("", false);
          }
        }
      }
    }
    this.conns.markConnected(cfg);
    await this.refresh();
  }

  /** Purge les entrées sauvegardées matchant `conn` (re-fetch pour couvrir
   *  les conns enregistrées pendant la session courante) — delete_conn. */
  async purgeSaved(conn: string) {
    this.saved = await invoke<SavedConn[]>("saved_connections").catch(() => this.saved);
    for (const c of this.saved) {
      if (connKeyOf(c, isAndroid()) === conn) {
        await invoke("remove_connection", { cfg: c }).catch(() => {});
      }
    }
    this.saved = this.saved.filter((c) => connKeyOf(c, isAndroid()) !== conn);
  }

  private fillForm(c: SavedConn) {
    ($("c-kind") as HTMLSelectElement).value = c.kind;
    ($("c-host") as HTMLInputElement).value = c.host ?? c.addr ?? c.path ?? "";
    ($("c-port") as HTMLInputElement).value = String(c.remote_port ?? 7331);
    ($("c-args") as HTMLInputElement).value = (c.args ?? []).join(" ");
    ($("c-sport") as HTMLInputElement).value = String(c.port ?? 22);
    ($("c-user") as HTMLInputElement).value = c.user ?? "";
    ($("c-pass") as HTMLInputElement).value = c.password ?? "";
    ($("c-native") as HTMLInputElement).checked = c.native === true || isAndroid();
    this.syncSshFields();
  }

  /// Champs SSH natifs visibles si kind=ssh && (native || android) ;
  /// args CLI sinon (desktop). Local → presque rien.
  private syncSshFields() {
    const kind = ($("c-kind") as HTMLSelectElement).value;
    const ssh = kind === "ssh";
    const nat = ($("c-native") as HTMLInputElement).checked || isAndroid();
    document.querySelectorAll<HTMLElement>(".nat-row").forEach((r) => {
      r.style.display = ssh && nat ? "" : "none";
    });
    ($("c-args-wrap") as HTMLElement).style.display = ssh && !nat ? "" : "none";
    ($("c-native-wrap") as HTMLElement).style.display = ssh && !isAndroid() ? "" : "none";
    ($("c-host") as HTMLInputElement).style.display = kind === "local" ? "none" : "";
    ($("c-port") as HTMLInputElement).style.display = ssh ? "" : "none";
    if (ssh && nat) void this.showPubkey();
  }

  private async showPubkey() {
    const box = $("c-pubkey-box");
    if (!box.hidden) return;
    box.hidden = false;
    const pre = $("c-pubkey");
    pre.textContent = await invoke<string>("ssh_pubkey").catch((e) => `⚠ ${e}`);
  }

  /** Repeuple le select des connexions récentes depuis `this.saved`. */
  private fillSavedSelect(): HTMLSelectElement {
    const sv = $("c-saved") as HTMLSelectElement;
    sv.innerHTML = `<option value="">${t("choose")}</option>`;
    this.saved.forEach((c, i) => {
      const o = document.createElement("option");
      o.value = String(i);
      o.textContent = `${c.kind} ${c.host ?? c.addr ?? c.path ?? ""}`;
      sv.appendChild(o);
    });
    return sv;
  }

  private async open() {
    // Android : pas de socket local ni de binaire ssh → SSH natif par défaut.
    if (isAndroid()) {
      const ks = $("c-kind") as HTMLSelectElement;
      ks.querySelector('option[value="local"]')?.remove();
      ks.value = "ssh";
      ($("c-native") as HTMLInputElement).checked = true;
    }
    this.syncSshFields();
    // Alias Host de ~/.ssh/config en suggestions
    const dl = $("ssh-hosts");
    dl.innerHTML = "";
    for (const h of await invoke<string[]>("ssh_hosts").catch(() => [] as string[])) {
      const o = document.createElement("option");
      o.value = h;
      dl.appendChild(o);
    }
    // Connexions récentes sauvegardées
    this.saved = await invoke<SavedConn[]>("saved_connections").catch(() => []);
    const sv = this.fillSavedSelect();
    // Pré-remplir avec la dernière utilisée → un clic sur Connecter suffit
    if (this.saved.length) {
      sv.value = "0";
      this.fillForm(this.saved[0]);
    }
    ($("dlg-conn") as HTMLDialogElement).showModal();
  }

  init() {
    $("c-pubkey-copy").onclick = async () => {
      const txt = $("c-pubkey").textContent ?? "";
      try {
        await navigator.clipboard.writeText(txt);
        $("c-pubkey-copy").textContent = "✓";
      } catch {
        // Clipboard refusé → sélectionner le texte pour copie manuelle.
        const r = document.createRange();
        r.selectNodeContents($("c-pubkey"));
        getSelection()?.removeAllRanges();
        getSelection()?.addRange(r);
      }
      setTimeout(() => { $("c-pubkey-copy").textContent = t("copy"); }, 1500);
    };
    ($("c-kind") as HTMLSelectElement).onchange = () => this.syncSshFields();
    ($("c-native") as HTMLInputElement).onchange = () => this.syncSshFields();

    // ✕ : retire la connexion sauvegardée sélectionnée (fichier + liste).
    $("c-saved-del").addEventListener("click", async () => {
      const sv = $("c-saved") as HTMLSelectElement;
      if (sv.value === "") return;
      const c = this.saved[Number(sv.value)];
      if (!c) return;
      const ok = await invoke("remove_connection", { cfg: c })
        .then(() => true)
        .catch((e) => { showErr(String(e)); return false; });
      if (!ok) return;
      this.saved = this.saved.filter((x) => x !== c);
      this.fillSavedSelect();
    });

    $("btn-connect").onclick = () => this.open();

    ($("c-saved") as HTMLSelectElement).onchange = (e) => {
      const i = Number((e.target as HTMLSelectElement).value);
      if (this.saved[i]) this.fillForm(this.saved[i]);
    };
    $("c-logs").addEventListener("click", async () => {
      const v = $("log-view");
      v.hidden = !v.hidden;
      if (!v.hidden) v.textContent = await invoke<string>("log_tail", { n: 200 });
    });

    $("c-cancel").onclick = () => ($("dlg-conn") as HTMLDialogElement).close("cancel");

    $("c-go").addEventListener("click", async (e) => {
      e.preventDefault();
      const btn = e.currentTarget as HTMLButtonElement;
      btn.disabled = true;
      btn.classList.add("spin");
      const kind = ($("c-kind") as HTMLSelectElement).value;
      const cfg: Record<string, unknown> =
        kind === "ssh"
          ? {
              kind: "ssh",
              host: ($("c-host") as HTMLInputElement).value,
              remote_port: Number(($("c-port") as HTMLInputElement).value) || 7331,
              args: ($("c-args") as HTMLInputElement).value.split(" ").filter(Boolean),
              port: Number(($("c-sport") as HTMLInputElement).value) || 22,
              user: ($("c-user") as HTMLInputElement).value || undefined,
              password: ($("c-pass") as HTMLInputElement).value || undefined,
              native: ($("c-native") as HTMLInputElement).checked || isAndroid() || undefined,
            }
          : kind === "tcp"
            ? { kind: "tcp", addr: ($("c-host") as HTMLInputElement).value }
            : { kind: "local" };
      try {
        await this.connect(cfg);
        invoke("save_connection", { cfg }).catch(console.error);
        ($("dlg-conn") as HTMLDialogElement).close();
      } catch (err) {
        showErr(String(err));
      } finally {
        btn.disabled = false;
        btn.classList.remove("spin");
      }
    });
  }

  /** Auto-reconnexion au démarrage avec la dernière connexion utilisée. */
  autoConnect() {
    invoke<SavedConn[]>("saved_connections")
      .then(async (conns) => {
        this.saved = conns;
        if (localStorage.getItem("ac-autoconnect") === "0") return;
        for (const c of conns) await this.connect({ ...c }).catch((e) => showErr(String(e)));
      })
      .catch(() => {});
  }
}
