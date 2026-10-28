// ---------------------------------------------------------------------------
// Composer : champ de saisie, envois (normal/steer/force), autocomplétion
// des slash-commandes, commandes builtin client, file d'attente des
// prompts, picker de sessions natives (reprise).
// ---------------------------------------------------------------------------

import { $, esc, notify, showErr } from "../core/dom";
import { icon, qIco } from "../ui/icons";
import { call } from "../core/rpc";
import { app } from "../core/state";
import { isAndroid } from "../core/chrome";
import type { CmdInfo, SendMode } from "../core/types";
import { t } from "../i18n";
import { cmdDesc, cmdHint, cmdName } from "./modelbar";
import { sendPref } from "./settings";
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
  /** Rafraîchit la sidebar (injecté). */
  refresh: () => Promise<void> = async () => {};
  private acSel = 0;
  /** Champ déplié manuellement (bouton expand) — replie dès que le
      contenu rentre sous le cap normal. */
  private inpExpanded = false;

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
  async refreshQueue() {
    const bar = $("queue-bar");
    if (!app.current) {
      bar.hidden = true;
      return;
    }
    try {
      const r = await call<{ queue: string[] }>("queue_list", {
        session: app.current.id,
      });
      bar.innerHTML = "";
      bar.hidden = r.queue.length === 0;
      if (r.queue.length) bar.innerHTML = icon("clock");
      r.queue.forEach((txt, i) => {
        const d = document.createElement("div");
        d.className = "q-item";
        const sp = document.createElement("span");
        sp.className = "q-txt";
        sp.textContent = txt;
        sp.title = txt;
        d.appendChild(sp);
        // Reprendre dans le champ + retirer de la file
        const e = qIco("edit", "Edit");
        e.onclick = () => {
          this.setInput(txt);
          call("queue_drop", { session: app.current!.id, index: i }).then(() => this.refreshQueue());
        };
        // Tête de file + cancel → envoi forcé (reste de file préservée)
        const p = qIco("top", "Send now");
        p.onclick = () =>
          call("queue_promote", { session: app.current!.id, index: i }).then(() => this.refreshQueue());
        const x = qIco("close", "Remove");
        x.onclick = () =>
          call("queue_drop", { session: app.current!.id, index: i }).then(() => this.refreshQueue());
        d.append(e, p, x);
        bar.appendChild(d);
      });
    } catch {
      bar.hidden = true;
    }
  }

  // ---- /compact : la compaction tourne en arrière-plan côté agent après un
  // idle immédiat — le démon passe la session en "compacting" et retient
  // les prompts en file (visibles dans la barre de file) jusqu'au signal
  // de fin (contexte réduit, message de fin, cancel, timeout). Un prompt
  // livré pendant le compact l'annulerait.

  async sendInput(mode: SendMode = "normal") {
    const txt = ($("input") as HTMLTextAreaElement).value.trim();
    if (!txt || !app.current) return;
    // Built-ins client (interceptent avant envoi à l'agent)
    const bm = /^\/(\w+)(?:\s+(.*))?$/.exec(txt);
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
    this.setInput("");
    await call("send", {
      session: app.current.id,
      text: txt,
      force: mode === "force",
      steer: mode === "steer",
    }).catch((e) => showErr(String(e)));
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
    $("cmd-ac").hidden = true;
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

  init() {
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
      if (e.key === "Enter" && e.ctrlKey) {
        e.preventDefault();
        this.sendInput("force"); // Ctrl+Entrée = envoi forcé (annule le tour en cours)
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
      this.refreshAc();
      this.fitInput();
    });
    $("input").addEventListener("blur", () => {
      // délai pour laisser le click sur un item passer
      setTimeout(() => ($("cmd-ac").hidden = true), 150);
    });

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
