// ---------------------------------------------------------------------------
// Composer : champ de saisie, envois (normal/steer/force), autocomplétion
// des slash-commandes, commandes builtin client, file d'attente des
// prompts, picker de sessions natives (reprise).
// ---------------------------------------------------------------------------

import { $, esc, notify, showErr } from "../core/dom";
import { icon, qIco } from "../ui/icons";
import { nav } from "../ui/nav";
import { call, callBg, isConnLost } from "../core/rpc";
import { app } from "../core/state";
import { isAndroid } from "../core/chrome";
import type { CmdInfo, SendMode } from "../core/types";
import { t } from "../i18n";
import { cmdDesc, cmdHint, cmdName } from "./modelbar";
import { sendPref } from "./settings";
import { Attachments } from "./attachments";
import { SttMic } from "./stt-mic";
import type { ChatView } from "./chat-view";

// ---- Commandes built-in (toujours dispo, exécutées côté client) ----
const BUILTINS = (): { name: string; description: string; hint: string }[] => [
  { name: "model", description: t("builtin_model"), hint: "<model>" },
  { name: "resume", description: t("builtin_resume"), hint: "" },
  { name: "cancel", description: t("builtin_cancel"), hint: "" },
  { name: "fork", description: t("builtin_fork"), hint: "" },
  { name: "clear", description: t("builtin_clear"), hint: "" },
  { name: "new", description: t("builtin_new"), hint: "" },
  { name: "sleep", description: t("builtin_sleep"), hint: "" },
];

export class Composer {
  private chat!: ChatView;
  /** Pièces jointes : chips, uploads (.ac-uploads), chemins pour
   *  l'agent — module dédié (picker « + », drop, collage). */
  private attach = new Attachments();
  /** Rafraîchit la sidebar (injecté). */
  refresh: () => Promise<void> = async () => {};
  private acSel = 0;
  /** Champ déplié manuellement (bouton expand) — replie dès que le
      contenu rentre sous le cap normal. */
  private inpExpanded = false;
  /** Brouillons par session (id → texte non envoyé). La map ne contient
   *  que les sessions non affichées : le texte de la session courante
   *  vit dans #input et n'y revient qu'à la bascule suivante. Volatil —
   *  pas de persistance disque. */
  private drafts = new Map<string, string>();
  /** Session propriétaire du texte actuellement dans #input. */
  private draftSid: string | null = null;
  /** Dictée vocale on-device : bouton mic + injection live des partials
   *  dans #input (Android only, no-op ailleurs). */
  readonly mic = new SttMic();

  bind(chat: ChatView) {
    this.chat = chat;
  }

  // ---- Hauteur du champ : auto-grow borné + bouton expand ----
  // Le champ grandit avec le contenu jusqu'à ~1/3 de la hauteur du fil ;
  // au-delà il reste borné et scrollable, et le bouton « N ⌃ » permet de
  // l'étendre à ~2/3 (re-clic = repli). Le bouton disparaît dès que le
  // texte redevient court (ou le champ est vidé/envoyé).

  private setInput(v: string) {
    ($("input") as HTMLTextAreaElement).value = v;
    this.fitInput();
    // La dropdown suit la valeur même quand elle change sans event input
    // (envoi, applyAc, reprise de file) — sinon elle resterait affichée.
    this.refreshAc();
  }

  fitInput() {
    const inp = $("input") as HTMLTextAreaElement;
    const wrap = $("input-wrap");
    const btn = $("btn-expand");
    // Mesure : retour à la hauteur naturelle (rows=2) puis scrollHeight
    // = hauteur nécessaire. +2 pour les bordures (box-sizing border-box).
    inp.style.height = "auto";
    const need = inp.scrollHeight + 2;
    // Référence stable : la colonne #chat (hauteur fixée par le viewport)
    // — pas #events, qui rétrécit quand le champ grandit (cap mouvant).
    const chatH = $("chat")?.clientHeight || window.innerHeight;
    const small = Math.max(100, Math.round(chatH * 0.33));
    const big = Math.max(small + 80, Math.round(chatH * 0.66));
    const over = need > small;
    if (!over) this.inpExpanded = false;
    const cap = this.inpExpanded ? big : small;
    inp.style.height = Math.min(need, cap) + "px";
    wrap.classList.toggle("over", over);
    btn.hidden = !over;
    if (over) {
      const cs = getComputedStyle(inp);
      const padV = parseFloat(cs.paddingTop) + parseFloat(cs.paddingBottom);
      const lh = parseFloat(cs.lineHeight) || 21;
      const lines = Math.max(
        inp.value.split("\n").length,
        Math.round((need - padV) / lh),
      );
      btn.innerHTML =
        icon(this.inpExpanded ? "chev-down" : "chev-up") +
        `<span class="n">${lines}</span>`;
      btn.title = t(this.inpExpanded ? "input_collapse" : "input_expand");
      btn.setAttribute("aria-label", btn.title);
    }
  }

  private async runBuiltin(name: string, arg: string): Promise<boolean> {
    if (!app.current) return false;
    switch (name) {
      case "model":
        if (!arg) return false;
        await call("set_model", { session: app.current.id, model: arg });
        return true;
      case "resume":
        await this.openNativePicker();
        return true;
      case "cancel":
        await call("cancel", { session: app.current.id });
        return true;
      case "clear":
      case "new": {
        // Sémantique /clear des CLI (Devin : alias /new) : journal vidé +
        // contexte agent reparti à zéro (session/new côté démon). La vue est
        // rechargée — le journal ne contient plus que le nouveau cycle.
        const id = app.current.id; // current peut changer pendant l'await
        await call("clear_session", { session: id });
        await this.chat.open(id);
        return true;
      }
      case "sleep":
        // Endort : libère le process, la session native reste reprenable —
        // le prochain envoi la réveille via session/load côté démon.
        await call("sleep_session", { session: app.current.id });
        await this.refresh();
        return true;
      case "fork": {
        // Le daemon libère la session native, /fork via le TUI de l'agent,
        // puis attache la session forkée (même groupe, titre + "_fork").
        this.chat.sysline(t("fork_started"));
        const r = await call<{ session: string }>("fork_session", {
          session: app.current.id,
        });
        await this.refresh();
        if (r.session) await this.chat.open(r.session);
        return true;
      }
    }
    return false;
  }

  /** Dialogue : sessions natives de l'agent (ACP session/list) → reprise. */
  private async openNativePicker() {
    if (!app.current) return;
    const box = $("native-list");
    box.innerHTML = "…";
    ($("dlg-resume") as HTMLDialogElement).showModal();
    try {
      const r = await call<{ sessions?: { sessionId: string; title?: string; updatedAt?: string; cwd?: string }[] }>(
        "list_native_sessions",
        { session: app.current.id },
      );
      const list = r.sessions ?? [];
      box.innerHTML = list.length
        ? ""
        : t("no_native");
      for (const s of list) {
        const row = document.createElement("div");
        row.className = "ac-item";
        row.innerHTML = `<span class="n">${esc(s.sessionId.slice(0, 12))}</span>` +
          `<span class="d">${esc(s.title || s.cwd || "")}</span>` +
          `<span class="h">${esc((s.updatedAt || "").slice(0, 16).replace("T", " "))}</span>`;
        row.onclick = async () => {
          ($("dlg-resume") as HTMLDialogElement).close();
          notify("info", t("resume_wait"));
          await call("resume_session", { session: app.current!.id, native_id: s.sessionId })
            .catch((e) => showErr(String(e)));
          await this.chat.open(app.current!.id);
        };
        box.appendChild(row);
      }
    } catch (e) {
      // Agent mort → impossible de lister ; proposer la reprise de la session courante
      box.innerHTML = `<div class="errline">${esc(String(e))}</div>`;
      const row = document.createElement("div");
      row.className = "ac-item";
      row.innerHTML = `<span class="n">${icon("play")}</span><span class="d">${t("resume_current")}</span>`;
      row.onclick = async () => {
        ($("dlg-resume") as HTMLDialogElement).close();
        notify("info", t("resume_wait"));
        await call("resume_session", { session: app.current!.id }).catch((er) => showErr(String(er)));
        await this.chat.open(app.current!.id);
      };
      box.appendChild(row);
    }
  }

  // ---- File d'attente des prompts (queue_list/edit/drop/promote) ----
  // #queue-bar = bande résumé (chips tronquées) ; le bouton horloge +
  // compteur ouvre #queue-panel : messages complets multi-lignes, édition
  // en place via queue_edit, promote/drop par entrée et action secondaire
  // « reprendre dans le composer » (copie + drop).

  /** Panneau d'inspection ouvert — persiste tant que la file n'est pas
   *  vide (la file qui se vide referme tout, édition comprise). */
  private qOpen = false;
  /** Dernier snapshot de queue_list — les re-renders déclenchés par les
   *  boutons l'utilisent sans refetch. */
  private qSnapshot: string[] = [];
  /** Édition inline en cours : index dans la file + texte serveur au
   *  moment de l'ouverture (un écart = entrée déplacée/supprimée
   *  ailleurs → éditeur refermé) + brouillon tapé et curseur, qui
   *  survivent aux rebuilds de refreshQueue pendant la frappe. */
  private qEditing: {
    index: number;
    orig: string;
    draft: string;
    sel?: [number, number];
  } | null = null;
  /** Conns dont le démon ne connaît pas QueueEdit (« parse: unknown
   *  variant ») : l'édition y est désactivée (tooltip) — list/promote/
   *  drop sont des ReqKind plus anciens et restent utilisables. */
  private qEditBroken = new Set<string>();
  /** Capability `queue_steer` du démon (injectée depuis conns.cap) —
   *  le bouton steer des entrées est masqué sur les vieux démons. */
  private qSteerCapable: (conn: string | undefined) => boolean = () => false;
  /** Rebuild du panneau en cours : le blur d'une textarea détachée par
   *  innerHTML ne doit ni sauvegarder ni annuler (l'éditeur est recréé
   *  juste après si l'entrée est toujours là). */
  private qRebuilding = false;

  async refreshQueue() {
    const bar = $("queue-bar");
    if (!app.current) {
      bar.hidden = true;
      this.closeQueuePanel();
      return;
    }
    const sid = app.current.id;
    try {
      const r = await callBg<{ queue: string[] }>("queue_list", { session: sid });
      // La session affichée a pu changer pendant l'appel → ne pas rendre
      // une file qui n'est plus celle de la vue.
      if (app.current?.id !== sid) return;
      this.qSnapshot = r.queue;
      if (!r.queue.length) {
        bar.hidden = true;
        this.closeQueuePanel();
        return;
      }
      bar.hidden = false;
      // Entrée éditée déplacée/supprimée entre-temps (drop/promote ici
      // ou par un autre client) → fermer l'éditeur plutôt que risquer
      // d'écraser un autre slot (le protocole est index-based).
      if (this.qEditing && r.queue[this.qEditing.index] !== this.qEditing.orig) {
        this.qEditing = null;
      }
      this.renderQueueBar(sid);
      this.renderQueuePanel();
    } catch (e) {
      // Transitoire conn : garder le dernier état connu de la file — le
      // prochain refresh (retour conn, nouvelle action) re-synchronisera.
      if (isConnLost(e, app.current?.conn ?? "")) return;
      bar.hidden = true;
      this.closeQueuePanel();
    }
  }

  /** Bande résumé : bouton d'ouverture (horloge + compteur) puis un chip
   *  tronqué par message avec les actions rapides. */
  private renderQueueBar(sid: string) {
    const bar = $("queue-bar");
    const queue = this.qSnapshot;
    // conn RPC : la clé réelle de la session (absente → call() injecte
    // currentConn ou laisse get_conn résoudre la conn par défaut) ;
    // "default" n'est une clé valide que côté client (qEditBroken).
    const conn = app.current?.conn;
    const connKey = conn ?? "default";
    bar.innerHTML = "";
    const open = qIco("clock", t("queue_inspect"));
    open.id = "queue-open";
    open.classList.toggle("on", this.qOpen);
    open.insertAdjacentHTML("beforeend", `<span class="n">${queue.length}</span>`);
    open.onclick = () => this.toggleQueuePanel();
    bar.appendChild(open);
    const broken = this.qEditBroken.has(connKey);
    queue.forEach((txt, i) => {
      const d = document.createElement("div");
      d.className = "q-item";
      const sp = document.createElement("span");
      sp.className = "q-txt";
      sp.textContent = txt;
      sp.title = txt;
      d.appendChild(sp);
      // ✏️ = ouvrir le panneau directement en édition sur cette entrée.
      const e = qIco("edit", broken ? t("queue_edit_unsupported") : t("queue_edit_title"));
      e.disabled = broken;
      e.onclick = () => {
        this.qEditing = { index: i, orig: txt, draft: txt };
        if (!this.qOpen) this.toggleQueuePanel();
        else this.renderQueuePanel();
      };
      // Steer : injection dans le tour en cours SANS l'annuler (≠ ⤒
      // qui annule) — refus → remise en file par le démon. Masqué sur
      // les démons ne connaissant pas queue_steer.
      if (this.qSteerCapable(conn)) {
        const st = qIco("steer", t("queue_steer"));
        st.onclick = () => void this.queueSteer(sid, i, conn);
        d.appendChild(st);
      }
      // Tête de file + cancel → envoi forcé (reste de file préservée)
      const p = qIco("top", t("queue_send_now"));
      p.onclick = () => this.queueCall("queue_promote", { session: sid, index: i, conn });
      const x = qIco("close", t("queue_remove"));
      x.onclick = () => this.queueCall("queue_drop", { session: sid, index: i, conn });
      d.append(e, p, x);
      bar.appendChild(d);
    });
  }

  /** Panneau d'inspection : chaque message en entier (multi-lignes),
   *  dans l'ordre, avec édition inline / promote / drop / reprise dans
   *  le composer. */
  private renderQueuePanel() {
    const panel = $("queue-panel");
    if (!this.qOpen || !this.qSnapshot.length || !app.current) {
      panel.hidden = true;
      return;
    }
    const sid = app.current.id;
    const conn = app.current.conn;
    const connKey = conn ?? "default";
    const queue = this.qSnapshot;
    panel.hidden = false;
    $("qp-count").textContent = String(queue.length);
    const list = $("qp-list");
    // Rebuild pendant une édition (refreshQueue déclenché par un nouveau
    // message en file) : la textarea va être recréée — sauvegarder le
    // curseur pour ne pas sauter en fin de texte à chaque frappe.
    const activeEl = document.activeElement;
    if (
      this.qEditing &&
      activeEl instanceof HTMLTextAreaElement &&
      activeEl.classList.contains("qp-edit")
    ) {
      this.qEditing.sel = [activeEl.selectionStart, activeEl.selectionEnd];
    }
    this.qRebuilding = true;
    list.innerHTML = "";
    this.qRebuilding = false;
    const broken = this.qEditBroken.has(connKey);
    queue.forEach((txt, i) => {
      const row = document.createElement("div");
      row.className = "qp-item";
      const idx = document.createElement("span");
      idx.className = "qp-idx";
      idx.textContent = `${i + 1}`;
      row.appendChild(idx);
      const acts = document.createElement("span");
      acts.className = "qp-acts";
      if (this.qEditing?.index === i && !broken) {
        this.renderQueueEditor(row, acts, sid, conn, i);
      } else {
        const sp = document.createElement("span");
        sp.className = "qp-txt";
        sp.textContent = txt;
        row.appendChild(sp);
        const e = qIco("edit", broken ? t("queue_edit_unsupported") : t("queue_edit_title"));
        e.disabled = broken;
        e.onclick = () => {
          this.qEditing = { index: i, orig: txt, draft: txt };
          this.renderQueuePanel();
        };
        // Action secondaire : copier dans le composer + retirer de la
        // file (perte de position — l'édition inline couvre le cas
        // « corriger avant envoi »).
        const m = qIco("down", t("queue_to_composer"));
        m.onclick = () => {
          this.setInput(txt);
          ($("input") as HTMLTextAreaElement).focus();
          this.queueCall("queue_drop", { session: sid, index: i, conn });
        };
        acts.append(e, m);
      }
      if (this.qSteerCapable(conn)) {
        const st = qIco("steer", t("queue_steer"));
        st.onclick = () => void this.queueSteer(sid, i, conn);
        acts.appendChild(st);
      }
      const p = qIco("top", t("queue_send_now"));
      p.onclick = () => this.queueCall("queue_promote", { session: sid, index: i, conn });
      const x = qIco("close", t("queue_remove"));
      x.onclick = () => this.queueCall("queue_drop", { session: sid, index: i, conn });
      acts.append(p, x);
      // preventDefault sur mousedown de chaque bouton : sinon le clic
      // déplace le focus → blur de la textarea d'édition → done() →
      // rebuild synchrone qui détache le bouton avant son click →
      // action silencieusement perdue (le focus reste, le clic part,
      // et le refreshQueue de l'action referme l'éditeur si l'entrée
      // a bougé).
      for (const b of acts.querySelectorAll<HTMLButtonElement>("button")) {
        b.onmousedown = (e) => e.preventDefault();
      }
      row.appendChild(acts);
      list.appendChild(row);
    });
  }

  /** queue_* puis re-render : l'erreur part en toast plutôt que de
   *  finir en unhandled rejection (conn down, index périmé…). */
  private queueCall(method: string, params: Record<string, unknown>) {
    void call(method, params)
      .then(() => this.refreshQueue())
      .catch((e) => showErr(String(e)));
  }

  /** Bouton « steer » d'une entrée : le démon la sort de la file et
   *  tente un session/prompt concurrent (tour préservé — ≠ promote qui
   *  annule). `delivered` de la réponse est traduit en toast ; la file
   *  renvoyée met la vue à jour immédiatement. */
  private async queueSteer(sid: string, index: number, conn: string | undefined) {
    try {
      const r = await call<{
        delivered: "steer" | "sent" | "requeued" | "held" | "gone";
        reason?: string | null;
        queue?: string[];
      }>("queue_steer", { session: sid, index, conn });
      if (Array.isArray(r.queue)) this.qSnapshot = r.queue;
      switch (r.delivered) {
        case "steer":
          // Refus tardif possible : l'event système du démon déclenche
          // déjà un refresh — ce rattrapage couvre les cas sans push.
          setTimeout(() => void this.refreshQueue(), 2500);
          break;
        case "sent":
          notify("info", t("queue_steer_sent"));
          break;
        case "requeued":
          notify("warn", t("queue_steer_refused"));
          break;
        case "held":
          notify("warn", t(
            r.reason === "compact" ? "queue_steer_compact"
            : r.reason === "asleep" ? "queue_steer_asleep"
            : r.reason === "not_ready" ? "queue_steer_notready"
            : "queue_steer_wait",
          ));
          break;
        case "gone":
          notify("warn", t("queue_edit_failed"));
          break;
      }
    } catch (e) {
      showErr(String(e));
    }
    await this.refreshQueue();
  }

  /** Item en édition inline : textarea + ✓/✗. Sauvegarde = {mod}+Entrée
   *  ou perte de focus ; Échap = annulation. preventDefault sur le
   *  mousedown des boutons : sans ça le clic tirerait le blur de la
   *  textarea (= sauvegarde) avant leur onclick — l'annuler serait perdu. */
  private renderQueueEditor(
    row: HTMLElement,
    acts: HTMLElement,
    sid: string,
    conn: string | undefined,
    i: number,
  ) {
    const ed = this.qEditing!;
    const ta = document.createElement("textarea");
    ta.className = "qp-edit";
    ta.value = ed.draft;
    ta.rows = Math.min(10, Math.max(2, ed.draft.split("\n").length));
    ta.spellcheck = false;
    ta.title = t("queue_edit_hint");
    ta.setAttribute("aria-label", ta.title);
    ta.addEventListener("input", () => {
      ed.draft = ta.value;
    });
    row.appendChild(ta);
    let finished = false;
    const done = (save: boolean) => {
      if (finished) return;
      // Blur zombie : la textarea est détachée par un rebuild de la
      // liste (refreshQueue), pas par un choix de l'utilisateur → la
      // nouvelle instance de l'éditeur prend le relais, rien à faire.
      // isConnected couvre un blur dispatché après détachement (timing
      // webview), indépendamment du flag synchrone qRebuilding.
      if (this.qRebuilding || !ta.isConnected) {
        finished = true;
        return;
      }
      finished = true;
      const text = ta.value;
      // qEditing invalidé entre-temps (entrée déplacée → le rebuild a
      // détaché cette textarea, dont le blur rappelle done) : ne rien
      // écrire, l'index viserait un autre slot.
      const live = this.qEditing === ed;
      this.qEditing = null;
      // Texte vidé → pas de prompt zombie en file : annulation +
      // avertissement plutôt que queue_edit "".
      if (save && live && text !== ed.orig && !text.trim()) {
        notify("warn", t("queue_edit_empty"));
      }
      if (!save || !live || text === ed.orig || !text.trim()) {
        this.renderQueuePanel();
        return;
      }
      call<{ ok: boolean }>("queue_edit", {
        session: sid,
        index: i,
        text,
        conn,
      })
        .then((r) => {
          // Index périmé (entrée déplacée/supprimée par ailleurs).
          if (!r.ok) notify("warn", t("queue_edit_failed"));
        })
        .catch((e) => {
          this.markQueueEditBroken(conn ?? "default", e);
          showErr(String(e));
        })
        .finally(() => this.refreshQueue());
    };
    ta.addEventListener("keydown", (e) => {
      if (e.key === "Escape") {
        e.preventDefault();
        // Scoper la touche : un Échap remontant à window fermerait aussi
        // un éventuel overlay ouvert au-dessus (settings…).
        e.stopPropagation();
        done(false);
      }
      if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
        e.preventDefault();
        done(true);
      }
    });
    ta.addEventListener("blur", () => done(true));
    const save = qIco("check", t("queue_save"));
    save.onmousedown = (e) => e.preventDefault();
    save.onclick = () => done(true);
    const cancel = qIco("close", t("queue_cancel_edit"));
    cancel.onmousedown = (e) => e.preventDefault();
    cancel.onclick = () => done(false);
    acts.append(save, cancel);
    queueMicrotask(() => {
      if (ed.sel) {
        const [s, e2] = ed.sel;
        ta.setSelectionRange(Math.min(s, ta.value.length), Math.min(e2, ta.value.length));
      }
      ta.focus();
    });
  }

  private toggleQueuePanel() {
    if (this.qOpen) {
      this.closeQueuePanel();
    } else if (this.qSnapshot.length && app.current) {
      this.qOpen = true;
      nav.push("queue");
      this.renderQueuePanel();
      $("queue-open")?.classList.add("on");
    }
  }

  /** Ferme le panneau (✕, file vide, session changée, back Android) —
   *  dépile aussi l'entrée nav et l'état « on » du bouton horloge. */
  closeQueuePanel() {
    if (this.qOpen) nav.close("queue");
    this.qOpen = false;
    this.qEditing = null;
    const panel = $("queue-panel");
    if (panel) panel.hidden = true;
    $("queue-open")?.classList.remove("on");
  }

  /** Vieux démon : ReqKind::QueueEdit inconnu → « parse: unknown
   *  variant ». L'édition est désactivée pour la conn de l'appel. */
  private markQueueEditBroken(connKey: string, e: unknown) {
    if (/unknown variant|parse:/i.test(String(e))) {
      this.qEditBroken.add(connKey);
    }
  }

  // ---- /compact : la compaction tourne en arrière-plan côté agent après un
  // idle immédiat — le démon passe la session en "compacting" et retient
  // les prompts en file (visibles dans la barre de file) jusqu'au signal
  // de fin (contexte réduit, message de fin, cancel, timeout). Un prompt
  // livré pendant le compact l'annulerait.

  async sendInput(mode: SendMode = "normal") {
    const txt = ($("input") as HTMLTextAreaElement).value.trim();
    if (!app.current) return;
    // Built-ins client (interceptent avant envoi à l'agent)
    const bm = txt ? /^\/(\w+)(?:\s+(.*))?$/.exec(txt) : null;
    if (bm && BUILTINS().some((b) => b.name === bm[1])) {
      this.setInput("");
      try {
        await this.runBuiltin(bm[1], (bm[2] ?? "").trim());
      } catch (e) {
        showErr(String(e));
      }
      return;
    }
    // Slash-commande ni builtin ni exposée par l'agent (available_commands) :
    // l'envoyer en texte brut coûte un tour pour rien (ex. /fork chez Devin =
    // TUI-only, jamais routé). Avertir localement. Liste vide = agent sans
    // annonce → on ne peut pas trancher, on laisse passer.
    if (
      bm &&
      app.current.available_commands.length > 0 &&
      !app.current.available_commands.some(
        (c) => cmdName(c).toLowerCase() === bm[1].toLowerCase(),
      )
    ) {
      this.setInput("");
      this.chat.sysline(t("cmd_unknown", { cmd: bm[1] }), "warn");
      return;
    }
    // Pièces jointes : envoyer maintenant perdrait les chemins des
    // uploads en vol — avertir plutôt que tronquer le message.
    if (this.attach.pending()) {
      notify("warn", t("att_wait"));
      return;
    }
    const files = this.attach.ready();
    if (!txt && !files.length) return;
    // Le message porte les chemins absolus — l'agent lit les pièces
    // avec ses outils fichiers habituels (bornés au même workspace).
    const atLines = files
      .map((f) => `[fichier joint: ${f.name} → ${f.path}]`)
      .join("\n");
    const payload = txt
      ? atLines
        ? `${txt}\n\n${atLines}`
        : txt
      : atLines;
    const sid = app.current.id; // peut changer pendant l'await
    this.setInput("");
    try {
      await call("send", {
        session: sid,
        text: payload,
        force: mode === "force",
        steer: mode === "steer",
      });
      this.attach.sent(sid);
    } catch (e) {
      showErr(String(e));
      // Envoi refusé : le brouillon n'est vidé que sur succès — rendre
      // le texte à SA session (champ, ou draft garé si l'utilisateur a
      // changé de session entre-temps), fusionné à une frappe plus
      // récente. Pas de regare sous une session qui n'existe plus.
      if (app.current?.id === sid) {
        const v = ($("input") as HTMLTextAreaElement).value;
        this.setInput([v, txt].filter(Boolean).join("\n"));
      } else if (txt && app.lastSessions.some((s) => s.id === sid)) {
        this.drafts.set(
          sid,
          [this.drafts.get(sid), txt].filter(Boolean).join("\n"),
        );
      }
    }
    // Commande slash non-builtin : l'agent l'applique au prochain tour (ex.
    // /compact → "context compacted" n'arrive qu'au message suivant) → hint.
    // (/compact : le démon arme la retenue "compacting" à la livraison.)
    if (bm) {
      this.chat.sysline(t("cmd_sent_hint", { cmd: bm[1] }));
    }
    void this.refreshQueue();
  }

  // ---- Autocomplete des slash-commands ----
  private moveAcSel(dir: number) {
    const items = [...$("cmd-ac").querySelectorAll<HTMLElement>(".ac-item")];
    if (!items.length) return;
    this.acSel = (this.acSel + dir + items.length) % items.length;
    items.forEach((el, i) => el.classList.toggle("sel", i === this.acSel));
    items[this.acSel].scrollIntoView({ block: "nearest" });
  }

  private applyAc(name: string) {
    this.setInput("/" + name + " ");
    ($("input") as HTMLTextAreaElement).focus();
  }

  private refreshAc() {
    const inp = $("input") as HTMLTextAreaElement;
    const ac = $("cmd-ac");
    const v = inp.value;
    const m = /^\/([\w-]*)$/.exec(v);
    if (!m || !app.current) {
      ac.hidden = true;
      return;
    }
    const q = m[1].toLowerCase();
    const matches = [
      ...app.current.available_commands,
      ...BUILTINS(),
    ]
      .filter((c) => cmdName(c).toLowerCase().startsWith(q))
      .slice(0, 12);
    if (!matches.length) {
      ac.hidden = true;
      return;
    }
    ac.innerHTML = "";
    this.acSel = 0;
    matches.forEach((c: CmdInfo, i: number) => {
      const n = cmdName(c);
      const el = document.createElement("div");
      el.className = "ac-item" + (i === 0 ? " sel" : "");
      el.dataset.name = n;
      const h = cmdHint(c);
      el.innerHTML = `<span class="n">/${esc(n)}</span>` +
        (h ? `<span class="h">${esc(h)}</span>` : "") +
        `<span class="d">${esc(cmdDesc(c))}</span>`;
      el.title = cmdDesc(c);
      el.onclick = () => this.applyAc(n);
      ac.appendChild(el);
    });
    ac.hidden = false;
  }

  /** Mode par défaut d'Entrée/bouton Envoyer (préférence paramètres). */
  private defaultMode(): SendMode {
    return sendPref() === "steer" ? "steer" : "normal";
  }

  /** Mode du bouton secondaire = l'inverse de la préférence : file ↔
   *  steer. C'est LA voie « autre mode » sur mobile (pas d'Alt+Entrée). */
  private altMode(): SendMode {
    return this.defaultMode() === "steer" ? "normal" : "steer";
  }

  /** Icône, tooltip, label et visibilité du bouton d'envoi alternatif —
   *  suivent la préférence (sans reload) et l'état de la session.
   *  Rôle steer : pertinent seulement pendant un vrai tour (hors working
   *  un steer tombe dans le flux normal — même effet que Envoyer).
   *  Rôle file : envoi normal — pertinent quand le steer ferait autre
   *  chose, donc agent occupé (working / waiting_input : un normal part
   *  en file). Pendant compact les deux modes sont retenus en file —
   *  bouton sans différence → caché. */
  syncAltBtn() {
    this.attach.syncVisible();
    const b = $("btn-alt") as HTMLButtonElement;
    const queue = this.altMode() === "normal";
    const st = app.current?.state;
    b.hidden = queue
      ? st !== "working" && st !== "waiting_input"
      : st !== "working";
    const ic = queue ? "clock" : "steer";
    if (b.dataset.icon !== ic) {
      b.dataset.icon = ic;
      b.innerHTML = icon(ic);
    }
    // data-i18n-title tenu à jour : applyStatic (changement de langue)
    // retraduit depuis l'attribut, pas depuis b.title.
    const key = queue ? "queue_title" : "steer_title";
    b.dataset.i18nTitle = key;
    b.title = t(key);
    b.setAttribute("aria-label", b.title);
  }

  /** Hints clavier du champ — dépendent du mode d'envoi choisi. */
  refreshHints() {
    // Un placeholder plus long peut wrapper → refit, et le titre de la
    // pill suit la langue courante.
    this.fitInput();
    this.syncAltBtn(); // bouton alternatif : suit la préférence/langue
    if (isAndroid()) return; // placeholder mobile sans raccourcis
    ($("input") as HTMLTextAreaElement).placeholder =
      sendPref() === "steer" ? t("input_ph_steer") : t("input_ph");
  }

  /** Session affichée changée : le brouillon comme les chips de pièces
   *  jointes appartiennent à la session qui les a vus — garés sous son
   *  id, ceux de la session affichée sont réinstallés. */
  sessionChanged() {
    const cur = app.current?.id ?? null;
    if (cur !== this.draftSid) {
      const text = ($("input") as HTMLTextAreaElement).value;
      if (this.draftSid) {
        // Le texte affiché appartient à la session quittée.
        if (text) this.drafts.set(this.draftSid, text);
        else this.drafts.delete(this.draftSid);
      }
      // Réinstaller le brouillon de la session affichée ; un texte tapé
      // sans session ouverte est attribué à celle qui arrive (il visait
      // l'agent à venir).
      let next = cur ? (this.drafts.get(cur) ?? "") : "";
      if (cur) this.drafts.delete(cur);
      if (!this.draftSid && cur && text) next = next ? `${next}\n${text}` : text;
      this.draftSid = cur;
      this.setInput(next);
    }
    this.attach.onSessionChanged();
  }

  /** Session supprimée : son brouillon et ses pièces garées partent
   *  avec elle (session_deleted, ou onSessionGone depuis markGone —
   *  gare d'abord le contenu affiché sous son id). Cas défensif : si la
   *  session visée est encore affichée, son texte est jeté du champ. */
  dropSession(id: string) {
    this.drafts.delete(id);
    if (this.draftSid === id) {
      this.draftSid = null;
      this.setInput("");
    }
    this.attach.dropSession(id);
  }

  /** Capacité « fichiers » du démon courant (injecté depuis conns.cap)
   *  — le + est masqué si le démon ignore write_file/create_path. */
  bindAttachCapable(capable: (conn: string | undefined) => boolean) {
    this.attach.capable = capable;
  }

  /** Capacité « steer de file » du démon (injectée depuis conns.cap)
   *  — bouton steer des entrées masqué si queue_steer est inconnu. */
  bindQueueSteerCapable(capable: (conn: string | undefined) => boolean) {
    this.qSteerCapable = capable;
  }

  init() {
    this.attach.init();
    this.draftSid = app.current?.id ?? null;
    this.mic.init({ setInput: (v) => this.setInput(v) });
    this.refreshHints();
    $("btn-send").onclick = () => this.sendInput(this.defaultMode());
    $("btn-alt").onclick = () => this.sendInput(this.altMode());
    $("input").addEventListener("keydown", (e) => {
      const ac = $("cmd-ac");
      if (!ac.hidden) {
        const sel = ac.querySelector<HTMLElement>(".ac-item.sel");
        if (e.key === "ArrowDown" || e.key === "ArrowUp") {
          e.preventDefault();
          this.moveAcSel(e.key === "ArrowDown" ? 1 : -1);
          return;
        }
        if ((e.key === "Tab" || e.key === "Enter") && sel) {
          e.preventDefault();
          this.applyAc(sel.dataset.name!);
          return;
        }
        if (e.key === "Escape") {
          ac.hidden = true;
          return;
        }
      }
      if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
        e.preventDefault();
        // Ctrl+Entrée (Cmd+Entrée sur macOS) = envoi forcé (annule le
        // tour en cours).
        this.sendInput("force");
        return;
      }
      if (e.key === "Enter" && e.altKey) {
        e.preventDefault();
        // Alt+Entrée = l'autre mode (queue ↔ steer selon la préférence).
        this.sendInput(this.altMode());
        return;
      }
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        this.sendInput(this.defaultMode());
      }
    });
    $("input").addEventListener("input", () => {
      // En dictée : la frappe ne doit pas être écrasée par le prochain
      // partial — le segment live se ré-ancre au curseur.
      this.mic.onUserInput();
      this.refreshAc();
      this.fitInput();
    });
    $("input").addEventListener("blur", () => {
      // délai pour laisser le click sur un item passer
      setTimeout(() => ($("cmd-ac").hidden = true), 150);
    });

    // ✕ du panneau d'inspection de la file (aussi cliqué par le back
    // Android via nav.rawCloseOverlay).
    $("qp-close").onclick = () => this.closeQueuePanel();

    // Bouton « N lignes » : bascule replié ↔ étendu, puis redonne le focus
    // au champ pour continuer la frappe.
    $("btn-expand").onclick = () => {
      this.inpExpanded = !this.inpExpanded;
      this.fitInput();
      ($("input") as HTMLTextAreaElement).focus();
    };
    // Les caps dépendent de la hauteur du fil (resize, rotation mobile).
    window.addEventListener("resize", () => this.fitInput());
    this.fitInput();

    $("btn-cancel").onclick = () =>
      app.current && call("cancel", { session: app.current.id }).catch((e) => showErr(String(e)));

    // Reprise d'un agent mort : spawn + session/load peuvent durer
    // plusieurs secondes — le bouton montre l'attente, l'échec part en
    // toast persistant (dialogue fermé à ce stade).
    const resumeBtn = $("btn-resume") as HTMLButtonElement;
    resumeBtn.onclick = async () => {
      if (!app.current) return;
      resumeBtn.disabled = true;
      resumeBtn.classList.add("spin");
      try {
        await call("resume_session", { session: app.current.id });
        await this.chat.open(app.current.id);
      } catch (e) {
        showErr(String(e));
      } finally {
        resumeBtn.disabled = false;
        resumeBtn.classList.remove("spin");
      }
    };

    // 💤 = endort la session (libère le process ; réveil auto au prochain
    // envoi) — cachée du header sur desktop, utilisée par le menu ⋮ mobile.
    $("btn-sleep").onclick = () =>
      app.current &&
      call("sleep_session", { session: app.current.id })
        .then(() => this.refresh())
        .catch((e) => showErr(String(e)));

    // ⟳ = vrai rechargement : le cache de conversation est ignoré.
    $("btn-refresh").onclick = () => app.current && this.chat.open(app.current.id, true);

    // Double-clic sur le titre → renommage inline. La session est capturée
    // au début : app.current peut changer pendant l'édition (clic sur une
    // autre ligne → blur → done), et le RPC doit viser la session éditée.
    $("chat-title").addEventListener("dblclick", () => {
      if (!app.current) return;
      const sess = app.current;
      const tl = $("chat-title");
      const parent = tl.parentElement;
      const inp = document.createElement("input");
      inp.className = "title-edit";
      inp.value = sess.title;
      tl.replaceWith(inp);
      inp.focus();
      inp.select();
      let finished = false;
      const done = async (save: boolean) => {
        if (finished) return;
        finished = true;
        const v = inp.value.trim();
        // Restaurer le titre quoiqu'il arrive : si l'input a été détaché
        // par un chemin externe, replaceWith ne remettrait rien et le
        // header resterait sans #chat-title (setChatTitle en no-op).
        if (inp.isConnected) inp.replaceWith(tl);
        else if (parent && !tl.isConnected) parent.appendChild(tl);
        if (save && v && v !== sess.title) {
          sess.title = v;
          if (app.current?.id === sess.id) this.chat.setChatTitle();
          await call("rename_session", { session: sess.id, title: v, conn: sess.conn })
            .catch((e) => showErr(String(e)));
          void this.refresh();
        }
      };
      inp.addEventListener("keydown", (e) => {
        if (e.key === "Enter") done(true);
        if (e.key === "Escape") done(false);
      });
      inp.addEventListener("blur", () => done(true));
    });
  }
}
