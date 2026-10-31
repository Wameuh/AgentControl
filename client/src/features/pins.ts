// ---------------------------------------------------------------------------
// Messages épinglés par session : liste persistée côté démon (pinned.json)
// — rechargée ici, donc visible de tout client et survivant au reload.
// Pin/unpin : icône .pin-tag injectée sur chaque .ev.msg (survol desktop,
// semi-visible permanente sur Android), toggle pin_message/unpin_message.
// Panneau d'accès rapide (btn-pins du header, entrée du sheet ⋮ mobile) :
// extrait + saut au message (ChatView.jumpToSeq). Un épinglé dont l'event
// source a quitté le journal (/clear, rebind, compaction) est `dead` :
// extrait toujours lisible, saut impossible — jamais purgé silencieusement.
// ---------------------------------------------------------------------------

import { $, esc, q, showErr } from "../core/dom";
import { bgErr, call, callBg } from "../core/rpc";
import { app } from "../core/state";
import { t } from "../i18n";
import { icon } from "../ui/icons";

interface PinEntry {
  seq: number;
  ts: string;
  type: string;
  text: string;
  pinned_at: string;
  live: boolean;
}

export interface PinsDeps {
  /** Capability `pinned` du démon de cette conn (daemon_status). */
  capable: (conn: string) => boolean;
  /** Saut au seq dans la vue conversation (vue ancrée si nécessaire). */
  jumpToSeq: (seq: number) => Promise<boolean>;
}

const fmtDay = (ts: string) => {
  const d = new Date(ts);
  return Number.isNaN(d.getTime())
    ? ""
    : d.toLocaleString(undefined, {
        month: "short",
        day: "numeric",
        hour: "2-digit",
        minute: "2-digit",
      });
};

export class Pins {
  private panel = $("pins-panel");
  private list = $("pp-list");
  private count = $("pp-count");
  private eventsEl = $("events");
  private deps: PinsDeps = {
    capable: () => false,
    jumpToSeq: () => Promise.resolve(false),
  };
  private items: PinEntry[] = [];
  /** Session dont `items` est la liste chargée. */
  private sid = "";
  /** Sid avec un list_pinned en vol — dedup des déclencheurs. */
  private loading = "";

  bind(d: Partial<PinsDeps>) {
    Object.assign(this.deps, d);
  }

  get isOpen() {
    return !this.panel.hidden;
  }

  init() {
    $("btn-pins").onclick = () => this.togglePanel();
    $("pp-close").onclick = () => this.close();
    // Toggle pin : délégation sur #events — les .pin-tag sont posés par
    // markNode au rendu, jamais besoin de re-câbler chaque nœud.
    this.eventsEl.addEventListener("click", (e) => {
      const tag = (e.target as HTMLElement).closest<HTMLElement>(".pin-tag");
      if (!tag) return;
      e.stopPropagation();
      const ev = tag.closest<HTMLElement>(".ev");
      const seq = Number(ev?.dataset.seq ?? 0);
      if (seq) void this.toggle(seq);
    });
    // Clics dans le panneau : ligne → saut (pins live) ; ✕ → désépingler.
    this.list.addEventListener("click", (e) => {
      const un = (e.target as HTMLElement).closest<HTMLElement>(".pin-unpin");
      if (un) {
        e.stopPropagation();
        void this.unpin(Number(un.dataset.seq ?? 0));
        return;
      }
      const row = (e.target as HTMLElement).closest<HTMLElement>(".pin-row[data-seq]");
      if (row && !row.classList.contains("dead")) {
        void this.deps.jumpToSeq(Number(row.dataset.seq));
      }
    });
  }

  togglePanel() {
    if (this.isOpen) {
      this.close();
      return;
    }
    const s = app.current;
    if (!s) return;
    if (!this.deps.capable(s.conn ?? "default")) {
      showErr(t("pins_unsupported"));
      return;
    }
    this.panel.hidden = false;
    this.renderPanel();
    // Liste potentiellement modifiée par un autre client → refresh à
    // chaque ouverture (petit RPC, pas de push dédié).
    void this.load();
  }

  close() {
    this.panel.hidden = true;
  }

  /** Chargement de la liste de la session affichée — idempotent, dedup
   *  d'un appel en vol sur le même sid. */
  private async load() {
    const s = app.current;
    if (!s) {
      this.items = [];
      this.sid = "";
      this.renderPanel();
      return;
    }
    if (!this.deps.capable(s.conn ?? "default")) {
      // Vieux démon : bouton masqué par le gate — rien à charger, et un
      // appel rendrait "unknown variant" 60 s plus tard.
      this.items = [];
      this.sid = s.id;
      this.renderPanel();
      return;
    }
    if (this.loading === s.id) return;
    this.loading = s.id;
    try {
      const r = await callBg<{ pinned?: PinEntry[] }>("list_pinned", {
        session: s.id,
      });
      if (app.current?.id !== s.id) return; // session changée entre-temps
      this.items = r.pinned ?? [];
      this.sid = s.id;
      this.applyAll();
      this.renderPanel();
    } catch (e) {
      // Load de fond : coupure transitoire → silence (sessionOpened au
      // retour de la conn rejouera) ; échec applicatif → toast.
      if (app.current?.id === s.id) bgErr("list_pinned", e, s.conn ?? "");
    } finally {
      if (this.loading === s.id) this.loading = "";
    }
  }

  /** La session ouverte a fini son chargement — (re)lit ses épinglés. */
  sessionOpened() {
    void this.load();
  }

  /** Le contexte journal affiché a changé : autre session (la liste de
   *  la nouvelle arrive via sessionOpened), /clear-rebind sur la même
   *  (les épinglés survivent côté démon mais deviennent morts → reload),
   *  ou session disparue (nettoyage). */
  sessionChanged() {
    const same = app.current && app.current.id === this.sid;
    this.items = [];
    if (!app.current) {
      this.sid = "";
      this.close();
      this.renderPanel();
      return;
    }
    if (same) void this.load();
    else this.renderPanel();
  }

  /** État épinglé d'un nœud rendu : classe `pinned` + titre du pin-tag.
   *  Seuls les épinglés LIVE comptent — un épinglé mort au seq qui
   *  collide avec un event du nouveau cycle ne doit pas le marquer. */
  markNode(el: HTMLElement) {
    if (!el.classList.contains("msg")) return;
    const seq = Number(el.dataset.seq ?? 0);
    const pinned =
      seq !== 0 && this.items.some((p) => p.seq === seq && p.live);
    el.classList.toggle("pinned", pinned);
    let tag = el.querySelector<HTMLElement>(".pin-tag");
    if (!tag) {
      // Démon sans la capability : pas d'épingle du tout — elle ne
      // ferait qu'afficher "non supporté" au clic (sur Android elle
      // est semi-visible en permanence, le bruit serait maximal).
      if (!this.deps.capable(app.current?.conn ?? "default")) return;
      tag = document.createElement("button");
      (tag as HTMLButtonElement).type = "button";
      tag.className = "pin-tag";
      tag.innerHTML = icon("pin");
      el.appendChild(tag);
    }
    tag.title = t(pinned ? "pin_remove" : "pin_add");
  }

  /** Re-marque les messages rendus — après chargement/mutation de la
   *  liste (les nœuds peuvent avoir été rendus avant sa connaissance). */
  private applyAll() {
    for (const el of this.eventsEl.querySelectorAll<HTMLElement>(
      ".ev.msg[data-seq]",
    )) {
      this.markNode(el);
    }
  }

  /** Toggle épinglé/désépinglé sur le seq du message cliqué — la réponse
   *  RPC porte la liste complète recalculée, appliquée directement. */
  private async toggle(seq: number) {
    const s = app.current;
    if (!s) return;
    if (!this.deps.capable(s.conn ?? "default")) {
      showErr(t("pins_unsupported"));
      return;
    }
    const isPinned = this.items.some((p) => p.seq === seq && p.live);
    try {
      const r = await call<{ pinned?: PinEntry[] }>(
        isPinned ? "unpin_message" : "pin_message",
        { session: s.id, seq },
      );
      if (app.current?.id !== s.id) return;
      this.items = r.pinned ?? [];
      this.sid = s.id;
      this.applyAll();
      this.renderPanel();
    } catch (e) {
      showErr(String(e));
    }
  }

  private async unpin(seq: number) {
    const s = app.current;
    if (!s || !seq) return;
    try {
      const r = await call<{ pinned?: PinEntry[] }>("unpin_message", {
        session: s.id,
        seq,
      });
      if (app.current?.id !== s.id) return;
      this.items = r.pinned ?? [];
      this.sid = s.id;
      this.applyAll();
      this.renderPanel();
    } catch (e) {
      showErr(String(e));
    }
  }

  private renderPanel() {
    this.count.textContent = this.items.length ? String(this.items.length) : "";
    this.list.innerHTML = "";
    if (!this.items.length) {
      const d = document.createElement("div");
      d.className = "pp-empty";
      d.textContent = t("pins_none");
      this.list.appendChild(d);
      return;
    }
    for (const p of this.items) {
      const row = document.createElement("div");
      row.className = "pin-row" + (p.live ? "" : " dead");
      row.dataset.seq = String(p.seq);
      const excerpt = p.text.replace(/\s+/g, " ").trim();
      row.innerHTML =
        `<span class="pin-ico">${icon("pin")}</span>` +
        `<span class="pin-type">${esc(p.type)}</span>` +
        `<span class="pin-ex" title="${q(esc(excerpt.slice(0, 500)))}">${esc(excerpt.slice(0, 160))}</span>` +
        `<span class="pin-ts">${esc(fmtDay(p.pinned_at))}</span>` +
        (p.live
          ? ""
          : `<span class="pin-dead" title="${q(esc(t("pins_dead")))}">${icon("warn")}</span>`) +
        `<button class="pin-unpin" data-seq="${p.seq}" title="${q(esc(t("pin_remove")))}">${icon("close")}</button>`;
      if (p.live) row.title = t("pins_jump");
      this.list.appendChild(row);
    }
  }
}
