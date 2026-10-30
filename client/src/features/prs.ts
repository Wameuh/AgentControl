// ---------------------------------------------------------------------------
// Vue « Repos » de la sidebar : pull requests du registre démon regroupées
// par dépôt — numéro, titre, statut (open/draft/conflict), branche, session
// responsable croisée avec list_sessions ; merged/closed dans une section
// « terminées » repliée (l'état live est resynchronisé par pr_sync quand
// l'onglet est actif). Clic sur la PR → GitHub (shellOpen) ; clic sur la
// session → ouvre la session. Réutilise le filtre de la sidebar.
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
/** Seuil d'échecs de sync → « non résoluble »
 *  (proto::PR_UNRESOLVED_AFTER — à garder aligné). */
const UNRESOLVED_AFTER = 3;
/** Entrée jamais résolue par le sync (repo fantôme, hôte mort) — les
 *  statuts finaux merged/closed gardent leur vérité. */
export function prUnresolved(p: PrEntry): boolean {
  return !CLOSED.has(p.status ?? "open") && (p.sync_fails ?? 0) >= UNRESOLVED_AFTER;
}
/** Cap de la section « terminées » — un long historique reste
 *  atteignable via le filtre partagé sans alourdir le DOM. */
const DONE_CAP = 40;
const skey = (conn: string | undefined, id: string) => `${conn ?? "default"} ${id}`;

/** Badge de la référence : `#N` github, `!N` gitlab, `c#N`/`c#I<8>` gerrit
 *  (même convention que `forge_badge` côté proto/acctl). */
export function prBadge(p: PrEntry): string {
  const f = p.forge || "github";
  if (f === "gitlab") return `!${p.pr}`;
  if (f === "gerrit") {
    return p.pr ? `c#${p.pr}` : `c#${(p.change_id ?? "").slice(0, 9)}`;
  }
  return `#${p.pr}`;
}

/** Lien web de la référence : `url` du registre d'abord, sinon
 *  reconstruite par forge (hôtes auto-hébergés inclus). */
function prUrl(p: PrEntry): string {
  if (p.url) return p.url;
  const f = p.forge || "github";
  const host = p.host || (f === "github" ? "github.com" : f === "gitlab" ? "gitlab.com" : "");
  if (f === "gitlab" && host) return `https://${host}/${p.repo}/-/merge_requests/${p.pr}`;
  if (f === "gerrit" && host) {
    return p.pr
      ? `https://${host}/c/${p.repo}/+/${p.pr}`
      : `https://${host}/q/${p.change_id ?? ""}`;
  }
  return p.repo.includes("/") ? `https://github.com/${p.repo}/pull/${p.pr}` : "";
}

/** Libellé de groupe : `o/r` github, `host:g/p` ailleurs (plusieurs
 *  forges peuvent servir le même chemin de projet). */
export function repoLabel(p: PrEntry): string {
  const f = p.forge || "github";
  if (f === "github") return p.repo;
  return p.repo ? `${p.host ?? ""}:${p.repo}` : (p.host ?? "?");
}

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

  /** PRs ouvertes visibles dans le compteur de l'onglet Repos — les
   *  « non résolubles » n'y comptent pas (rien d'actionnable). */
  openCount() {
    return this.rows.filter(
      (r) => !r.conn_error && !CLOSED.has(r.status ?? "open") && !prUnresolved(r),
    ).length;
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
    const unres = prUnresolved(p);
    const stTxt = unres
      ? t("pr_st_unresolved")
      : st === "open" ? t("pr_st_open")
      : st === "draft" ? t("pr_st_draft")
      : st === "conflict" ? t("pr_st_conflict")
      : st === "merged" ? t("pr_st_merged")
      : st === "closed" ? t("pr_st_closed")
      : st;
    const div = document.createElement("div");
    div.className = "pr" + (CLOSED.has(st) ? " pr-done" : "") + (unres ? " pr-unres" : "");
    const openHost = p.host ||
      ({ github: "GitHub", gitlab: "gitlab.com", gerrit: "Gerrit" } as Record<string, string>)[p.forge || "github"] || "GitHub";
    div.title = `${repoLabel(p)} ${prBadge(p)} — ${stTxt}${p.via ? ` · ${p.via}` : ""}` +
      `${unres ? `\n${t("pr_unres_hint", { n: p.sync_fails ?? 0 })}` : ""}` +
      `${p.branch ? `\n${p.branch}` : ""}${p.note ? `\n${p.note}` : ""}\n${t("pr_open_title", { host: openHost })}`;
    const top = document.createElement("div");
    top.className = "pr-top";
    top.innerHTML = `<span class="pr-num">${esc(prBadge(p))}</span>` +
      `<span class="pr-t">${this.deps.hl(p.title ?? "")}</span>` +
      `<span class="pr-st st-${unres ? "unres" : ["open", "draft", "conflict", "merged", "closed"].includes(st) ? st : "other"}">${esc(stTxt)}</span>`;
    div.appendChild(top);
    const sub = document.createElement("div");
    sub.className = "pr-sub";
    // Non attribuées : pas de chip session — le dépôt remplace l'info.
    sub.innerHTML = unassigned
      ? `<span class="pr-repo-lbl">${this.deps.hl(repoLabel(p))}</span>` +
        (p.branch ? `<span class="pr-branch">${this.deps.hl(p.branch)}</span>` : "")
      : (p.branch ? `<span class="pr-branch">${this.deps.hl(p.branch)}</span>` : "");
    if (!unassigned) sub.appendChild(this.sessChip(p));
    div.appendChild(sub);
    div.onclick = () => {
      const u = prUrl(p);
      if (u) shellOpen(u);
    };
    return div;
  }

  /** État plié d'un groupe persisté par clé localStorage — `defaultClosed`
   *  inverse le défaut (section « terminées » : historique replié). */
  private isClosed(key: string, defaultClosed = false): boolean {
    return !this.filter &&
      (localStorage.getItem(key) ?? (defaultClosed ? "1" : "0")) === "1";
  }

  /** Header de groupe (dépôt ou section « non attribuées ») — pliable,
   *  état persisté par clé localStorage, forcé ouvert sous filtre. */
  private groupHead(labelHtml: string, key: string, count: number | string, extraCls = "", defaultClosed = false): HTMLElement {
    const closed = this.isClosed(key, defaultClosed);
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
    const done: PrEntry[] = [];
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
      if (CLOSED.has(r.status ?? "open")) { done.push(r); continue; }
      if (r.session) open.push(r);
      else unassigned.push(r);
    }

    // Regroupement par dépôt : clé minuscule (GitHub est insensible à la
    // casse — même convention que la clé du registre démon), libellé =
    // casse d'origine ; repos triés alpha, PR par numéro décroissant.
    const byRepo = new Map<string, { label: string; rows: PrEntry[] }>();
    const match = (p: PrEntry) => {
      const s = this.sessOf(p);
      return !f || hit(repoLabel(p)) || hit(prBadge(p)) || hit(p.title) ||
        hit(p.branch) || hit(p.change_id) || hit(s?.title) || hit(s?.agent);
    };
    for (const p of open) {
      if (!match(p)) continue;
      const k = repoLabel(p).toLowerCase();
      let g = byRepo.get(k);
      if (!g) byRepo.set(k, (g = { label: repoLabel(p), rows: [] }));
      g.rows.push(p);
    }
    let shown = 0;
    for (const g of [...byRepo.values()].sort((a, b) => a.label.localeCompare(b.label))) {
      g.rows.sort((a, b) => b.pr - a.pr);
      const key = `ac-prrepo-${g.label.toLowerCase()}`;
      const closed = this.isClosed(key);
      // Tag forge explicite hors github (défaut, icône colorée suffit) —
      // `gitlab`/`gerrit` lèvent l'ambiguïté d'un chemin de repo brut.
      const f = (g.rows[0].forge || "github").toLowerCase();
      const tag = f === "github" ? "" : `<span class="pr-forge fg-${esc(f)}">${esc(f)}</span>`;
      el.appendChild(this.groupHead(
        `<span class="pr-repo-ic fg-${esc(f)}">${icon("repo")}</span>${tag}${this.deps.hl(g.label)}`,
        key, g.rows.length));
      shown += g.rows.length;
      if (!closed) for (const p of g.rows) el.appendChild(this.prRow(p, false));
    }

    // Section « non attribuées » : PR du registre sans session.
    const unF = unassigned.filter(match);
    if (unF.length) {
      unF.sort((a, b) => b.pr - a.pr);
      const key = "ac-prrepo-?unassigned";
      const closed = this.isClosed(key);
      el.appendChild(this.groupHead(
        `<span class="pr-repo-ic">${icon("pr")}</span>${esc(t("pr_unassigned"))}`,
        key, unF.length, " unatt"));
      shown += unF.length;
      if (!closed) for (const p of unF) el.appendChild(this.prRow(p, true));
    }

    // Section « terminées » : merged/closed — la vue reflète l'état
    // réel de la forge au lieu de masquer ces PR. Repliée par défaut
    // (historique), bornée aux DONE_CAP plus récemment mises à jour.
    const doneF = done.filter(match);
    if (doneF.length) {
      doneF.sort(
        (a, b) =>
          (b.updated_at ?? "").localeCompare(a.updated_at ?? "") || b.pr - a.pr,
      );
      const shownDone = doneF.slice(0, DONE_CAP);
      const key = "ac-prrepo-?done";
      const closed = this.isClosed(key, true);
      el.appendChild(this.groupHead(
        `<span class="pr-repo-ic">${icon("check")}</span>${esc(t("pr_done"))}`,
        key, doneF.length > DONE_CAP ? `${DONE_CAP}+` : doneF.length,
        " unatt", true));
      shown += shownDone.length;
      if (!closed) for (const p of shownDone) el.appendChild(this.prRow(p, true));
    }

    if (!shown && !noted) {
      el.appendChild(this.empty(
        !this.loaded ? t("pr_loading") : f ? t("pr_none_filt") : t("pr_none")));
    }
  }
}
