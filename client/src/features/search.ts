// ---------------------------------------------------------------------------
// Recherche plein-texte dans l'historique de la session affichée.
// Le scan vit côté démon (search_events sur le journal) : la vue ne rend
// qu'une fenêtre bornée d'events — filtrer le DOM en raterait l'essentiel.
// Résultats paginés via le curseur next_before ; un clic centre la vue sur
// l'event (ChatView.jumpToSeq → fenêtre de contexte, mode ancré).
// ---------------------------------------------------------------------------

import { $, esc, showErr } from "../core/dom";
import { app } from "../core/state";
import { call } from "../core/rpc";
import { t } from "../i18n";

interface SearchMatch {
  seq: number;
  ts: string;
  type: string;
  excerpt: string;
}
interface SearchPage {
  matches: SearchMatch[];
  next_before: number;
  done: boolean;
  scanned: number;
}

/** Dépendances injectées — ChatView n'est pas importé (cycle). */
export interface SearchDeps {
  jumpToSeq: (seq: number) => Promise<boolean>;
}

const PAGE_LIMIT = 100;

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

export class SessionSearch {
  private wrap = $("search-wrap");
  private input = $("search-input") as HTMLInputElement;
  private count = $("search-count");
  private list = $("search-results");
  private deps: SearchDeps = { jumpToSeq: () => Promise.resolve(false) };
  private matches: SearchMatch[] = [];
  private idx = -1;
  private reqSeq = 0; // invalidation des réponses périmées
  private nextBefore = 0;
  private done = true;
  private loading = false;
  private debounce: ReturnType<typeof setTimeout> | undefined;

  bind(d: SearchDeps) {
    this.deps = d;
  }

  get isOpen() {
    return !this.wrap.hidden;
  }

  init() {
    $("btn-search").onclick = () => this.toggle();
    $("search-close").onclick = () => this.close();
    $("search-prev").onclick = () => void this.step(-1);
    $("search-next").onclick = () => void this.step(1);
    this.input.addEventListener("input", () => {
      clearTimeout(this.debounce);
      this.debounce = setTimeout(() => void this.run(), 300);
    });
    this.input.addEventListener("keydown", (e) => {
      if (e.key === "Enter") {
        e.preventDefault();
        void this.step(e.shiftKey ? -1 : 1);
      } else if (e.key === "Escape") {
        e.preventDefault();
        this.close();
      }
    });
    // Ctrl/Cmd+F ouvre la recherche de la session affichée (remplace le
    // find webview natif, inopérant ici puisque l'historique est paginé).
    window.addEventListener("keydown", (e) => {
      if ((e.ctrlKey || e.metaKey) && (e.key === "f" || e.key === "F")) {
        e.preventDefault();
        this.open();
      }
    });
    // Clic sur une ligne de résultat → saut (délégation, bouton "more" inclus).
    this.list.addEventListener("click", (e) => {
      const row = (e.target as HTMLElement).closest<HTMLElement>("[data-idx]");
      if (row) {
        void this.goto(Number(row.dataset.idx));
        return;
      }
      if ((e.target as HTMLElement).closest("#search-more")) void this.more();
    });
  }

  toggle() {
    if (this.isOpen) this.close();
    else this.open();
  }

  open() {
    if (!app.current) return;
    this.wrap.hidden = false;
    this.input.focus();
    this.input.select();
    if (this.input.value.trim() && !this.matches.length) void this.run();
  }

  close() {
    this.wrap.hidden = true;
    this.reqSeq++; // annule un scan en vol
    clearTimeout(this.debounce);
  }

  /** Le journal affiché a changé (autre session, /clear, suppression,
   *  connexion perdue) : les résultats indexent l'ancien journal — reset
   *  complet, la barre se ferme. */
  onSessionChanged() {
    this.matches = [];
    this.idx = -1;
    this.nextBefore = 0;
    this.done = true;
    this.count.textContent = "";
    this.list.hidden = true;
    this.list.innerHTML = "";
    this.close();
  }

  private showCount() {
    const n = this.matches.length;
    this.count.textContent = !n
      ? t("search_none")
      : `${this.idx + 1}/${n}${this.done ? "" : "+"}`;
  }

  /** Rendu de la liste — appelé après chaque page de résultats. */
  private renderList() {
    this.list.innerHTML = "";
    this.matches.forEach((m, i) => {
      const row = document.createElement("button");
      row.className = "sr-row" + (i === this.idx ? " cur" : "");
      row.dataset.idx = String(i);
      const ts = fmtDay(m.ts);
      row.innerHTML =
        `<span class="sr-type">${esc(m.type)}</span>` +
        (ts ? `<span class="sr-ts">${esc(ts)}</span>` : "") +
        `<span class="sr-ex">${esc(m.excerpt)}</span>`;
      this.list.appendChild(row);
    });
    if (!this.done) {
      const more = document.createElement("button");
      more.id = "search-more";
      more.textContent = t("search_more");
      this.list.appendChild(more);
    }
    this.list.hidden = false;
  }

  /** Premier scan pour la requête courante (debouncé par l'input). */
  private async run() {
    const query = this.input.value.trim();
    this.matches = [];
    this.idx = -1;
    this.nextBefore = 0;
    this.done = true;
    this.list.hidden = true;
    this.list.innerHTML = "";
    if (!app.current || !query) {
      this.count.textContent = "";
      return;
    }
    const my = ++this.reqSeq;
    const sid = app.current.id;
    this.count.textContent = "…";
    try {
      const page = await call<SearchPage>("search_events", {
        session: sid,
        query,
        limit: PAGE_LIMIT,
      });
      if (my !== this.reqSeq || app.current?.id !== sid) return;
      this.matches = page.matches;
      this.done = page.done;
      this.nextBefore = page.next_before;
      if (this.matches.length) {
        this.idx = 0;
        this.showCount();
        this.renderList();
        void this.goto(0);
      } else {
        this.showCount();
        this.renderList();
      }
    } catch (e) {
      if (my === this.reqSeq) {
        this.count.textContent = "";
        showErr(`search_events: ${e}`);
      }
    }
  }

  /** Page suivante du scan (curseur next_before) — bouton "more". */
  private async more() {
    if (this.loading || this.done || !app.current) return;
    const my = this.reqSeq;
    const sid = app.current.id;
    this.loading = true;
    try {
      const page = await call<SearchPage>("search_events", {
        session: sid,
        query: this.input.value.trim(),
        before_seq: this.nextBefore,
        limit: PAGE_LIMIT,
      });
      if (my !== this.reqSeq || app.current?.id !== sid) return;
      this.matches.push(...page.matches);
      this.done = page.done;
      this.nextBefore = page.next_before;
      this.showCount();
      this.renderList();
    } finally {
      this.loading = false;
    }
  }

  private markRow() {
    for (const r of this.list.querySelectorAll(".sr-row")) {
      r.classList.toggle("cur", Number((r as HTMLElement).dataset.idx) === this.idx);
    }
    this.list.querySelector(".sr-row.cur")?.scrollIntoView({ block: "nearest" });
  }

  private async goto(i: number) {
    const m = this.matches[i];
    if (!m) return;
    this.idx = i;
    this.showCount();
    this.markRow();
    await this.deps.jumpToSeq(m.seq);
  }

  /** ↑/↓ : navigation cyclique dans les résultats chargés ; en fin de
   *  liste avec scan incomplet, ↓ charge la page suivante d'abord. */
  private async step(d: number) {
    if (!this.matches.length) return;
    let i = this.idx + d;
    if (i >= this.matches.length) {
      if (!this.done) {
        await this.more();
        i = this.idx + d; // matches peut avoir grandi
      }
      if (i >= this.matches.length) i = 0; // boucle
    }
    if (i < 0) i = this.matches.length - 1;
    await this.goto(i);
  }
}
