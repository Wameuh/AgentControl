// ---------------------------------------------------------------------------
// Vue « Repos » de la sidebar : pull requests du registre démon regroupées
// par dépôt — numéro, titre, statut (open/draft/conflict), branche, session
// responsable croisée avec list_sessions. Clic sur la PR → GitHub (shellOpen) ;
// clic sur la session → ouvre la session. Réutilise le filtre de la sidebar.
// ---------------------------------------------------------------------------

import { $, esc, q } from "../core/dom";
import { shellOpen } from "../core/chrome";
import { t } from "../i18n";
import { dotCls, dotIcon } from "./status";
import { icon } from "../ui/icons";
import { nav } from "../ui/nav";
import type { ConnLive, PrEntry, SessionMeta } from "../core/types";

/** Ligne du `pr_list` fusionné : entrée du registre OU marqueur d'échec
 *  par connexion (`{conn, conn_error}` — même convention que
 *  list_sessions : un démon sans pr_list remonte son erreur ainsi). */
export type PrRow = PrEntry & { conn_error?: boolean };

export interface PrListDeps {
  /** Ouvre une session (comportement sidebar : ferme le panneau options). */
  openSession: (id: string) => Promise<void>;
  /** État live d'une connexion (« ok » = le démon répond). */
  connState: (k: string) => ConnLive | undefined;
  /** Surlignage <mark> du filtre courant (partagé avec la vue sessions). */
  hl: (txt: string) => string;
}

const CLOSED = new Set(["merged", "closed"]);
const skey = (conn: string | undefined, id: string) => `${conn ?? "default"} ${id}`;

export class PrListView {
  private deps!: PrListDeps;
  private rows: PrRow[] = [];
  private sessions = new Map<string, SessionMeta>();
  private filter = "";
  /** L'appel pr_list lui-même a rejeté (les échecs par connexion sont des
   *  marqueurs conn_error dans rows, pas ce flag). */
  private failed = false;
  /** Premier setData reçu — avant ça, la vue affiche « chargement » et
   *  non « aucune PR » (le flash vide au boot serait trompeur). */
  private loaded = false;

  bind(d: PrListDeps) {
    this.deps = d;
  }

  setFilter(f: string) {
    this.filter = f;
  }

  /** PRs ouvertes visibles dans le compteur de l'onglet Repos. */
  openCount() {
    return this.rows.filter((r) => !r.conn_error && !CLOSED.has(r.status ?? "open")).length;
  }

  /** Alimente la vue à chaque refresh de la sidebar : lignes du registre
   *  fusionné + snapshot des sessions (dots d'état, titres à jour). */
  setData(rows: PrRow[], sessions: SessionMeta[], failed: boolean) {
    this.rows = rows;
    this.failed = failed;
    this.loaded = true;
    this.sessions.clear();
    for (const s of sessions) this.sessions.set(skey(s.conn, s.id), s);
  }

  private sessOf(p: PrEntry): SessionMeta | undefined {
    return p.session ? this.sessions.get(skey(p.conn, p.session)) : undefined;
  }

  /** Ligne d'info non bloquante en tête de liste (démon trop ancien,
   *  connexion injoignable, registre inaccessible). */
  private note(txt: string, warn: boolean): HTMLElement {
    const d = document.createElement("div");
    d.className = "pr-note" + (warn ? " warn" : "");
    d.innerHTML = `${icon("warn")}<span>${esc(txt)}</span>`;
    return d;
  }

  private empty(txt: string): HTMLElement {
    const d = document.createElement("div");
    d.className = "sess-none";
    d.textContent = txt;
    return d;
  }

  /** Chip session : vivante → gemme d'état + titre cliquable ; référencée
   *  mais absente de list_sessions → libellé brut estompé (supprimée ou
   *  connexion down — on garde l'info du registre). */
  private sessChip(p: PrEntry): HTMLElement {
    const s = this.sessOf(p);
    const el = document.createElement(s ? "button" : "span");
    el.className = "pr-sess" + (s ? "" : " gone") + (s?.offline ? " offline" : "");
    if (!s) {
      el.textContent = p.session || "—";
      el.title = t("pr_sess_gone");
      return el;
    }
    el.innerHTML = `<span class="dot st-${dotCls(s.state)}" title="${q(esc(s.state))}">${dotIcon(s.state)}</span>` +
      `<span class="stxt">${this.deps.hl(s.title)}</span>`;
    el.title = `${s.title} — ${s.agent}`;
    (el as HTMLButtonElement).onclick = (e) => {
      e.stopPropagation();
      void this.deps.openSession(s.id);
      nav.closeDrawer(); // mobile : la drawer laisserait le chat masqué
    };
    return el;
  }

  private prRow(p: PrEntry, unassigned: boolean): HTMLElement {
    const st = p.status || "open";
    const stTxt =
      st === "open" ? t("pr_st_open")
      : st === "draft" ? t("pr_st_draft")
      : st === "conflict" ? t("pr_st_conflict")
      : st === "merged" ? t("pr_st_merged")
      : st === "closed" ? t("pr_st_closed")
      : st;
    const div = document.createElement("div");
    div.className = "pr";
    div.title = `${p.repo}#${p.pr} — ${stTxt}${p.via ? ` · ${p.via}` : ""}` +
      `${p.branch ? `\n${p.branch}` : ""}${p.note ? `\n${p.note}` : ""}\n${t("pr_open_title")}`;
    const top = document.createElement("div");
    top.className = "pr-top";
    top.innerHTML = `<span class="pr-num">#${p.pr}</span>` +
      `<span class="pr-t">${this.deps.hl(p.title ?? "")}</span>` +
      `<span class="pr-st st-${["open", "draft", "conflict"].includes(st) ? st : "other"}">${esc(stTxt)}</span>`;
    div.appendChild(top);
    const sub = document.createElement("div");
    sub.className = "pr-sub";
    // Non attribuées : pas de chip session — le dépôt remplace l'info.
    sub.innerHTML = unassigned
      ? `<span class="pr-repo-lbl">${this.deps.hl(p.repo)}</span>` +
        (p.branch ? `<span class="pr-branch">${this.deps.hl(p.branch)}</span>` : "")
      : (p.branch ? `<span class="pr-branch">${this.deps.hl(p.branch)}</span>` : "");
    if (!unassigned) sub.appendChild(this.sessChip(p));
    div.appendChild(sub);
    div.onclick = () => {
      if (p.repo.includes("/")) shellOpen(`https://github.com/${p.repo}/pull/${p.pr}`);
    };
    return div;
  }

  /** Header de groupe (dépôt ou section « non attribuées ») — pliable,
   *  état persisté par clé localStorage, forcé ouvert sous filtre. */
  private groupHead(labelHtml: string, key: string, count: number, extraCls = ""): HTMLElement {
    const closed = !this.filter && localStorage.getItem(key) === "1";
    const h = document.createElement("div");
    h.className = "grp-head pr-repo" + extraCls + (closed ? " closed" : "");
    h.innerHTML = `<span class="grp-arrow">${icon(closed ? "chev-right" : "chev-down")}</span> ` +
      `${labelHtml}<span class="pr-n">${count}</span>`;
    h.onclick = () => {
      // Sous filtre tout est déplié — pas de toggle d'un état invisible.
      if (this.filter) return;
      localStorage.setItem(key, closed ? "0" : "1");
      this.render();
    };
    return h;
  }

  render() {
    const el = $("pr-list");
    el.innerHTML = "";
    const f = this.filter;
    const hit = (txt?: string) => (txt ?? "").toLowerCase().includes(f);

    if (this.failed) el.appendChild(this.note(t("pr_fetch_failed"), true));
    // Notes émises → pas d'état « aucune PR » en plus : il nierait le
    // problème réel (échec ≠ registre vide).
    let noted = this.failed;
    const open: PrEntry[] = [];
    const unassigned: PrEntry[] = [];
    for (const r of this.rows) {
      if (r.conn_error) {
        // Conn OK mais pr_list en échec → démon trop ancien pour le
        // registre ; conn down → simple injoignabilité.
        const alive = (this.deps.connState(r.conn ?? "") ?? "ok") === "ok";
        el.appendChild(this.note(
          t(alive ? "pr_conn_old" : "pr_conn_off", { conn: r.conn ?? "?" }), alive));
        noted = true;
        continue;
      }
      if (CLOSED.has(r.status ?? "open")) continue;
      if (r.session) open.push(r);
      else unassigned.push(r);
    }

    // Regroupement par dépôt : clé minuscule (GitHub est insensible à la
    // casse — même convention que la clé du registre démon), libellé =
    // casse d'origine ; repos triés alpha, PR par numéro décroissant.
    const byRepo = new Map<string, { label: string; rows: PrEntry[] }>();
    const match = (p: PrEntry) => {
      const s = this.sessOf(p);
      return !f || hit(p.repo) || hit(`#${p.pr}`) || hit(p.title) ||
        hit(p.branch) || hit(s?.title) || hit(s?.agent);
    };
    for (const p of open) {
      if (!match(p)) continue;
      const k = p.repo.toLowerCase();
      let g = byRepo.get(k);
      if (!g) byRepo.set(k, (g = { label: p.repo, rows: [] }));
      g.rows.push(p);
    }
    let shown = 0;
    for (const g of [...byRepo.values()].sort((a, b) => a.label.localeCompare(b.label))) {
      g.rows.sort((a, b) => b.pr - a.pr);
      const key = `ac-prrepo-${g.label.toLowerCase()}`;
      const closed = !f && localStorage.getItem(key) === "1";
      el.appendChild(this.groupHead(
        `<span class="pr-repo-ic">${icon("repo")}</span>${this.deps.hl(g.label)}`, key, g.rows.length));
      shown += g.rows.length;
      if (!closed) for (const p of g.rows) el.appendChild(this.prRow(p, false));
    }

    // Section « non attribuées » : PR du registre sans session.
    const unF = unassigned.filter(match);
    if (unF.length) {
      unF.sort((a, b) => b.pr - a.pr);
      const key = "ac-prrepo-?unassigned";
      const closed = !f && localStorage.getItem(key) === "1";
      el.appendChild(this.groupHead(
        `<span class="pr-repo-ic">${icon("pr")}</span>${esc(t("pr_unassigned"))}`,
        key, unF.length, " unatt"));
      shown += unF.length;
      if (!closed) for (const p of unF) el.appendChild(this.prRow(p, true));
    }

    if (!shown && !noted) {
      el.appendChild(this.empty(
        !this.loaded ? t("pr_loading") : f ? t("pr_none_filt") : t("pr_none")));
    }
  }
}
