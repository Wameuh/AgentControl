// ---------------------------------------------------------------------------
// Dialogue de connexion : formulaire SSH/TCP/local, connexions récentes
// sauvegardées, bootstrap/install du démon distant, garde-fous de version.
// ---------------------------------------------------------------------------

import { invoke } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import { listen } from "@tauri-apps/api/event";
import { $, dlgErr, esc, notify } from "../core/dom";
import { call } from "../core/rpc";
import { cmpVer, connKeyOf, daemonParams, minorOf } from "../core/conn";
import { isAndroid } from "../core/chrome";
import type { SavedConn, SessionMeta } from "../core/types";
import { t, type Key } from "../i18n";
import { icon } from "../ui/icons";
import { showOutdated } from "./update";
import type { ConnectionManager } from "./connections";

// Étapes du script ensure_daemon — chaque `echo step:<id>` distant est
// streamé (stderr/stdout selon le transport) en event `daemon-step`.
const SCRIPT_STEPS = ["check", "download", "install", "service", "verify"];
// Variante pour l'auto-update via ReqKind::UpdateDaemon (pas de script).
const SELF_STEPS = ["send", "restart"];

export class ConnectDialog {
  private conns!: ConnectionManager;
  /** Rafraîchit la sidebar après connexion (injecté). */
  refresh: () => Promise<void> = async () => {};
  /** Auto-connexion en cours : hôtes affichés sur l'écran vide (injecté). */
  connecting: (_hosts: string[]) => void = () => {};
  private saved: SavedConn[] = [];
  private stepEls = new Map<string, HTMLElement>();

  bind(conns: ConnectionManager) {
    this.conns = conns;
  }

  /** Erreur du flux de connexion : dans le dialogue s'il est ouvert (les
   *  toasts passent sous son backdrop modal), toast sinon (auto-connect). */
  private report(msg: string, level: "info" | "success" | "warn" | "error" = "error") {
    if (($("dlg-conn") as HTMLDialogElement).open) dlgErr("c-err", msg);
    else notify(level, msg);
  }

  /** Ligne de statut bas du dialogue de connexion (étape en cours). */
  private cStatus(msg: string | null) {
    const el = $("c-status");
    if (msg == null) {
      el.hidden = true;
      el.textContent = "";
      return;
    }
    el.textContent = msg;
    el.hidden = false;
  }

  /** Dialogue modal pendant install/update du démon distant — liste
   *  d'étapes mises à jour par les events `daemon-step` du backend. */
  private daemonShow(title: string, msg: string, steps: string[] = [], note = "") {
    const d = $("dlg-daemon") as HTMLDialogElement;
    $("dlg-daemon-title").textContent = title;
    $("dlg-daemon-msg").textContent = msg;
    $("dlg-daemon-note").textContent = note;
    const ul = $("dlg-daemon-steps");
    ul.innerHTML = "";
    this.stepEls.clear();
    for (const id of steps) {
      const li = document.createElement("li");
      li.innerHTML =
        `<span class="st-ico">○</span><span>${esc(t(`dstep_${id}` as Key))}</span>`;
      ul.appendChild(li);
      this.stepEls.set(id, li);
    }
    if (!d.open) d.showModal();
  }

  /** Marqueur `step:<id>` reçu : les étapes passées → done, celle-ci → cur. */
  private daemonStep(step: string) {
    if (!this.stepEls.has(step)) return; // script sans marqueurs connus
    let seen = false;
    for (const [id, li] of this.stepEls) {
      const ico = li.querySelector<HTMLElement>(".st-ico")!;
      if (id === step) {
        seen = true;
        li.className = "cur";
        ico.textContent = "◌";
      } else if (!seen) {
        li.className = "done";
        ico.textContent = "✓";
      }
    }
  }

  private daemonFinish() {
    for (const li of this.stepEls.values()) {
      li.className = "done";
      li.querySelector<HTMLElement>(".st-ico")!.textContent = "✓";
    }
  }

  private daemonClose() {
    const d = $("dlg-daemon") as HTMLDialogElement;
    if (d.open) d.close();
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
    // → filtre client sur s.conn. "Vivant" = ni dead ni interrupted ni
    // asleep (idle inclus : le restart tue son process aussi ; une session
    // endormie n'a déjà plus de process). Échec → compteur inconnu.
    const sessions = await call<SessionMeta[]>("list_sessions", { conn })
      .catch(() => null);
    const n = sessions?.filter((s) =>
      s.conn === conn && s.state !== "dead" && s.state !== "interrupted" &&
      s.state !== "asleep").length;
    return this.daemonAction(
      t("dupdate_h"),
      n == null
        ? t("dupdate_msg_unk", { dv, lv, host })
        : t("dupdate_msg", { dv, lv, host, n }),
      t("update_now"),
    );
  }

  /** Tente `update_daemon` (auto-update par le démon lui-même).
   *  `true` = traité (succès ou échec définitif affiché — pas de
   *  fallback) ; `false` = démon trop vieux pour connaître la requête
   *  → l'appelant bascule sur le script `ensure_daemon`. Sur succès le
   *  démon sort sous son superviseur : la connexion morte qui suit est
   *  attendue, le backoff de conns la rétablit tout seul.
   *
   *  Gate capacité : `canSelfUpdate` = flag `self_update` du
   *  daemon_status. Un démon plus vieux ne connaît pas
   *  ReqKind::UpdateDaemon — sa réponse d'erreur de parse porte id=0,
   *  que client-core droppe (routage par id) → l'appel attendrait le
   *  timeout de 60 s pour rien. On saute donc la tentative. Le flag
   *  est immune à la numérotation de release (contrairement à un
   *  seuil de version codé en dur). */
  private async tryDaemonSelfUpdate(
    conn: string, canSelfUpdate: boolean, lv: string,
  ): Promise<boolean> {
    if (!canSelfUpdate) return false;
    try {
      const r = await call<{ restarting?: boolean }>(
        "update_daemon", { conn, version: lv });
      if (r?.restarting) notify("success", t("dupdate_self_ok", { lv }));
      else notify("warn", t("dupdate_self_manual", { lv }));
      return true;
    } catch (e) {
      const msg = String(e);
      // Requête inconnue du démon (build hybride/dev) → script ssh.
      if (/unknown|inconnue|unrecognized|unsupported|not.?supported/i.test(msg)) return false;
      // Échec définitif (binaire non inscriptible, download) : le
      // script aurait les mêmes droits → afficher plutôt que retenter.
      this.report(msg);
      return true;
    }
  }

  /** `ensure_daemon` via le script distant, derrière le dialogue à étapes.
   *  Retourne la réponse ("ok:*"|"newer:*"|"busy:*"|"installed:*"). */
  private async runDaemonScript(
    cfg: Record<string, unknown>, tag: string, title: string, msg: string,
  ): Promise<string> {
    this.daemonShow(title, msg, SCRIPT_STEPS);
    try {
      const res = await invoke<string>("ensure_daemon", daemonParams(cfg, tag));
      this.daemonFinish();
      return res;
    } finally {
      this.daemonClose();
    }
  }

  /** Bootstrap install sur un hôte où connect a échoué : l'action copie
   *  des binaires et active une unit systemd distante — validation
   *  explicite, car l'hôte peut être un passage (port forwardé vers un
   *  démon managé par conteneur/superviseur) où installer serait une
   *  erreur. */
  private async confirmDaemonInstall(host: string): Promise<boolean> {
    return this.daemonAction(t("dinstall_h"), t("dinstall_msg", { host }), t("install_now"));
  }

  /** Dialogue de confirmation générique pour une action mutante sur le
   *  démon distant (dlg-dupdate réutilisé avec les strings de l'action). */
  private daemonAction(h3: string, msg: string, go: string): Promise<boolean> {
    const dlg = $("dlg-dupdate") as HTMLDialogElement;
    if (dlg.open) return Promise.resolve(false);
    dlg.querySelector("h3")!.textContent = h3;
    $("dupdate-msg").textContent = msg;
    ($("dupdate-go") as HTMLButtonElement).textContent = go;
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
    const host = String(cfg.host ?? cfg.addr ?? cfg.path ?? "local");
    dlgErr("c-err", null);
    this.cStatus(t("connecting_to", { host }));
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
      // L'install copie des binaires + une unit systemd sur l'hôte : si la
      // connexion pointe un démon managé autrement (conteneur, superviseur,
      // port forwardé) mais momentanément down, installer ici écraserait
      // l'hôte — confirmation explicite avant toute mutation.
      if (!(await this.confirmDaemonInstall(String(cfg.host)))) throw e;
      const res = await this.runDaemonScript(
        cfg, ver, t("dinstall_title"),
        t("installing_daemon") + " — " + host,
      );
      // Démon plus récent : le script distant refuse le downgrade.
      if (res.startsWith("newer:")) newerDaemon = res.slice(6);
      else if (res.startsWith("busy:")) {
        this.report(t("dupdate_busy", { host, addr: res.slice(5) }), "warn");
      } else if (res.startsWith("installed:") && res.slice(10) !== ver) {
        this.report(t("dupdate_stale", { host, want: ver, got: res.slice(10) || "?" }));
      }
      this.cStatus(t("connecting_to", { host }));
      status = await invoke("connect", { cfg }).catch((e2) => {
        if (newerDaemon) showOutdated(ver, newerDaemon);
        throw e2;
      });
    }
    // Incompatibilité de version mineure (x.y) — jamais à chaque patch.
    const dv = (status as { version?: string }).version || newerDaemon;
    const lv = await getVersion().catch(() => "0.0.0");
    this.cStatus(dv ? t("daemon_detected", { v: dv }) : null);
    // Capacité annoncée par daemon_status — absente des démons plus
    // vieux que la feature (undefined → falsy → gate vers le script).
    const canSelfUpdate =
      (status as { self_update?: boolean }).self_update === true;
    if (dv && lv && minorOf(dv) !== minorOf(lv)) {
      if (cmpVer(minorOf(dv), minorOf(lv)) > 0) {
        // Démon plus récent → client outdated : popup, démon non modifié.
        showOutdated(lv, dv);
      } else {
        // Auto-update par le démon (quel que soit le transport : la
        // requête passe par le socket déjà connecté) — le binaire se
        // remplace puis son superviseur le relance. Le restart tue les
        // agents en cours → validation utilisateur.
        const conn = this.conns.keyOf(cfg);
        if (await this.confirmDaemonUpdate(dv, lv, host, conn)) {
          let selfDone = false;
          if (canSelfUpdate) {
            this.daemonShow(
              t("dupdate_running"), t("updating_daemon", { dv, lv }) + " — " + host,
              SELF_STEPS,
            );
            this.daemonStep("send");
            selfDone = await this.tryDaemonSelfUpdate(conn, true, lv);
            if (selfDone) {
              // Le démon sort sous son superviseur : la reconnexion est
              // gérée par le backoff — la note l'explique, court délai
              // pour laisser lire l'étape "restart" puis fermer.
              this.daemonStep("restart");
              await new Promise((r) => setTimeout(r, 1200));
            }
            this.daemonClose();
          }
          // `false` = démon trop vieux pour la requête → fallback
          // script distant (ssh seulement — il faut un exec).
          if (!selfDone && cfg.kind === "ssh" && cfg.host) {
            const res = await this.runDaemonScript(
              cfg, lv, t("dupdate_running"),
              t("updating_daemon", { dv, lv }) + " — " + host,
            ).catch((e) => {
              this.report(String(e));
              return "";
            });
            // Un autre client a pu upgrader le démon entre-temps → même garde-fou.
            if (res.startsWith("newer:")) showOutdated(lv, res.slice(6));
            else if (res.startsWith("busy:")) {
              this.report(t("dupdate_busy", { host, addr: res.slice(5) }), "warn");
            } else if (res) {
              if (res.startsWith("installed:") && res.slice(10) !== lv) {
                this.report(t("dupdate_stale", { host, want: lv, got: res.slice(10) || "?" }));
              }
              this.cStatus(t("connecting_to", { host }));
              status = await invoke("connect", { cfg }).catch(() => status);
            }
          } else if (!selfDone) {
            // Hors-ssh (local/tcp) avec un démon trop vieux pour
            // l'auto-update : pas d'exec possible → message honnête.
            this.report(t("dupdate_too_old", { dv, lv }));
          }
        }
      }
    }
    this.cStatus(null);
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

  /** Rang d'un niveau de log — le filtre du sélecteur est un niveau MINIMUM. */
  private logRank(l: string): number {
    return ({ debug: 0, info: 1, warn: 2, error: 3 } as Record<string, number>)[l] ?? 1;
  }

  /** Tail de client.log filtré par niveau et coloré (lignes
   *  `[iso] [LVL] fichier.rs:NN msg` ; les lignes anciennes sans tag de
   *  niveau sont traitées en info). */
  private async renderLog() {
    const min = this.logRank(($("log-level") as HTMLSelectElement).value);
    const txt = await invoke<string>("log_tail", { n: 400 }).catch(() => "");
    const rows: string[] = [];
    for (const line of txt.split("\n")) {
      const m = line.match(/^(\[[^\]]*\] \[(DEBUG|INFO|WARN|ERROR)\])(?: (\S+:\d+))?/);
      if (this.logRank(m?.[2]?.toLowerCase() ?? "info") < min) continue;
      const cls = m ? ` ll-${m[2].toLowerCase()}` : "";
      let body = esc(line);
      if (m) {
        body = `<span class="ll-meta">${esc(m[1])}</span>`;
        if (m[3]) body += ` <span class="ll-meta">${esc(m[3])}</span>`;
        body += esc(line.slice(m[0].length));
      }
      rows.push(`<span class="ll${cls}">${body}</span>`);
    }
    const v = $("log-view");
    v.innerHTML = rows.join("\n");
    v.scrollTop = v.scrollHeight;
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
    // Progression du script distant (marqueurs step:* streamés par
    // ensure_daemon) — met à jour la liste si le dialogue est ouvert.
    void listen<string>("daemon-step", (e) => this.daemonStep(e.payload));
    // Un <dialog> modal se ferme sur Échap — pendant install/update le
    // travail continuerait en arrière-plan sans retour visible → bloqué.
    ($("dlg-daemon") as HTMLDialogElement).addEventListener("cancel", (e) =>
      e.preventDefault(),
    );
    $("c-pubkey-copy").onclick = async () => {
      const txt = $("c-pubkey").textContent ?? "";
      try {
        await navigator.clipboard.writeText(txt);
        $("c-pubkey-copy").innerHTML = icon("check");
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
        .catch((e) => { dlgErr("c-err", String(e)); return false; });
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
      const show = ($("log-view")).hidden;
      $("log-view").hidden = !show;
      $("log-bar").hidden = !show;
      if (show) await this.renderLog();
    });
    ($("log-level") as HTMLSelectElement).onchange = () => this.renderLog();

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
        // Le dialogue reste ouvert : l'erreur va dans sa zone interne
        // (un toast passerait sous le backdrop modal).
        dlgErr("c-err", String(err));
      } finally {
        this.cStatus(null);
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
        // Les échecs tombent en toast (dialogue fermé) ; l'écran vide
        // affiche "connexion à X…" le temps du boot.
        this.connecting(conns.map((c) => c.host ?? c.addr ?? c.path ?? "local"));
        for (const c of conns) {
          await this.connect({ ...c }).catch((e) => this.report(String(e)));
        }
        this.connecting([]);
      })
      .catch(() => {});
  }
}
