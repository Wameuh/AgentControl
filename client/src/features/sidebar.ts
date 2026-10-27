// ---------------------------------------------------------------------------
// Liste des sessions : regroupées par connexion puis groupes nommés —
// badges PR et état de connexion, menu contextuel, renommage inline,
// drag & drop inter-groupes, poignée de redimensionnement.
// ---------------------------------------------------------------------------

import { $, esc, showErr } from "../core/dom";
import { call } from "../core/rpc";
import { app } from "../core/state";
import { isAndroid, meshSetMood } from "../core/chrome";
import type { PrEntry, SessionMeta } from "../core/types";
import { t } from "../i18n";
import { dotIcon, setAppIcon } from "./status";
import { icon } from "../ui/icons";
import { bindCtx, hideSessCtx, menuBtn, placeMenu } from "../ui/ctxmenu";
import { nav } from "../ui/nav";
import type { ChatView } from "./chat-view";
import type { ConnectionManager } from "./connections";
import type { GroupStore } from "./groups";
import type { NewSessionDialog } from "./new-session";
import type { SettingsPanel } from "./settings";

const DTYPE = "application/x-ac-session";

export class Sidebar {
  private conns!: ConnectionManager;
  private groups!: GroupStore;
  private newSess!: NewSessionDialog;
  private chat!: ChatView;
  private settings!: SettingsPanel;

  /** PRs ouvertes par session ("conn\0sid") — rempli par refresh ;
   *  vide si le démon ne connaît pas pr_list (vieux démon → pas de badge). */
  private prBySess = new Map<string, PrEntry[]>();
  /** delete_session en vol OU terminée : l'id reste tombstoné après
   *  succès (un refresh dont le snapshot list_sessions date d'avant la
   *  fin de la suppression ne doit pas réinjecter la ligne — un id
   *  supprimé ne réapparaît jamais) ; retiré seulement en cas d'échec,
   *  où la session existe encore. */
  private pendingDel = new Set<string>();
  private ctx = $("sctx");

  bind(conns: ConnectionManager, groups: GroupStore, newSess: NewSessionDialog, chat: ChatView, settings: SettingsPanel) {
    this.conns = conns;
    this.groups = groups;
    this.newSess = newSess;
    this.chat = chat;
    this.settings = settings;
  }

  /** Ouvrir une session depuis la sidebar = quitter le panneau options
   *  (il recouvre #chat — la session s'afficherait dessous, invisible ;
   *  close() est un no-op quand il est déjà fermé). */
  private openSession(id: string): Promise<void> {
    this.settings.close();
    return this.chat.open(id);
  }

  /** Ligne de session marquée active immédiatement au clic — avant tout
   *  chargement, sinon l'ancienne session semble encore sélectionnée. */
  setActiveRow(id: string) {
    for (const r of document.querySelectorAll<HTMLElement>("#session-list .sess")) {
      r.classList.toggle("active", r.dataset.sid === id);
    }
  }

  async refresh() {
    // pr_list en parallèle : un démon mort/vieux ne doit pas retarder la
    // sidebar (erreur → pas de badge, c'est tout).
    const [raw, prs] = await Promise.all([
      call<SessionMeta[]>("list_sessions").catch((e) => {
        setAppIcon("grey"); // démon injoignable → aucun agent joignable
        this.chat.updateEmptyState(); // l'accueil ne doit pas attendre la liste
        throw e;
      }),
      call<PrEntry[]>("pr_list").catch(() => [] as PrEntry[]),
    ]);
    this.prBySess.clear();
    for (const p of prs) {
      if (!p.session || p.status === "merged" || p.status === "closed") continue;
      const k = `${p.conn ?? "default"}${p.session}`;
      if (!this.prBySess.has(k)) this.prBySess.set(k, []);
      this.prBySess.get(k)!.push(p);
    }
    // Marqueur Rust `conn_error` : la sonde list_sessions d'une conn a
    // échoué (timeout 8 s) → la marquer down (une seule fois : déjà down,
    // markDead re-rendererait refresh en boucle).
    const list = raw.filter((s) => {
      if (this.pendingDel.has(s.id)) return false;
      if (!(s as { conn_error?: boolean }).conn_error) return true;
      const k = s.conn ?? "";
      if ((this.conns.state(k) ?? "ok") === "ok") this.conns.markDead(k);
      return false;
    });
    // Conn down : ses sessions n'arrivent plus — réafficher le dernier cache
    // connu (grisé) au lieu de faire disparaître le serveur.
    for (const [k, st] of this.conns.liveEntries()) {
      if (st !== "ok" && !list.some((s) => (s.conn ?? "default") === k)) {
        for (const s of app.lastSessions.filter(
          (x) => (x.conn ?? "default") === k && !this.pendingDel.has(x.id),
        )) {
          list.push({ ...s, offline: true });
        }
      }
    }
    app.lastSessions = list;
    const live = list.filter((s) =>
      !s.offline && s.state !== "dead" && s.state !== "interrupted" && s.state !== "asleep");
    setAppIcon(
      live.some((s) => s.state === "waiting_input")
        ? "red"
        : live.some(
              (s) =>
                s.state === "working" ||
                s.state === "running" ||
                s.state === "starting",
            )
          ? "gold"
          : live.some((s) => s.state === "compacting")
            ? "violet"
            : live.length > 0 ? "base" : "grey",
    );
    // Session courante disparue alors que sa connexion répond encore
    // (sessions du même serveur présentes) → supprimée côté serveur.
    this.chat.markGoneIfMissing(list);
    // Lumières "agents" du fond : une par session active (les sessions
    // cachées d'une conn down ne comptent pas).
    const n = list.filter(
      (s) =>
        !s.offline &&
        (s.state === "working" || s.state === "waiting_input" || s.state === "compacting"),
    ).length;
    meshSetMood({ agents: Math.min(n, 4), intensity: 0.55 });
    const el = $("session-list");
    el.innerHTML = "";
    // Rendu : connexion → groupes nommés (le store client garde les groupes
    // VIDES visibles) → sessions non groupées.
    const byConn = new Map<string, SessionMeta[]>();
    for (const s of list) {
      const c = s.conn ?? "default";
      if (!byConn.has(c)) byConn.set(c, []);
      byConn.get(c)!.push(s);
    }
    // Conns suivies même sans session (ex. serveur down) → header + badge.
    for (const conn of [...new Set([...this.conns.knownConns(), ...byConn.keys()])].sort()) {
      const sessions = (byConn.get(conn) ?? []).sort((a, b) => a.title.localeCompare(b.title));
      const key = `ac-grp-${conn}`;
      const connClosed = localStorage.getItem(key) === "1";
      const st = this.conns.state(conn) ?? "ok";
      const ch = document.createElement("div");
      ch.className = "grp-head" + (connClosed ? " closed" : "");
      const badge = this.conns.hasCfg(conn)
        ? `<span class="conn-st ${st === "ok" ? "ok" : st === "reconnecting" ? "warn" : "bad"}"` +
          ` title="${q(esc(this.conns.tip(st, conn)))}">${st === "reconnecting" ? icon("refresh") : icon("dot")}</span> `
        : "";
      ch.innerHTML = `<span class="grp-arrow">${icon(connClosed ? "chev-right" : "chev-down")}</span> ${badge}${esc(conn)}` +
        `<button class="grp-add" title="${esc(t("new_group"))}">${icon("plus")}</button>`;
      ch.onclick = () => {
        localStorage.setItem(key, connClosed ? "0" : "1");
        void this.refresh();
      };
      ch.querySelector(".grp-add")!.addEventListener("click", (e) => {
        e.stopPropagation();
        this.groups.openDlg("new", null, conn);
      });
      // Badge ≠ ok → clic = reconnecter maintenant (retry simple).
      ch.querySelector(".conn-st")?.addEventListener("click", (e) => {
        e.stopPropagation();
        if ((this.conns.state(conn) ?? "ok") !== "ok") this.conns.retry(conn);
      });
      // Déposer sur le header connexion = dégrouper la session
      this.bindDropTarget(ch, conn, undefined);
      bindCtx(ch, (x, y) => this.conns.openCtx(x, y, conn));
      el.appendChild(ch);

      const grpNames = [
        ...new Set([
          ...this.groups.stored(conn),
          ...sessions.map((s) => s.group).filter((g): g is string => !!g),
        ]),
      ].sort();
      for (const grp of grpNames) {
        const gkey = `ac-grp-${conn}::${grp}`;
        const grpClosed = connClosed || localStorage.getItem(gkey) === "1";
        const g = document.createElement("div");
        g.className = "grp-sub" + (grpClosed ? " closed" : "");
        g.innerHTML = `<span class="grp-arrow">${icon(grpClosed ? "chev-right" : "chev-down")}</span> ${esc(grp)}` +
          `<button class="grp-add" title="${esc(t("new_sess_grp"))}">${icon("plus")}</button>`;
        g.onclick = () => {
          localStorage.setItem(gkey, localStorage.getItem(gkey) === "1" ? "0" : "1");
          void this.refresh();
        };
        // + : nouvel agent dans le groupe — agent + workspace du 1er membre
        // préremplis (sessions triées par titre → "le premier" = premier affiché).
        g.querySelector(".grp-add")!.addEventListener("click", (e) => {
          e.stopPropagation();
          const apiConn = conn === "default" ? undefined : conn;
          this.newSess.setPendingGroup({ conn: apiConn, name: grp });
          void this.newSess.open(apiConn, sessions.find((x) => x.group === grp));
        });
        bindCtx(g, () => this.groups.openDlg("rename", null, conn, grp));
        this.bindDropTarget(g, conn, grp);
        el.appendChild(g);
        for (const s of sessions.filter((x) => x.group === grp)) {
          el.appendChild(this.sessRow(s, conn, grp, grpClosed));
        }
      }
      for (const s of sessions.filter((x) => !x.group)) {
        el.appendChild(this.sessRow(s, conn, "", connClosed));
      }
    }
    // La liste arrive → l'accueil recalcule son état (0 session → CTA).
    this.chat.updateEmptyState();
  }

  /** Cible de drop pour une session : conn = câble, group undefined = dégrouper. */
  private bindDropTarget(el: HTMLElement, conn: string, group: string | undefined) {
    el.addEventListener("dragover", (e) => {
      if (e.dataTransfer?.types.includes(DTYPE)) {
        e.preventDefault();
        el.classList.add("drop-hint");
      }
    });
    el.addEventListener("dragleave", () => el.classList.remove("drop-hint"));
    el.addEventListener("drop", async (e) => {
      e.preventDefault();
      el.classList.remove("drop-hint");
      try {
        const d = JSON.parse(e.dataTransfer!.getData(DTYPE)) as { id: string; conn: string };
        if (d.conn !== conn) return; // pas de déplacement inter-serveur
        const s = app.lastSessions.find((x) => x.id === d.id);
        if (!s || (s.group ?? "") === (group ?? "")) return;
        await this.groups.applyAssign(s, group);
        void this.refresh();
      } catch {
        /* donnée étrangère : ignorer */
      }
    });
  }

  /** Menu contextuel session (clic droit / appui long) : rename / fork /
   *  groupe / exit — fixe au point cliqué sur desktop, action-sheet ancrée
   *  en bas sur Android. */
  private openSessionCtx(x: number, y: number, s: SessionMeta, row: HTMLElement) {
    this.ctx.innerHTML = `<div class="fctx-path">${esc(s.title)}</div>`;
    menuBtn(this.ctx, t("ctx_rename"), "", () => {
      hideSessCtx();
      this.renameSessRow(s, row);
    });
    // conn: s.conn — la session peut vivre sur une autre connexion que
    // celle affichée ; sans ça call() routerait vers le mauvais démon.
    const forkBtn = menuBtn(this.ctx, t("ctx_fork"), "", () => {
      hideSessCtx();
      if (app.current?.id === s.id) this.chat.sysline(t("fork_started"));
      void call<{ session: string }>("fork_session", { session: s.id, conn: s.conn })
        .then(async (r) => {
          await this.refresh();
          if (r.session) await this.openSession(r.session);
        })
        .catch((e) => showErr(String(e)));
    });
    // Fork = respawn depuis la session native — impossible sans id natif.
    forkBtn.disabled = !s.native_session_id;
    menuBtn(this.ctx, t("ctx_group"), "", () => {
      hideSessCtx();
      this.groups.openDlg("session", s);
    });
    // Endormir = libère le process agent (la session native reste
    // reprenable — réveil auto au prochain envoi). Sans objet si la
    // session n'a déjà plus de process (dead/interrupted/asleep).
    const sleepBtn = menuBtn(this.ctx, t("ctx_sleep"), "", () => {
      hideSessCtx();
      void call("sleep_session", { session: s.id, conn: s.conn })
        .then(() => this.refresh())
        .catch((e) => showErr(String(e)));
    });
    sleepBtn.disabled = !!s.offline ||
      s.state === "asleep" || s.state === "dead" || s.state === "interrupted";
    // Exit = tue le process agent (kill_session) — la session reste, reprenable.
    // Action destructive → double-clic armé comme delete_path / sess-del.
    menuBtn(this.ctx, t("ctx_exit"), "danger", (b) => {
      if (!b.dataset.armed) {
        b.dataset.armed = "1";
        b.textContent = t("ctx_confirm");
        return;
      }
      hideSessCtx();
      void call("kill_session", { session: s.id, conn: s.conn })
        .then(() => this.refresh())
        .catch((e) => showErr(String(e)));
    });
    placeMenu(this.ctx, "sctx", x, y);
  }

  /** Renommage inline du titre dans la ligne session (même pattern que le
   *  double-clic sur chat-title). */
  private renameSessRow(s: SessionMeta, row: HTMLElement) {
    const span = row.querySelector<HTMLElement>(".t");
    if (!span) return;
    const inp = document.createElement("input");
    inp.id = "title-edit";
    inp.value = s.title;
    span.replaceWith(inp);
    row.draggable = false; // sinon un drag depuis l'input part en drag de ligne
    inp.focus();
    inp.select();
    // blur ré-entrant : retirer l'input focusé (Escape, ou refresh
    // qui rebuild la liste) redéclenche blur → sans garde, une annulation
    // persisterait le texte tapé. !isConnected = ligne déjà reconstruite.
    let finished = false;
    const done = async (save: boolean) => {
      if (finished || !inp.isConnected) { finished = true; return; }
      finished = true;
      if (!isAndroid()) row.draggable = true;
      const v = inp.value.trim();
      inp.replaceWith(span);
      if (save && v && v !== s.title) {
        s.title = v;
        span.textContent = v;
        if (app.current?.id === s.id) {
          app.current.title = v;
          $("chat-title").textContent = v;
        }
        await call("rename_session", { session: s.id, title: v, conn: s.conn })
          .catch((e) => showErr(String(e)));
        void this.refresh();
      }
    };
    inp.addEventListener("click", (e) => e.stopPropagation());
    inp.addEventListener("keydown", (e) => {
      if (e.key === "Enter") done(true);
      if (e.key === "Escape") done(false);
    });
    inp.addEventListener("blur", () => done(true));
  }

  private sessRow(s: SessionMeta, conn: string, grp: string, closed: boolean): HTMLElement {
    const div = document.createElement("div");
    div.dataset.sid = s.id;
    div.className = "sess" + (grp ? " in-grp" : "") + (s.offline ? " offline" : "") +
      (app.current?.id === s.id ? " active" : "");
    if (closed) div.style.display = "none";
    const prs = this.prBySess.get(`${conn}${s.id}`) ?? [];
    const prHtml = prs.length
      ? `<span class="sess-pr" title="${esc(prs.map((p) => `${p.repo}#${p.pr}${p.title ? ` — ${p.title}` : ""}${p.status ? ` (${p.status})` : ""}`).join("\n"))}">${prs.map((p) => `#${p.pr}`).join(" ")}</span>`
      : "";
    div.innerHTML = `<span class="dot st-${s.state}" title="${esc(s.state)}">${dotIcon(s.state)}</span><span class="t">${esc(s.title)}</span>${prHtml}<span class="a">${esc(s.agent)}</span><button class="sess-del" title="Supprimer">${icon("close")}</button>`;
    div.onclick = () => {
      void this.openSession(s.id);
      // Mobile : fermer le drawer après sélection.
      nav.closeDrawer();
    };
    bindCtx(div, (x, y) => this.openSessionCtx(x, y, s, div));
    if (!isAndroid()) {
      div.draggable = true;
      div.addEventListener("dragstart", (e) => {
        e.dataTransfer!.setData(DTYPE, JSON.stringify({ id: s.id, conn }));
        e.dataTransfer!.effectAllowed = "move";
      });
    }
    const del = div.querySelector<HTMLElement>(".sess-del")!;
    del.addEventListener("click", (e) => {
      e.stopPropagation();
      if (!del.dataset.armed) {
        del.dataset.armed = "1";
        del.textContent = t("delete_armed");
        setTimeout(() => { del.dataset.armed = ""; del.innerHTML = icon("close"); }, 2500);
        return;
      }
      // Optimiste : la ligne disparaît sans attendre le démon (kill de
      // l'agent + purge des fichiers peuvent durer). L'event
      // session_deleted confirmera ; en cas d'erreur la ligne revient.
      this.pendingDel.add(s.id);
      if (app.current?.id === s.id) {
        // Session affichée : markGone fait le ménage complet (conv
        // cachée, current, gem, boutons, badge "gone", updateHeader).
        // L'event session_deleted trouvera app.current déjà null → il
        // ne repassera pas par là.
        this.chat.markGone();
      } else {
        this.chat.dropConv(s.id);
      }
      div.remove();
      void this.refresh();
      void call("delete_session", { session: s.id, conn: s.conn })
        .catch((er) => {
          showErr(String(er));
          this.pendingDel.delete(s.id);
          // Échec → la session existe encore : la réinjecter dans le
          // cache liste pour qu'elle réapparaisse au refresh (même
          // offline si la connexion est tombée entre-temps) — sauf si
          // sa connexion a été supprimée entre-temps.
          if (this.conns.hasCfg(s.conn ?? "local") &&
              !app.lastSessions.some((x) => x.id === s.id)) {
            app.lastSessions.push(s);
          }
        })
        .finally(() => void this.refresh());
    });
    return div;
  }

  init() {
    // Fermeture du menu ctx : clic ailleurs / Échap / scroll de la liste.
    // Un clic DANS le menu ne doit pas le fermer avant l'action du bouton.
    this.ctx.addEventListener("click", (e) => e.stopPropagation());
    window.addEventListener("click", hideSessCtx);
    window.addEventListener("keydown", (e) => {
      if (e.key === "Escape") hideSessCtx();
    });
    $("session-list").addEventListener("scroll", hideSessCtx);

    // Redimensionnement de la sidebar : drag sur la poignée bord droit.
    // Largeur persistée dans localStorage.
    const bar = $("sidebar");
    const grip = $("sidebar-resize");
    const clamp = (x: number) => Math.min(520, Math.max(180, x));
    const saved = Number(localStorage.getItem("sidebar-w"));
    if (saved) {
      bar.style.width = clamp(saved) + "px";
      bar.style.minWidth = clamp(saved) + "px";
    }
    grip.addEventListener("mousedown", (e) => {
      e.preventDefault();
      grip.classList.add("drag");
      document.body.style.userSelect = "none";
      const move = (ev: MouseEvent) => {
        const w = clamp(ev.clientX);
        bar.style.width = w + "px";
        bar.style.minWidth = w + "px";
      };
      const up = (ev: MouseEvent) => {
        document.removeEventListener("mousemove", move);
        document.removeEventListener("mouseup", up);
        grip.classList.remove("drag");
        document.body.style.userSelect = "";
        localStorage.setItem("sidebar-w", String(clamp(ev.clientX)));
      };
      document.addEventListener("mousemove", move);
      document.addEventListener("mouseup", up);
    });
  }
}
