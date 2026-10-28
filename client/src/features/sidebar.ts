// ---------------------------------------------------------------------------
// Liste des sessions : regroupées par connexion puis groupes nommés —
// badges PR et état de connexion, menu contextuel, renommage inline,
// drag & drop inter-groupes, poignée de redimensionnement.
// ---------------------------------------------------------------------------

import { $, esc, q, showErr } from "../core/dom";
import { call } from "../core/rpc";
import { app } from "../core/state";
import { isAndroid, meshSetMood } from "../core/chrome";
import type { PrEntry, SessionMeta } from "../core/types";
import { t } from "../i18n";
import { dotCls, dotIcon, setAppIcon } from "./status";
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

  /** Renommage inline en cours (compteur : plusieurs lignes peuvent
   *  théoriquement être en édition). Tant qu'il est >0, renderList()
   *  diffère son rebuild — innerHTML="" détacherait l'input de titre
   *  (blur → annulation ou sauvegarde prématurée), ce qui tuait
   *  l'édition dès qu'une session qui travaille émettait un
   *  status_changed. Le dernier done() rejoue le rendu différé. */
  private editCount = 0;
  private rebuildPending = false;

  /** Suppression initiée ici — l'event session_deleted qui suit ne doit
   *  pas re-notifier (la ligne a déjà disparu, l'utilisateur sait). */
  wasPendingDel(id: string) {
    return this.pendingDel.has(id);
  }

  /** Marque des suppressions initiées en masse hors sidebar (ex. delete
   *  d'une connexion → cascade delete_session) : leurs events
   *  session_deleted ne doivent pas spammer de toasts. */
  notePendingDel(ids: string[]) {
    for (const id of ids) this.pendingDel.add(id);
  }

  /** Démarque une suppression en échec (cascade) : la session existe
   *  encore, elle ne doit pas rester filtrée de la liste. */
  clearPendingDel(id: string) {
    this.pendingDel.delete(id);
  }
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
    this.renderList();
    // La liste arrive → l'accueil recalcule son état (0 session → CTA).
    this.chat.updateEmptyState();
  }

  /** Filtre de l'arbre (normalisé : trim + bas de casse) — vide = tout. */
  private filter = "";

  /** Surligne la 1re occurrence du filtre dans un libellé (HTML sûr :
   *  chaque segment passe par esc(), seul le <mark> est injecté). */
  private hl(txt: string): string {
    const f = this.filter;
    txt = txt ?? ""; // champ absent du fil (vieux démon) → "" plutôt que TypeError/"undefined"
    if (!f) return esc(txt);
    const i = txt.toLowerCase().indexOf(f);
    if (i < 0) return esc(txt);
    return esc(txt.slice(0, i)) + "<mark>" + esc(txt.slice(i, i + f.length)) + "</mark>" +
      esc(txt.slice(i + f.length));
  }

  /** Rendu de #session-list depuis app.lastSessions — appelé par refresh()
   *  et à chaque frappe du filtre (pas de RPC : la donnée est déjà là).
   *  Filtre actif : seules les branches avec ≥1 session visible survivent,
   *  tout est déplié, et la 1re occurrence du filtre est surlignée. */
  private renderList() {
    // Une édition de titre est en cours : reconstruire la liste
    // détacherait l'input (blur → fin d'édition) — rebuild différé, le
    // dernier done() rejoue renderList(). Couvre les deux appelants :
    // refresh() (status_changed d'une session qui travaille) et les
    // frappes du filtre. Le flag se consomme au prochain rendu réel.
    if (this.editCount > 0) {
      this.rebuildPending = true;
      return;
    }
    this.rebuildPending = false;
    const el = $("session-list");
    el.innerHTML = "";
    const f = this.filter;
    const hit = (txt: string) => (txt ?? "").toLowerCase().includes(f);
    // Rendu : connexion → groupes nommés (le store client garde les groupes
    // VIDES visibles hors filtre) → sessions non groupées.
    const byConn = new Map<string, SessionMeta[]>();
    for (const s of app.lastSessions) {
      if (this.pendingDel.has(s.id)) continue;
      const c = s.conn ?? "default";
      if (!byConn.has(c)) byConn.set(c, []);
      byConn.get(c)!.push(s);
    }
    // Hors filtre : conns suivies même sans session (ex. serveur down) →
    // header + badge. Filtré : une conn sans session visible disparaît.
    const connKeys = [
      ...new Set([...(f ? [] : this.conns.knownConns()), ...byConn.keys()]),
    ].sort();
    let shown = 0;
    for (const conn of connKeys) {
      const sessions = (byConn.get(conn) ?? []).sort((a, b) => a.title.localeCompare(b.title));
      // Une connexion ou un groupe dont le NOM matche affiche toutes ses
      // sessions (sinon l'utilisateur ne verrait que les titres chanceux).
      const connHit = !!f && hit(conn);
      const grpHit = (g: string) => !!f && hit(g);
      const pass = (s: SessionMeta, gHit: boolean) =>
        !f || connHit || gHit || hit(s.title) || hit(s.agent);
      const grpNames = [
        ...new Set([
          ...(f ? [] : this.groups.stored(conn)),
          ...sessions.map((s) => s.group).filter((g): g is string => !!g),
        ]),
      ].sort();
      // Hors filtre les groupes stockés VIDES restent visibles ; filtré,
      // un groupe sans session visible disparaît.
      const grpRows = grpNames
        .map((grp) => ({ grp, rows: sessions.filter((x) => x.group === grp && pass(x, grpHit(grp))) }))
        .filter((g) => !f || g.rows.length > 0);
      const plain = sessions.filter((x) => !x.group && pass(x, false));
      // Filtré : conn sans aucune session visible → pas de header. Hors
      // filtre : le header s'affiche même sans session (conns suivies).
      if (f && !grpRows.length && !plain.length) continue;
      shown += plain.length + grpRows.reduce((acc, g) => acc + g.rows.length, 0);

      const key = `ac-grp-${conn}`;
      const connClosed = !f && localStorage.getItem(key) === "1";
      const st = this.conns.state(conn) ?? "ok";
      const ch = document.createElement("div");
      ch.className = "grp-head" + (connClosed ? " closed" : "");
      const badge = this.conns.hasCfg(conn)
        ? `<span class="conn-st ${st === "ok" ? "ok" : st === "reconnecting" ? "warn" : "bad"}"` +
          ` title="${q(esc(this.conns.tip(st, conn)))}">${st === "reconnecting" ? icon("refresh") : icon("dot")}</span> `
        : "";
      ch.innerHTML = `<span class="grp-arrow">${icon(connClosed ? "chev-right" : "chev-down")}</span> ${badge}${this.hl(conn)}` +
        `<button class="grp-add" title="${esc(t("new_group"))}">${icon("plus")}</button>`;
      ch.onclick = () => {
        // Sous filtre l'arbre est forcé déplié : toggler persisterait un
        // état invisible à l'écran (+ RPC refresh inutile).
        if (this.filter) return;
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

      for (const { grp, rows } of grpRows) {
        const gkey = `ac-grp-${conn}::${grp}`;
        const grpClosed = !f && (connClosed || localStorage.getItem(gkey) === "1");
        const g = document.createElement("div");
        g.className = "grp-sub" + (grpClosed ? " closed" : "");
        g.innerHTML = `<span class="grp-arrow">${icon(grpClosed ? "chev-right" : "chev-down")}</span> ${this.hl(grp)}` +
          `<button class="grp-add" title="${esc(t("new_sess_grp"))}">${icon("plus")}</button>`;
        g.onclick = () => {
          if (this.filter) return; // idem : pas de toggle invisible sous filtre
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
        bindCtx(g, (x, y) =>
          this.openGroupCtx(x, y, conn, grp, sessions.find((m) => m.group === grp)));
        this.bindDropTarget(g, conn, grp);
        el.appendChild(g);
        for (const s of rows) {
          el.appendChild(this.sessRow(s, conn, grp, grpClosed));
        }
      }
      for (const s of plain) {
        el.appendChild(this.sessRow(s, conn, "", connClosed));
      }
    }
    if (f && !shown) {
      const none = document.createElement("div");
      none.className = "sess-none";
      none.textContent = t("sess_filter_none");
      el.appendChild(none);
    }
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

  /** Menu contextuel d'un groupe nommé (clic droit / appui long) : nouvelle
   *  session dans le groupe / renommer / dissoudre — même composant #sctx
   *  que les menus session/fichier/connexion (action-sheet sur Android).
   *  `ref` = 1er membre affiché, préremplit agent + workspace (comme "+"). */
  private openGroupCtx(
    x: number, y: number, conn: string, grp: string, ref?: SessionMeta,
  ) {
    this.ctx.innerHTML = `<div class="fctx-path">${esc(grp)}</div>`;
    menuBtn(this.ctx, t("new_sess_grp"), "", () => {
      hideSessCtx();
      const apiConn = conn === "default" ? undefined : conn;
      this.newSess.setPendingGroup({ conn: apiConn, name: grp });
      void this.newSess.open(apiConn, ref);
    });
    menuBtn(this.ctx, t("rename_group"), "", () => {
      hideSessCtx();
      this.groups.openDlg("rename", null, conn, grp);
    });
    // Dissoudre dégroupe toutes les sessions membres — destructif →
    // double-clic armé comme ctx_exit / sess-del (label dédié : on
    // dégroupe, on ne supprime pas les sessions).
    const nMembers = app.lastSessions.filter(
      (s) => (s.conn ?? "default") === conn && s.group === grp,
    ).length;
    menuBtn(this.ctx, t("dissolve_group"), "danger", (b) => {
      if (!b.dataset.armed) {
        b.dataset.armed = "1";
        b.textContent = t("ctx_dissolve_armed", { n: nMembers });
        return;
      }
      hideSessCtx();
      void this.groups.dissolve(conn, grp)
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
    inp.className = "title-edit";
    inp.value = s.title;
    span.replaceWith(inp);
    row.draggable = false; // sinon un drag depuis l'input part en drag de ligne
    inp.focus();
    inp.select();
    // Tant que l'édition est ouverte, refresh() diffère son rebuild de
    // liste (un status_changed d'une session qui travaille ne doit pas
    // tuer la saisie) ; done() le déclenche à la fin.
    this.editCount++;
    let finished = false;
    const done = async (save: boolean) => {
      if (finished) return;
      finished = true;
      if (!isAndroid()) row.draggable = true;
      const v = inp.value.trim();
      if (inp.isConnected) inp.replaceWith(span);
      const changed = !!(save && v && v !== s.title);
      if (changed) {
        s.title = v;
        span.textContent = v;
        if (app.current?.id === s.id) {
          app.current.title = v;
          this.chat.setChatTitle();
        }
        await call("rename_session", { session: s.id, title: v, conn: s.conn })
          .catch((e) => showErr(String(e)));
      }
      this.editCount--;
      // Refresh seulement si utile : sauvegarde (retri par titre +
      // resync complet) ou rebuilds différés à rejouer — renderList()
      // suffit, les données sont déjà à jour (lastSessions et filtre ont
      // continué de bouger pendant l'édition). Un Escape/blur sans
      // changement ne vaut pas un round-trip.
      if (changed) void this.refresh();
      else if (this.editCount === 0 && this.rebuildPending) this.renderList();
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
      ? `<span class="sess-pr" title="${q(esc(prs.map((p) => `${p.repo}#${p.pr}${p.title ? ` — ${p.title}` : ""}${p.status ? ` (${p.status})` : ""}`).join("\n")))}">${prs.map((p) => `#${p.pr}`).join(" ")}</span>`
      : "";
    div.innerHTML = `<span class="dot st-${dotCls(s.state)}" title="${q(esc(s.state))}">${dotIcon(s.state)}</span><span class="t">${this.hl(s.title)}</span>${prHtml}<span class="a">${this.hl(s.agent)}</span><button class="sess-del" title="Supprimer">${icon("close")}</button>`;
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
    // Filtre de l'arbre : recherche par nom de session / agent / groupe /
    // connexion. Rendu local instantané — pas de debounce, pas de RPC.
    const fIn = $("sess-filter-input") as HTMLInputElement;
    const fClr = $("sess-filter-clear");
    const applyFilter = () => {
      this.filter = fIn.value.trim().toLowerCase();
      fClr.hidden = !this.filter;
      this.renderList();
    };
    fIn.addEventListener("input", applyFilter);
    fIn.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && fIn.value) {
        fIn.value = "";
        applyFilter();
        e.stopPropagation(); // ne pas fermer un menu ctx en même temps
      }
    });
    fClr.addEventListener("click", () => {
      fIn.value = "";
      applyFilter();
      fIn.focus();
    });
    // Ctrl/Cmd+F sans session ouverte → filtre d'arbre (avec session, le
    // handler de search.ts prend la main pour la recherche d'historique).
    // Pas sous un <dialog> ouvert : l'input invisible volerait les frappes.
    window.addEventListener("keydown", (e) => {
      if ((e.ctrlKey || e.metaKey) && (e.key === "f" || e.key === "F") &&
          !app.current && !document.querySelector("dialog[open]")) {
        e.preventDefault();
        fIn.focus();
        fIn.select();
      }
    });

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
