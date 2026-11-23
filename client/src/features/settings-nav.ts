// ---------------------------------------------------------------------------
// Navigation du panneau Paramètres (desktop) : rail de catégories groupé
// par portée (« Cette application » / « Serveur ») construit depuis les
// sections du markup, une section affichée à la fois, recherche sur
// libellés + descriptions, et contrôles visuels (vignettes de thème,
// pastilles d'accent, segmentés) adossés aux <select> d'origine — le
// staging (#190) et les tests continuent de lire/écrire ces selects.
// Android garde sa liste + sous-écrans (mobile/settings-mobile.ts) :
// seuls les contrôles visuels y sont branchés.
// ---------------------------------------------------------------------------

import { $, esc } from "../core/dom";
import { isAndroid } from "../core/chrome";
import { t } from "../i18n";
import { icon, type IconName } from "../ui/icons";

/** Lien du groupe « Serveur » qui ouvre une autre vue (Composants…). */
export interface ServerLink {
  id: string;
  label: string;
  icon: IconName;
  fn: () => void;
}

export interface SettingsNavDeps {
  /** Connexion liée : libellé d'hôte + état, null = aucune. */
  server: () => { label: string; up: boolean } | null;
  /** Vues serveur ouvrables depuis le rail (selon les capabilities). */
  serverLinks: () => ServerLink[];
  /** Ids des contrôles modifiés non enregistrés (pastille du rail). */
  dirtyIds: () => Set<string>;
  /** Quitter les Paramètres vers une autre vue (garde « non enregistré »). */
  leave: () => Promise<boolean>;
}

const DEFAULT_SEC = "s-sec-general";

/** Minuscules sans diacritiques — « général » trouve « general ». */
const fold = (s: string) =>
  s.normalize("NFD").replace(/[̀-ͯ]/g, "").toLowerCase();

export class SettingsNav {
  private active = DEFAULT_SEC;
  private desktop = !isAndroid();

  constructor(private deps: SettingsNavDeps) {}

  private sections(): HTMLDetailsElement[] {
    return [
      ...document.querySelectorAll<HTMLDetailsElement>(
        "#settings-body > details.s-sec",
      ),
    ];
  }

  init() {
    this.bindVisualControls();
    if (!this.desktop) return;
    // Desktop : les sections restent <details> (Android en dépend) mais
    // toujours ouvertes — leur titre sert d'en-tête de la catégorie.
    for (const d of this.sections()) {
      d.open = true;
      d.addEventListener("toggle", () => {
        if (!d.open) d.open = true;
      });
    }
    const input = $("s-search") as HTMLInputElement;
    input.addEventListener("input", () => this.applySearch());
    input.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && input.value) {
        e.stopPropagation();
        input.value = "";
        this.applySearch();
      }
    });
  }

  /** Reconstruit le rail (ouverture, langue, capabilities). */
  render() {
    if (!this.desktop) return;
    const host = $("s-rail-items");
    host.innerHTML = "";
    const visible = this.sections().filter((d) => !d.hidden);
    if (!visible.some((d) => d.id === this.active))
      this.active = visible[0]?.id ?? DEFAULT_SEC;
    const dirty = this.dirtySections();
    const grp = (html: string) => {
      const g = document.createElement("div");
      g.className = "s-rail-grp";
      g.innerHTML = html;
      host.appendChild(g);
    };
    const item = (
      id: string, label: string, ic: IconName, fn: () => void, ext = false,
    ) => {
      const b = document.createElement("button");
      b.type = "button";
      b.className = "s-rail-item";
      b.dataset.sec = id;
      b.innerHTML = `${icon(ic)}<span>${esc(label)}</span>`;
      if (ext) b.insertAdjacentHTML("beforeend", `<span class="s-rail-ext">${icon("arrow-right")}</span>`);
      else if (dirty.has(id)) b.insertAdjacentHTML("beforeend", `<span class="s-rail-dirty" title="${esc(t("settings_dirty"))}"></span>`);
      if (!ext && id === this.active && !this.searching()) b.setAttribute("aria-current", "true");
      b.onclick = fn;
      host.appendChild(b);
    };
    const title = (d: HTMLDetailsElement) =>
      d.querySelector(".s-sec-title")?.textContent?.trim() ?? d.id;
    const ic = (d: HTMLDetailsElement) => (d.dataset.railIcon || "settings") as IconName;

    grp(`${icon("monitor")}<span>${esc(t("s_scope_app"))}</span>`);
    for (const d of visible.filter((x) => x.dataset.scope !== "server"))
      item(d.id, title(d), ic(d), () => this.select(d.id));

    const srvSecs = visible.filter((x) => x.dataset.scope === "server");
    const links = this.deps.serverLinks();
    if (srvSecs.length || links.length) {
      const srv = this.deps.server();
      grp(
        `<span class="s-conn-dot${srv?.up ? " up" : ""}"></span>` +
          `<span>${esc(t("s_scope_server"))}</span>` +
          (srv ? `<span class="s-rail-host">${esc(srv.label)}</span>` : ""),
      );
      for (const d of srvSecs) item(d.id, title(d), ic(d), () => this.select(d.id));
      for (const l of links)
        item(l.id, l.label, l.icon, () => {
          void this.deps.leave().then((ok) => {
            if (ok) l.fn();
          });
        }, true);
    }
    this.show();
  }

  /** Sections contenant au moins un contrôle modifié. */
  private dirtySections(): Set<string> {
    const out = new Set<string>();
    for (const id of this.deps.dirtyIds()) {
      const sec = document.getElementById(id)?.closest("details.s-sec");
      if (sec?.id) out.add(sec.id);
    }
    return out;
  }

  /** Affiche une catégorie (desktop) ou ouvre la section (Android). */
  select(id: string) {
    const d = document.getElementById(id) as HTMLDetailsElement | null;
    if (!d) return;
    if (!this.desktop) {
      d.open = true;
      d.scrollIntoView({ block: "start" });
      return;
    }
    this.active = id;
    const input = $("s-search") as HTMLInputElement;
    if (input.value) input.value = "";
    this.applySearch(false);
    this.render();
    $("settings-body").scrollTop = 0;
  }

  private searching() {
    return this.desktop && !!($("s-search") as HTMLInputElement).value.trim();
  }

  private show() {
    for (const d of this.sections())
      d.classList.toggle("s-active", d.id === this.active);
  }

  /** Filtre les lignes de toutes les sections visibles ; un titre de
   *  section qui correspond montre la section entière. */
  private applySearch(rerender = true) {
    const view = $("settings-view");
    const q = fold(($("s-search") as HTMLInputElement).value.trim());
    view.classList.toggle("s-searching", !!q);
    let any = false;
    for (const d of this.sections()) {
      const rows = [...d.querySelectorAll<HTMLElement>(":scope > .s-sec-body > *")];
      if (!q || d.hidden) {
        d.classList.remove("s-hit");
        for (const r of rows) r.classList.remove("s-miss");
        continue;
      }
      const titleHit = fold(d.querySelector(".s-sec-title")?.textContent ?? "").includes(q);
      let hit = false;
      for (const r of rows) {
        const m = titleHit || (!r.hidden && fold(r.textContent ?? "").includes(q));
        r.classList.toggle("s-miss", !m);
        if (m && !r.hidden) hit = true;
      }
      d.classList.toggle("s-hit", hit);
      if (hit) any = true;
    }
    $("s-search-empty").hidden = !q || any;
    if (rerender) this.render();
  }

  /** Réinitialise la recherche (fermeture du panneau). */
  resetSearch() {
    if (!this.desktop) return;
    ($("s-search") as HTMLInputElement).value = "";
    this.applySearch(false);
  }

  // -------------------------------------------------------------------
  // Contrôles visuels ↔ selects
  // -------------------------------------------------------------------

  private groups(): HTMLElement[] {
    return [...document.querySelectorAll<HTMLElement>("#settings-view [data-for]")];
  }

  private bindVisualControls() {
    for (const g of this.groups()) {
      const sel = $(g.dataset.for!) as HTMLSelectElement;
      const pick = (v: string) => {
        if (sel.value === v) return;
        sel.value = v;
        // Déclenche le handler `onchange` posé par le staging (#190).
        sel.dispatchEvent(new Event("change"));
        this.syncVisual();
      };
      g.addEventListener("click", (e) => {
        const b = (e.target as HTMLElement).closest<HTMLButtonElement>("button[data-v]");
        if (b) pick(b.dataset.v!);
      });
      // Flèches : se déplacer dans le groupe comme un vrai radiogroup.
      g.addEventListener("keydown", (e) => {
        if (!["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown"].includes(e.key)) return;
        const btns = [...g.querySelectorAll<HTMLButtonElement>("button[data-v]")];
        const i = btns.findIndex((b) => b.dataset.v === sel.value);
        const d = e.key === "ArrowLeft" || e.key === "ArrowUp" ? -1 : 1;
        const n = btns[(i + d + btns.length) % btns.length];
        e.preventDefault();
        pick(n.dataset.v!);
        n.focus();
      });
    }
    this.syncVisual();
  }

  /** Reflète la valeur des selects (ouverture, annulation, langue). */
  syncVisual() {
    for (const g of this.groups()) {
      const v = ($(g.dataset.for!) as HTMLSelectElement).value;
      for (const b of g.querySelectorAll<HTMLButtonElement>("button[data-v]")) {
        const on = b.dataset.v === v;
        b.setAttribute("aria-checked", on ? "true" : "false");
        b.tabIndex = on ? 0 : -1;
      }
    }
  }
}

/** Nom lisible d'un contrôle stagé pour la barre d'enregistrement :
 *  libellé de sa ligne, ou « événement (canal) » dans la matrice. */
export function settingName(id: string): string {
  const el = document.getElementById(id);
  if (!el) return id;
  const txt = (n: Element | null | undefined) => n?.textContent?.trim() ?? "";
  const cell = el.closest("td, th");
  if (cell && el.closest(".s-mx")) {
    const table = el.closest("table")!;
    const tr = cell.parentElement as HTMLTableRowElement;
    const col = [...tr.children].indexOf(cell);
    const head = table.querySelectorAll("thead th")[col];
    const chan = txt(head?.querySelector(".mx-head span"));
    if (cell.tagName === "TH") {
      return el.closest(".mx-sub") ? txt(el.closest(".mx-sub")?.querySelector("span")) : chan;
    }
    return `${txt(tr.querySelector(".mx-ev > span"))} (${chan})`;
  }
  const row = el.closest(".s-row");
  return txt(row?.querySelector(".s-txt > span")) || el.getAttribute("aria-label") || id;
}
