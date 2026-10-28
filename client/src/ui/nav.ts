// ---------------------------------------------------------------------------
// Navigation Android : pile d'overlays adossée à l'historique → le bouton
// "retour" ferme le panneau/menu/dialog ouvert au lieu de quitter l'app.
// Scrim (drawer/sheet) et bottom-sheet ⋮ sont créés ici aussi.
// ---------------------------------------------------------------------------

import { $, esc } from "../core/dom";
import { isAndroid } from "../core/chrome";
import { icon, type IconName } from "./icons";

type ScrimOwner = "drawer" | "sheet";

export interface SheetItem {
  label: string;
  icon?: IconName;
  danger?: boolean;
  fn: () => void;
}

export class OverlayNav {
  private stack: string[] = [];
  private suppressPops = 0; // back() déclenchés par close → autant de popstate à ignorer
  private scrimOwner: ScrimOwner | null = null;
  private scrim: HTMLElement;
  private omenu: HTMLElement;
  /** Véto sur la fermeture par back système — `false` = l'overlay reste
   *  (l'état history est re-poussé). Posé par FileExplorer (buffer sale). */
  backGuard?: (id: string) => boolean;

  constructor() {
    // Drawer mobile : ☰ ouvre la sidebar, tap dehors/selection ferme.
    this.scrim = document.createElement("div");
    this.scrim.id = "scrim";
    this.scrim.hidden = true;
    this.scrim.onclick = () => {
      if (this.scrimOwner === "drawer") {
        this.close("drawer");
        $("sidebar").classList.remove("open");
        this.scrimOff("drawer");
      } else if (this.scrimOwner === "sheet") {
        this.close("sheet");
        this.omenu.hidden = true;
        this.scrimOff("sheet");
      }
    };
    // NB : dans #app-body (pas body) — #app a z-index:1 (contexte d'empilement),
    // un enfant de body passerait au-dessus de toute l'app quel que soit son z.
    $("app-body").appendChild(this.scrim);
    // Menu ⋮ en bottom-sheet (Android uniquement)
    this.omenu = document.createElement("div");
    this.omenu.id = "omenu";
    this.omenu.hidden = true;
    $("app-body").appendChild(this.omenu);

    window.addEventListener("popstate", () => {
      if (!isAndroid()) return;
      if (this.suppressPops > 0) { this.suppressPops--; return; }
      const top = this.stack[this.stack.length - 1];
      if (!top) return;
      // Véto : l'overlay refuse la fermeture (ex. éditeur avec buffer sale)
      // → on ré-empile l'état pour annuler le back sans toucher à la vue.
      if (this.backGuard && !this.backGuard(top)) {
        try { history.pushState({ ac: top }, ""); } catch { /* noop */ }
        return;
      }
      this.stack.pop();
      this.rawCloseOverlay(top);
    });
    // Dialogs trackés automatiquement dans la pile (Cancel = back propre).
    if (isAndroid()) {
      const origShowModal = HTMLDialogElement.prototype.showModal;
      const nav = this;
      HTMLDialogElement.prototype.showModal = function (this: HTMLDialogElement) {
        origShowModal.call(this);
        nav.push(`dlg:${this.id}`);
      };
      document.addEventListener("close", (e) => {
        const d = e.target as HTMLDialogElement;
        if (d?.id) nav.close(`dlg:${d.id}`);
      }, true);
    }
    this.bindSwipe();
  }

  push(id: string) {
    if (!isAndroid()) return;
    this.stack.push(id);
    try { history.pushState({ ac: id }, ""); } catch { /* noop */ }
  }

  close(id: string) {
    if (!isAndroid()) return;
    const i = this.stack.lastIndexOf(id);
    if (i < 0) return;
    this.stack.splice(i, 1);
    if ((history.state as { ac?: string } | null)?.ac === id) {
      this.suppressPops++;
      history.back();
    }
  }

  private rawCloseOverlay(id: string) {
    if (id === "drawer") {
      $("sidebar").classList.remove("open");
      this.scrimOff("drawer");
    } else if (id === "sheet" || id === "fctx" || id === "sctx") {
      this.omenu.hidden = true;
      $("fctx").hidden = true;
      $("sctx").hidden = true;
      this.scrimOff("sheet");
    } else if (id === "fileview") {
      $("file-viewer").classList.remove("open");
      stopVideos($("file-viewer"));
    } else if (id === "files") {
      $("file-panel").hidden = true;
      stopVideos($("file-panel"));
    } else if (id === "term") {
      // Passe par le handler de ✕ : wantOpen retombe + nav.close.
      $("btn-term-hide").click();
    } else if (id === "settings") {
      // Passe par le handler de ✕ : hidden + levée de l'inert + nav.close.
      $("settings-close").click();
    } else if (id.startsWith("dlg:")) {
      (document.getElementById(id.slice(4)) as HTMLDialogElement | null)?.close();
    }
  }

  private scrimOn(owner: ScrimOwner) {
    this.scrimOwner = owner;
    this.scrim.hidden = false;
  }

  private scrimOff(owner: ScrimOwner) {
    if (this.scrimOwner === owner) { this.scrimOwner = null; this.scrim.hidden = true; }
  }

  sheet(items: SheetItem[]) {
    this.omenu.innerHTML = "";
    for (const it of items) {
      const b = document.createElement("button");
      b.className = "om-item" + (it.danger ? " danger" : "");
      b.innerHTML = (it.icon ? `${icon(it.icon)} ` : "") + `<span>${esc(it.label)}</span>`;
      b.onclick = () => { this.closeSheet(); it.fn(); };
      this.omenu.appendChild(b);
    }
    this.omenu.hidden = false;
    this.scrimOn("sheet");
    this.push("sheet");
  }

  closeSheet() {
    this.close("sheet");
    this.omenu.hidden = true;
    this.scrimOff("sheet");
  }

  openDrawer() {
    $("sidebar").classList.add("open");
    this.scrimOn("drawer");
    this.push("drawer");
  }

  closeDrawer() {
    this.close("drawer");
    $("sidebar").classList.remove("open");
    this.scrimOff("drawer");
  }

  /** ☰ toggle + swipe bord gauche → drawer. */
  private bindSwipe() {
    $("tb-menu").onclick = () =>
      $("sidebar").classList.contains("open") ? this.closeDrawer() : this.openDrawer();
    // Swipe depuis le bord gauche → ouvre le drawer ; swipe gauche dessus → ferme.
    let sx = 0, sy = 0, swiping = false;
    document.addEventListener("touchstart", (e) => {
      const t0 = e.touches[0];
      const inDrawer = (e.target as HTMLElement).closest("#sidebar") != null;
      if (!isAndroid() || document.querySelector("dialog[open]")) { swiping = false; return; }
      swiping = (inDrawer && $("sidebar").classList.contains("open")) ||
        (!inDrawer && t0.clientX <= 26 && !(e.target as HTMLElement).closest("#file-panel, #omenu, #fctx, #sctx"));
      sx = t0.clientX; sy = t0.clientY;
    }, { passive: true });
    document.addEventListener("touchmove", (e) => {
      if (!swiping) return;
      const t0 = e.touches[0];
      const dx = t0.clientX - sx, dy = t0.clientY - sy;
      if (Math.abs(dy) > Math.abs(dx)) { swiping = false; return; }
      if (dx > 56) { this.openDrawer(); swiping = false; }
      else if (dx < -56 && $("sidebar").classList.contains("open")) { this.closeDrawer(); swiping = false; }
    }, { passive: true });
  }
}

/** Coupe les <video> présentes dans l'overlay fermé — sinon lecture
 *  et stream démon continuent en tâche de fond (pas d'import de la
 *  feature fichiers : nettoyage purement DOM, quel que soit l'overlay). */
function stopVideos(root: HTMLElement) {
  root.querySelectorAll("video").forEach((v) => {
    v.pause();
    v.removeAttribute("src");
    v.load();
  });
}

export const nav = new OverlayNav();
