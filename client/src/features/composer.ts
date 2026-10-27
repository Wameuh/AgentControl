// ---------------------------------------------------------------------------
// Composer : champ de saisie, envois (normal/steer/force), autocomplétion
// des slash-commandes, commandes builtin client, file d'attente des
// prompts, picker de sessions natives (reprise).
// ---------------------------------------------------------------------------

import { $, esc, qBtn, showErr } from "../core/dom";
import { call } from "../core/rpc";
import { app } from "../core/state";
import type { CmdInfo, SendMode } from "../core/types";
import { t } from "../i18n";
import { cmdDesc, cmdHint, cmdName } from "./modelbar";
import type { ChatView } from "./chat-view";

// ---- Commandes built-in (toujours dispo, exécutées côté client) ----
const BUILTINS = (): { name: string; description: string; hint: string }[] => [
  { name: "model", description: t("builtin_model"), hint: "<model>" },
  { name: "resume", description: t("builtin_resume"), hint: "" },
  { name: "cancel", description: t("builtin_cancel"), hint: "" },
  { name: "fork", description: t("builtin_fork"), hint: "" },
  { name: "clear", description: t("builtin_clear"), hint: "" },
  { name: "new", description: t("builtin_new"), hint: "" },
];

export class Composer {
  private chat!: ChatView;
  /** Rafraîchit la sidebar (injecté). */
  refresh: () => Promise<void> = async () => {};
  private acSel = 0;

  bind(chat: ChatView) {
    this.chat = chat;
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
      row.innerHTML = `<span class="n">↻</span><span class="d">${t("resume_current")}</span>`;
      row.onclick = async () => {
        ($("dlg-resume") as HTMLDialogElement).close();
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
      r.queue.forEach((txt, i) => {
        const d = document.createElement("div");
        d.className = "q-item";
        const sp = document.createElement("span");
        sp.className = "q-txt";
        sp.textContent = txt;
        sp.title = txt;
        d.appendChild(sp);
        // ✎ = reprendre dans le champ + retirer de la file
        const e = qBtn("✎", "Edit");
        e.onclick = () => {
          ($("input") as HTMLTextAreaElement).value = txt;
          call("queue_drop", { session: app.current!.id, index: i }).then(() => this.refreshQueue());
        };
        // ⏫ = tête de file + cancel → envoi forcé (reste de file préservée)
        const p = qBtn("⏫", "Send now");
        p.onclick = () =>
          call("queue_promote", { session: app.current!.id, index: i }).then(() => this.refreshQueue());
        const x = qBtn("✕", "Remove");
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
      ($("input") as HTMLTextAreaElement).value = "";
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
      ($("input") as HTMLTextAreaElement).value = "";
      this.chat.sysline(t("cmd_unknown", { cmd: bm[1] }));
      return;
    }
    ($("input") as HTMLTextAreaElement).value = "";
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
    const inp = $("input") as HTMLTextAreaElement;
    inp.value = "/" + name + " ";
    $("cmd-ac").hidden = true;
    inp.focus();
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

  init() {
    $("btn-send").onclick = () => this.sendInput("normal");
    $("btn-steer").onclick = () => this.sendInput("steer");
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
        this.sendInput("steer"); // Alt+Entrée = injecter après le tool call courant
        return;
      }
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        this.sendInput("normal");
      }
    });
    $("input").addEventListener("input", () => this.refreshAc());
    $("input").addEventListener("blur", () => {
      // délai pour laisser le click sur un item passer
      setTimeout(() => ($("cmd-ac").hidden = true), 150);
    });

    $("btn-cancel").onclick = () =>
      app.current && call("cancel", { session: app.current.id }).catch((e) => showErr(String(e)));

    $("btn-resume").onclick = () =>
      app.current &&
      call("resume_session", { session: app.current.id })
        .then(() => this.chat.open(app.current!.id))
        .catch((e) => showErr(String(e)));

    // ⟳ = vrai rechargement : le cache de conversation est ignoré.
    $("btn-refresh").onclick = () => app.current && this.chat.open(app.current.id, true);

    // Double-clic sur le titre → renommage inline
    $("chat-title").addEventListener("dblclick", () => {
      if (!app.current) return;
      const tl = $("chat-title");
      const inp = document.createElement("input");
      inp.id = "title-edit";
      inp.value = app.current.title;
      tl.replaceWith(inp);
      inp.focus();
      inp.select();
      const done = async (save: boolean) => {
        const v = inp.value.trim();
        inp.replaceWith(tl);
        if (save && v && v !== app.current!.title) {
          app.current!.title = v;
          tl.textContent = v;
          await call("rename_session", { session: app.current!.id, title: v })
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
