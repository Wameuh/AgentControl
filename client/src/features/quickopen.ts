// ---------------------------------------------------------------------------
// Palette « quick open » ({mod}+P) : recherche de fichier par nom via
// find_files (debounce 250 ms, comme la recherche de l'explorateur),
// ouverture directe d'un chemin saisi (relatif, absolu, ~) et fichiers
// récents en tête quand la saisie est vide.
// Rendue en <dialog> modal — famille visuelle des dialogs existants :
// Échap / clic sur le backdrop ferment, ↑↓ + Entrée / clic choisissent,
// focus piégé et back Android gérés nativement par le dialog.
// ---------------------------------------------------------------------------

import { $, esc } from "../core/dom";
import { app } from "../core/state";
import { callBg } from "../core/rpc";
import { recentsId, recentFiles } from "../core/recent-files";
import { t } from "../i18n";
import { icon, type IconName } from "../ui/icons";

type Mode = "search" | "path";

interface QoItem {
  kind: "open" | "file" | "dir" | "recent";
  /** Texte principal de la ligne (nom, ou libellé « Ouvrir « … » »). */
  label: string;
  /** Chemin du parent, affiché atténué à droite. */
  hint: string;
  /** Chemin passé à openFile/openDir (relatif ou absolu). */
  path: string;
}

export interface QuickOpenDeps {
  openFile: (path: string) => Promise<void>;
  openDir: (rel: string) => Promise<void> | void;
}

/** Résultats affichés au plus (find_files limité pareil côté démon). */
const MAX_SHOWN = 50;

/** Saisie assimilable à un chemin : absolu, ~, ou relatif avec séparateur. */
const looksLikePath = (v: string) =>
  v.includes("/") || v.includes("\\") || v.startsWith("~");

const base = (p: string) => p.split("/").pop() ?? p;
const parent = (p: string) => (p.includes("/") ? p.slice(0, p.lastIndexOf("/")) : "");

const ICONS: Record<QoItem["kind"], IconName> = {
  open: "arrow-right",
  file: "file",
  dir: "folder",
  recent: "clock",
};

export class QuickOpen {
  private dlg = $("dlg-quick") as HTMLDialogElement;
  private input = $("qo-input") as HTMLInputElement;
  private list = $("qo-list");
  private mode: Mode = "search";
  private deps: QuickOpenDeps = {
    openFile: () => Promise.resolve(),
    openDir: () => {},
  };
  private items: QoItem[] = [];
  private sel = 0;
  private seq = 0;
  private timer = 0;
  /** Résultats de la dernière recherche (null = pas encore/pas de query). */
  private hits: QoItem[] | null = null;
  /** Ligne d'état sous la liste (aucun résultat, tronqué, erreur). */
  private note: { text: string; err: boolean } | null = null;
  /** Démon sans find_files : la recherche est coupée, le chemin reste. */
  private noFind = false;

  bind(d: Partial<QuickOpenDeps>) {
    Object.assign(this.deps, d);
  }

  get isOpen() {
    return this.dlg.open;
  }

  init() {
    $("btn-openpath").onclick = () => this.open("search");
    // Clic sur le backdrop : la cible est le <dialog> lui-même quand la
    // frappe tombe hors de sa boîte (le contenu ne la remplit pas).
    this.dlg.addEventListener("click", (e) => {
      if (e.target === this.dlg) this.close();
    });
    this.dlg.addEventListener("close", () => this.onClosed());
    this.input.addEventListener("input", () => {
      window.clearTimeout(this.timer);
      this.timer = window.setTimeout(() => void this.run(), 250);
    });
    this.input.addEventListener("keydown", (e) => {
      if (e.key === "ArrowDown" || e.key === "ArrowUp") {
        e.preventDefault();
        this.move(e.key === "ArrowDown" ? 1 : -1);
      } else if (e.key === "Enter") {
        e.preventDefault();
        this.choose(this.sel);
      }
    });
    this.list.addEventListener("click", (e) => {
      const row = (e.target as HTMLElement).closest<HTMLElement>("[data-qi]");
      if (row) this.choose(Number(row.dataset.qi));
    });
    // Survol = sélection (comme VS Code) — sans déplacer le scroll.
    this.list.addEventListener("mousemove", (e) => {
      const row = (e.target as HTMLElement).closest<HTMLElement>("[data-qi]");
      if (row) {
        this.sel = Number(row.dataset.qi);
        this.markSel();
      }
    });
    // Ctrl/Cmd+P : la palette prend le raccourci partout SAUF terminal
    // interactif (le PTY en a besoin : previous-history readline). Sous un
    // autre dialogue modal on mange la touche sans ouvrir — sinon le
    // navigateur ouvrirait son dialogue d'impression. Sans session
    // ouverte : non intercepté (print natif conserve son raccourci,
    // comme le find natif sous Ctrl+F). e.repeat : un combo maintenu
    // toggle-fermerait en boucle (flicker).
    window.addEventListener("keydown", (e) => {
      if (!(e.ctrlKey || e.metaKey) || (e.key !== "p" && e.key !== "P") || e.repeat)
        return;
      if (this.dlg.open) {
        e.preventDefault();
        this.close();
        return;
      }
      if (!app.current) return;
      if ((e.target as HTMLElement).closest?.("#term-panel")) return;
      e.preventDefault();
      if (document.querySelector("dialog[open]")) return;
      this.open("search");
    });
  }

  /** Ouvre la palette — `path` : saisie guidée chemin (menu explicite),
   *  `search` : recherche par nom (Ctrl+P, bouton de l'explorateur). */
  open(mode: Mode = "search") {
    if (!app.current) return;
    if (this.dlg.open) {
      this.close();
      return;
    }
    this.mode = mode;
    this.input.placeholder = t(mode === "path" ? "qo_path_ph" : "qo_ph");
    this.input.value = "";
    this.hits = null;
    this.note = null;
    // noFind re-testé à chaque ouverture : la session a pu changer vers
    // un démon plus récent qui connaît find_files.
    this.noFind = false;
    this.dlg.showModal();
    this.input.focus();
    this.render();
  }

  close() {
    if (this.dlg.open) this.dlg.close();
  }

  /** Fermeture (Échap natif, backdrop, choix, Ctrl+P toggle) : la
   *  recherche en vol est invalidée et l'état de liste remis à zéro. */
  private onClosed() {
    window.clearTimeout(this.timer);
    this.seq++;
    this.items = [];
    this.sel = 0;
    this.hits = null;
    this.note = null;
  }

  // -- Liste : items sélectionnables + lignes d'état -------------------------

  private render() {
    this.list.innerHTML = "";
    this.items = [];
    const raw = this.input.value.trim();
    if (!raw) {
      const rec = recentFiles(recentsId(app.current?.id ?? "", app.current?.conn));
      if (rec.length) {
        this.list.insertAdjacentHTML(
          "beforeend",
          `<div class="qo-sec">${esc(t("qo_recent"))}</div>`,
        );
        for (const p of rec) {
          this.items.push({ kind: "recent", label: base(p), hint: parent(p), path: p });
        }
      } else {
        this.note = { text: t("qo_start"), err: false };
      }
    } else {
      // Chemin saisi : « ouvrir <path> » en tête — toujours en mode path,
      // seulement si ça ressemble à un chemin en mode recherche.
      if (this.mode === "path" || looksLikePath(raw)) {
        this.items.push({ kind: "open", label: t("qo_open", { path: raw }), hint: "", path: raw });
      }
      if (this.hits) this.items.push(...this.hits.slice(0, MAX_SHOWN));
      if (!this.note && this.noFind && !looksLikePath(raw)) {
        this.note = { text: t("qo_nosearch"), err: false };
      }
    }
    // Sélection ramenée au premier item : chaque nouveau jeu de résultats
    // rend Entrée = « ouvrir le meilleur match » (comportement VS Code).
    this.sel = 0;
    this.items.forEach((it, i) => this.list.appendChild(this.row(it, i)));
    if (this.note?.text) {
      const d = document.createElement("div");
      d.className = this.note.err ? "ferr" : "qo-note";
      d.textContent = this.note.text;
      this.list.appendChild(d);
    }
    this.markSel();
  }

  private row(it: QoItem, i: number): HTMLElement {
    const d = document.createElement("div");
    d.className = `fhit qo-row qo-${it.kind}` + (it.kind === "dir" ? " is-dir" : "");
    d.id = `qo-opt-${i}`;
    d.dataset.qi = String(i);
    d.setAttribute("role", "option");
    d.innerHTML =
      `<span class="fico">${icon(ICONS[it.kind])}</span>` +
      `<span class="fname">${esc(it.label)}</span>` +
      (it.hint ? `<span class="fhdir">${esc(it.hint)}</span>` : "");
    return d;
  }

  private markSel() {
    for (const r of this.list.querySelectorAll<HTMLElement>("[data-qi]")) {
      const cur = Number(r.dataset.qi) === this.sel;
      r.classList.toggle("cur", cur);
      r.setAttribute("aria-selected", String(cur));
    }
    const cur = this.list.querySelector<HTMLElement>(".qo-row.cur");
    cur?.scrollIntoView({ block: "nearest" });
    if (cur) this.input.setAttribute("aria-activedescendant", cur.id);
    else this.input.removeAttribute("aria-activedescendant");
  }

  private move(d: number) {
    if (!this.items.length) return;
    this.sel = (this.sel + d + this.items.length) % this.items.length;
    this.markSel();
  }

  private choose(i: number) {
    const it = this.items[i];
    if (!it) return;
    this.close();
    if (it.kind === "dir") {
      void this.deps.openDir(it.path);
    } else if (it.kind === "open" && it.path.endsWith("/") && it.path.length > 1
               && !it.path.startsWith("/") && !it.path.startsWith("~")) {
      // « dossier/ » relatif saisi = intention de navigation → révélé
      // dans l'arbre plutôt qu'une erreur « Is a directory » du viewer.
      void this.deps.openDir(it.path.replace(/\/+$/, ""));
    } else {
      void this.deps.openFile(it.path);
    }
  }

  // -- Recherche find_files -------------------------------------------------

  private async run() {
    const raw = this.input.value.trim();
    const seq = ++this.seq;
    this.hits = null;
    this.note = null;
    if (!raw || !app.current) {
      this.render();
      return;
    }
    if (this.noFind) {
      this.render();
      return;
    }
    const sid = app.current.id;
    // find_files matche sur le NOM seul : une saisie « sous/dossier/fic »
    // interroge sur le dernier segment, puis les chemins retournés sont
    // filtrés côté client sur la saisie complète.
    const q = looksLikePath(raw) ? (raw.split(/[\\/]/).pop() ?? raw) : raw;
    if (!q) {
      this.render();
      return;
    }
    try {
      const r = await callBg<{
        matches: { path: string; dir: boolean; size: number }[];
        truncated: boolean;
      }>("find_files", { session: sid, query: q, limit: MAX_SHOWN });
      if (seq !== this.seq) return;
      // Session affichée changée pendant le vol : les hits indexent
      // l'ancien workspace et ouvriraient dans le mauvais cwd — fermer
      // plutôt que laisser une liste périmée visible.
      if (app.current?.id !== sid) {
        this.close();
        return;
      }
      const lq = raw.toLowerCase();
      this.hits = r.matches
        .filter((m) => !looksLikePath(raw) || m.path.toLowerCase().includes(lq))
        .map((m) => ({
          kind: m.dir ? ("dir" as const) : ("file" as const),
          label: base(m.path),
          hint: parent(m.path),
          path: m.path,
        }));
      if (!this.hits.length && !looksLikePath(raw)) {
        this.note = { text: t("file_search_none"), err: false };
      } else if (r.truncated) {
        this.note = { text: t("file_search_trunc"), err: false };
      }
    } catch (e) {
      if (seq !== this.seq) return;
      if (app.current?.id !== sid) {
        this.close();
        return;
      }
      // Vieux démon sans find_files (même détection que l'explorateur) :
      // la recherche tombe mais la saisie de chemin reste fonctionnelle.
      if (/unknown variant|méthode inconnue/i.test(String(e))) {
        this.noFind = true;
      } else {
        this.note = { text: String(e), err: true };
      }
    }
    this.render();
  }
}
