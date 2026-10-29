// ---------------------------------------------------------------------------
// Vue de connexion : remplace la zone de discussion pour choisir/éditer
// une connexion — PAS un dialogue modal (bloquant, masque tout) : c'est un
// panneau plein dans #chat comme settings-view (inert + nav + Échap).
// Modèle mono-connexion (#31) : c'est aussi l'écran d'accueil quand aucune
// connexion n'est up, et la progression d'install/update du démon y est
// inline (plus de dlg-daemon qui gelait la saisie derrière un backdrop).
// ---------------------------------------------------------------------------

import { invoke, getVersion, listen, isWeb } from "../core/bridge";
import { $, dlgErr, esc, notify, q } from "../core/dom";
import { call } from "../core/rpc";
import { cmpVer, connKeyOf, daemonParams, minorOf } from "../core/conn";
import { accentOf, isAndroid } from "../core/chrome";
import type { SavedConn, SessionMeta } from "../core/types";
import { t, type Key } from "../i18n";
import { icon } from "../ui/icons";
import { nav } from "../ui/nav";
import { showOutdated } from "./update";
import type { ConnectionManager } from "./connections";

// Étapes du script ensure_daemon — chaque `echo step:<id>` distant est
// streamé (stderr/stdout selon le transport) en event `daemon-step`.
const SCRIPT_STEPS = ["check", "download", "install", "service", "verify"];
// Variante pour l'auto-update via ReqKind::UpdateDaemon (pas de script).
const SELF_STEPS = ["send", "restart"];

export class ConnView {
  private conns!: ConnectionManager;
  /** Rafraîchit la sidebar après connexion (injecté). */
  refresh: () => Promise<void> = async () => {};
  /** Auto-connexion en cours : hôtes affichés sur l'écran vide (injecté). */
  connecting: (_hosts: string[]) => void = () => {};
  /** La vue s'apprête à s'ouvrir : l'autre overlay de #chat (paramètres)
   *  doit se fermer — deux panneaux pleins empilés seraient inertes l'un
   *  sous l'autre (injecté par le bootstrap). */
  onOpening: () => void = () => {};
  private saved: SavedConn[] = [];
  private stepEls = new Map<string, HTMLElement>();
  /** Clés de conn dont le bouton supprimer est armé (2e tap = suppression) —
   *  survit aux re-renders de la liste. */
  private armedDel = new Set<string>();
  /** L'utilisateur a fermé la vue pendant une progression démon : les
   *  étapes suivantes du MÊME flux ne doivent pas la re-pop (install →
   *  self-update → script enchaînés). Réarmé à chaque connect. */
  private daemonDetached = false;
  /** La vue s'est auto-ouverte comme accueil (rien d'up) et l'utilisateur
   *  l'a fermée sans connecter — pas de réouverture forcée tant qu'aucune
   *  conn ne repasse « ok » (réarmement dans onLiveChanged). */
  private homeDismissed = false;

  bind(conns: ConnectionManager) {
    this.conns = conns;
  }

  private isOpen() {
    return !$("conn-view").hidden;
  }

  /** Rend les éléments recouverts de #chat inertes (clics + tabulation) —
   *  même mécanisme que settings-view (un <div> ne capte pas le focus
   *  comme un <dialog> modal). */
  private setChatInert(v: boolean) {
    for (const el of [...$("chat").children]) {
      if (el.id !== "conn-view") (el as HTMLElement).inert = v;
    }
  }

  /** Erreur du flux de connexion : dans la vue si elle est ouverte, toast
   *  sinon (auto-connect en arrière-plan). */
  private report(msg: string, level: "info" | "success" | "warn" | "error" = "error") {
    if (this.isOpen()) dlgErr("c-err", msg);
    else notify(level, msg);
  }

  /** Ligne de statut du flux de connexion (étape en cours). */
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

  /** Progression install/update du démon — inline dans la vue (steps
   *  streamés par les events `daemon-step`). Ouvre la vue si besoin,
   *  sauf si l'utilisateur l'a détachée pour ce flux. */
  private daemonShow(msg: string, steps: string[] = []) {
    if (!this.isOpen() && !this.daemonDetached) void this.open();
    $("cv-progress").hidden = false;
    $("cv-progress-msg").textContent = msg;
    const ul = $("cv-progress-steps");
    ul.innerHTML = "";
    this.stepEls.clear();
    for (const id of steps) {
      const li = document.createElement("li");
      li.innerHTML =
        `<span class="st-ico">${icon("dot")}</span><span>${esc(t(`dstep_${id}` as Key))}</span>`;
      ul.appendChild(li);
      this.stepEls.set(id, li);
    }
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
        ico.innerHTML = icon("loader");
      } else if (!seen) {
        li.className = "done";
        ico.innerHTML = icon("check");
      }
    }
  }

  private daemonFinish() {
    for (const li of this.stepEls.values()) {
      li.className = "done";
      li.querySelector<HTMLElement>(".st-ico")!.innerHTML = icon("check");
    }
  }

  private daemonClose() {
    $("cv-progress").hidden = true;
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
   *  Gate capacité : l'appelant ne tente ceci que si `self_update` est
   *  vrai dans le daemon_status. Un démon plus vieux ne connaît pas
   *  ReqKind::UpdateDaemon — sa réponse d'erreur de parse porte id=0,
   *  que client-core droppe (routage par id) → l'appel attendrait le
   *  timeout de 60 s pour rien. Le flag est immune à la numérotation
   *  de release (contrairement à un seuil de version codé en dur). */
  private async tryDaemonSelfUpdate(
    conn: string, lv: string,
  ): Promise<boolean> {
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

  /** `ensure_daemon` via le script distant, derrière la progression inline.
   *  Retourne la réponse ("ok:*"|"newer:*"|"busy:*"|"installed:*"). */
  private async runDaemonScript(
    cfg: Record<string, unknown>, tag: string, msg: string,
  ): Promise<string> {
    this.daemonShow(msg, SCRIPT_STEPS);
    try {
      const res = await invoke<string>("ensure_daemon", daemonParams(cfg, tag));
      this.daemonFinish();
      // Laisser voir l'état « toutes étapes ✓ » un instant avant de cacher.
      await new Promise((r) => setTimeout(r, 450));
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
   *  démon distant (dlg-dupdate réutilisé avec les strings de l'action).
   *  Reste modal : c'est une question exigeant une réponse, pas une
   *  progression — le gel visé par la spec était la modale longue. */
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

  /// Compteur de génération des connect() — le flux contient de longues
  /// attentes utilisateur (install/update du démon) : un connect initié
  /// plus tard doit gagner le binding, pas le dernier résolu en vol.
  private connectSeq = 0;
  /// Clé de conn visée par le DERNIER connect initié (cf. garde gen).
  private wantedKey = "";

  async connect(cfg: Record<string, unknown>) {
    const host = String(cfg.host ?? cfg.addr ?? cfg.path ?? "local");
    const gen = ++this.connectSeq;
    const wantKey = this.conns.keyOf(cfg);
    this.wantedKey = wantKey;
    // Nouvelle tentative : la progression redevient visible (le flag
    // détaché du flux précédent ne doit pas masquer celle-ci).
    this.daemonDetached = false;
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
        cfg, ver, t("installing_daemon") + " — " + host,
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
              t("updating_daemon", { dv, lv }) + " — " + host, SELF_STEPS,
            );
            this.daemonStep("send");
            selfDone = await this.tryDaemonSelfUpdate(conn, lv);
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
              cfg, lv, t("updating_daemon", { dv, lv }) + " — " + host,
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
    // Un connect plus récent a été initié pendant les attentes de ce flux
    // (install/update confirmées par l'utilisateur) : il gagne le binding.
    // Le transport résolu ici est relâché — sauf s'il sert le flux plus
    // récent (même clé = même endpoint, ctx Rust déjà remplacé).
    if (gen !== this.connectSeq) {
      if (wantKey !== this.wantedKey) {
        await invoke("disconnect", { conn: wantKey }).catch(() => {});
      }
      return;
    }
    this.conns.markConnected(cfg, status as Record<string, unknown>);
    // Modèle multi-instances : cette instance se lie à UNE connexion —
    // les autres transports encore ouverts (conn précédente) sont
    // relâchés ; leurs sessions démon restent vivantes, pilotables par
    // une autre instance. Pose aussi le chrome (accent/titre de la conn).
    await this.conns.evictExcept(wantKey);
    await this.refresh();
    // Bind réussi → retour à la discussion (la vue était l'accueil).
    this.close();
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
    // renderState (et donc onLiveChanged) a déjà tourné dans delete() —
    // sans re-render la ligne resterait affichée alors qu'elle est purgée.
    if (this.isOpen()) this.renderSavedList();
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
    ($("c-accent") as HTMLSelectElement).value = c.color ?? "";
    this.syncSshFields();
  }

  /** Remet le formulaire en mode « nouvelle connexion » (sortie de
   *  l'édition d'une saved conn). */
  private resetForm() {
    ($("cv-form-title") as HTMLElement).textContent = t("new_conn");
    ($("cv-form-reset") as HTMLButtonElement).hidden = true;
    // Défaut ssh : le cas nominal (démon sur serveur distant).
    ($("c-kind") as HTMLSelectElement).value = "ssh";
    for (const id of ["c-host", "c-args", "c-user", "c-pass"]) {
      ($(id) as HTMLInputElement).value = "";
    }
    ($("c-port") as HTMLInputElement).value = "7331";
    ($("c-sport") as HTMLInputElement).value = "22";
    ($("c-native") as HTMLInputElement).checked = isAndroid();
    ($("c-accent") as HTMLSelectElement).value = "";
    this.syncSshFields();
  }

  /** Passage en édition d'une connexion sauvegardée : form prérempli +
   *  bouton reset pour revenir au mode nouvelle connexion. */
  private editSaved(c: SavedConn) {
    this.fillForm(c);
    ($("cv-form-title") as HTMLElement).textContent = t("edit_conn");
    ($("cv-form-reset") as HTMLButtonElement).hidden = false;
    $("cv-form-sec").scrollIntoView({ block: "nearest" });
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

  /** Appelé par chat.updateEmptyState quand aucune conv n'est affichée :
   *  aucune conn bound OU toutes down/gone → la vue s'ouvre en accueil
   *  (spec : « pas de conn configurée → vue accueil = cet écran » ; une
   *  perte de conn avec une session ouverte n'ouvre PAS la vue — on
   *  n'arrache pas l'utilisateur à sa lecture). Fermée manuellement →
   *  pas de réouverture forcée jusqu'au prochain bind réussi. */
  maybeAutoShow() {
    if (this.conns.anyUp()) {
      this.homeDismissed = false;
      return;
    }
    if (this.homeDismissed || this.isOpen()) return;
    void this.open();
  }

  /** États live changés (renderState de ConnectionManager) : réarme
   *  l'accueil auto dès qu'une conn remonte « ok », et rafraîchit les
   *  badges de statut de la liste si la vue est ouverte. */
  onLiveChanged() {
    if (this.conns.anyUp()) this.homeDismissed = false;
    if (this.isOpen()) this.renderSavedList();
  }

  /** Liste des connexions sauvegardées : pastille couleur, hôte, statut
   *  live quand la clé est bound, actions (connecter ici / nouvelle
   *  instance desktop / éditer / supprimer en 2 taps). */
  renderSavedList() {
    const ul = $("cv-saved");
    ul.innerHTML = "";
    $("cv-saved-sec").hidden = this.saved.length === 0;
    this.saved.forEach((c) => {
      const key = connKeyOf(c, isAndroid());
      const st = this.conns.state(key);
      const bound = this.conns.hasCfg(key);
      const li = document.createElement("li");
      li.className = "cv-conn";
      const acc = accentOf(c.color);
      const dot = `<span class="cv-dot${acc ? ` c-${acc}` : ""}"></span>`;
      const status = bound
        ? `<span class="cv-st cv-st-${st ?? "down"}">${esc(
            this.conns.tip(st ?? "down", key).split(" — ")[0])}</span>`
        : "";
      li.innerHTML =
        `${dot}<span class="cv-name">${esc(c.kind)} ${esc(c.host ?? c.addr ?? c.path ?? "local")}</span>` +
        status +
        `<span class="cv-btns">` +
        // Icônes inline : hydrateIcons() ne tourne qu'au boot — un
        // <li> créé après resterait avec des boutons vides.
        `<button type="button" class="cv-go" title="${q(esc(t("cv_connect_here")))}">${icon("play")}</button>` +
        (isAndroid() || isWeb
          ? ""
          : `<button type="button" class="cv-new" title="${q(esc(t("conn_spawn")))}">${icon("external")}</button>`) +
        `<button type="button" class="cv-edit" title="${q(esc(t("edit_conn")))}">${icon("edit")}</button>` +
        `<button type="button" class="cv-del danger" title="${q(esc(t("ctx_delete_conn")))}">${icon("close")}</button>` +
        `</span>`;
      (li.querySelector(".cv-go") as HTMLButtonElement).onclick = () =>
        void this.go({ ...c });
      const nw = li.querySelector(".cv-new") as HTMLButtonElement | null;
      if (nw) {
        nw.onclick = async () => {
          try {
            await invoke("spawn_instance", { conn: key });
            notify("success", t("conn_spawned"));
          } catch (e) {
            this.report(String(e));
          }
        };
      }
      (li.querySelector(".cv-edit") as HTMLButtonElement).onclick = () =>
        this.editSaved(c);
      const del = li.querySelector(".cv-del") as HTMLButtonElement;
      if (this.armedDel.has(key)) del.classList.add("armed");
      del.onclick = () => {
        // Deux taps : le premier arme (icône rouge), le second supprime.
        // L'armement vit dans armedDel — pas dans le DOM : un re-render
        // (liveChanged) entre les deux taps effacerait sinon l'état.
        if (!this.armedDel.has(key)) {
          this.armedDel.add(key);
          del.classList.add("armed");
          setTimeout(() => {
            this.armedDel.delete(key);
            // Le bouton capturé a pu être détaché par un re-render
            // (liveChanged) : le nouveau porte déjà .armed → re-render.
            if (del.isConnected) del.classList.remove("armed");
            else if (this.isOpen()) this.renderSavedList();
          }, 2500);
          return;
        }
        this.armedDel.delete(key);
        if (bound) {
          // Conn bound → delete complet (sessions démon tuées si joignable,
          // cfg + saved + groupes purgés, chrome retombé au neutre).
          void this.conns.delete(key);
        } else {
          void invoke("remove_connection", { cfg: c })
            .then(() => {
              this.saved = this.saved.filter((x) => x !== c);
              this.renderSavedList();
            })
            .catch((e) => this.report(String(e)));
        }
      };
      // Clic sur la ligne (hors boutons) = éditer aussi — geste rapide.
      li.onclick = (e) => {
        if ((e.target as HTMLElement).closest("button")) return;
        this.editSaved(c);
      };
      ul.appendChild(li);
    });
  }

  /** Ouvre la vue (idempotent — déjà ouverte = no-op, la liste et les
   *  badges sont tenus à jour par onLiveChanged) : affichée AVANT les
   *  awaits (retour visuel immédiat, et isOpen() devient vrai d'emblée —
   *  un second appel pendant les fetches resterait un no-op). */
  async open() {
    if (this.isOpen()) return;
    this.onOpening();
    const view = $("conn-view");
    view.hidden = false;
    this.setChatInert(true);
    // Focus au panneau : le focus résiduel dans #input enverrait des
    // frappes invisibles à l'agent (même motif que settings-view).
    view.focus({ preventScroll: true });
    nav.push("connview");
    // Android : pas de socket local → l'option n'existe pas (SSH natif
    // par défaut, c'est resetForm/fillForm qui posent les valeurs).
    if (isAndroid()) {
      ($("c-kind") as HTMLSelectElement).querySelector('option[value="local"]')?.remove();
    }
    this.resetForm();
    // Alias Host de ~/.ssh/config en suggestions
    const dl = $("ssh-hosts");
    dl.innerHTML = "";
    for (const h of await invoke<string[]>("ssh_hosts").catch(() => [] as string[])) {
      const o = document.createElement("option");
      o.value = h;
      dl.appendChild(o);
    }
    // Connexions sauvegardées → liste à pastilles (pas un <select>).
    this.saved = await invoke<SavedConn[]>("saved_connections").catch(() => []);
    this.renderSavedList();
  }

  close() {
    const was = this.isOpen();
    $("conn-view").hidden = true;
    // Ne lever l'inert que si l'autre overlay #chat est aussi fermé —
    // sinon on ré-expose #input derrière settings-view (frappes
    // invisibles, le bug exact que l'inert prévient).
    if ($("settings-view").hidden) this.setChatInert(false);
    nav.close("connview");
    if (!was) return;
    // Fermée pendant une progression démon → détachée pour ce flux (la
    // suite tourne en tâche de fond, résultats via report/cStatus).
    this.daemonDetached = true;
    // Fermée alors que rien n'est up → épisode d'accueil soldé : pas de
    // réouverture forcée (CTA de l'empty-state rouvre à la demande).
    this.homeDismissed = !this.conns.anyUp();
    // Focus rendu au champ (pas sous Android : ouvrirait le clavier virtuel).
    if (!isAndroid()) ($("input") as HTMLTextAreaElement).focus();
  }

  toggle() {
    if (this.isOpen()) this.close();
    else void this.open();
  }

  /** Clique sur « Connecter » du formulaire (ou ligne « connecter ici ») :
   *  construit la cfg depuis le form ou directement depuis la saved conn. */
  private async go(cfg: Record<string, unknown>) {
    const btn = $("c-go") as HTMLButtonElement;
    btn.disabled = true;
    btn.classList.add("spin");
    try {
      await this.connect(cfg);
      invoke("save_connection", { cfg }).catch(console.error);
    } catch (err) {
      // La vue reste ouverte : l'erreur va dans sa zone interne.
      dlgErr("c-err", String(err));
    } finally {
      this.cStatus(null);
      btn.disabled = false;
      btn.classList.remove("spin");
    }
  }

  private cfgFromForm(): Record<string, unknown> {
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
    // Couleur propre à la connexion ("" = accent global des réglages).
    cfg.color = ($("c-accent") as HTMLSelectElement).value || undefined;
    return cfg;
  }

  init() {
    // Progression du script distant (marqueurs step:* streamés par
    // ensure_daemon) — met à jour la liste inline.
    void listen<string>("daemon-step", (e) => this.daemonStep(e.payload));
    // « Arrière-plan » = fermer la vue : l'opération distante continue,
    // ses résultats reviennent via report()/cStatus (vue détachée).
    $("cv-progress-bg").onclick = () => {
      this.daemonDetached = true;
      this.close();
    };
    $("connview-close").onclick = () => this.close();
    // Échap ferme la vue — sauf si un dialogue est ouvert par-dessus
    // (confirmation update/install du démon : Échap = lui).
    window.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && this.isOpen() && !document.querySelector("dialog[open]"))
        this.close();
    });
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
    $("cv-form-reset").onclick = () => this.resetForm();
    $("cv-logs").addEventListener("toggle", async (e) => {
      if ((e.target as HTMLDetailsElement).open) await this.renderLog();
    });
    ($("log-level") as HTMLSelectElement).onchange = () => this.renderLog();

    // Le bouton vit dans la sidebar — drawer sur Android : le refermer
    // d'abord sinon la vue s'ouvre derrière le tiroir. Toggle : recliquer
    // quand elle est déjà ouverte = « j'ai fini » → retour discussion.
    $("btn-connect").onclick = () => {
      nav.closeDrawer();
      this.toggle();
    };
    $("c-go").addEventListener("click", async (e) => {
      e.preventDefault();
      await this.go(this.cfgFromForm());
    });
  }

  /** Auto-reconnexion au démarrage — UNE connexion par instance (modèle
   *  multi-instances) : celle demandée par `--conn` (`want`, clé connKey),
   *  sinon la dernière utilisée (première entrée de connections.json).
   *  Les autres connexions enregistrées restent disponibles dans la vue
   *  et aux autres instances. `saved` est préchargé par le boot. */
  autoConnect(saved: SavedConn[], want?: string) {
    this.saved = saved;
    // Web : la conn unique (le démon qui sert la page) est vitale —
    // la préférence "autoconnect off" n'a pas de sens ici. Et `want`
    // (--conn) est une intention explicite (instance spawnée depuis une
    // autre) — pas une reconnexion passive : elle court-circuite la pref.
    if (!want && !isWeb && localStorage.getItem("ac-autoconnect") === "0") return;
    const target =
      (want ? saved.find((c) => connKeyOf(c, isAndroid()) === want) : undefined) ??
      saved[0];
    if (!target) return;
    const host = String(target.host ?? target.addr ?? target.path ?? "local");
    this.connecting([host]);
    void this.connect({ ...target })
      .catch((e) => this.report(String(e)))
      .finally(() => this.connecting([]));
  }
}
