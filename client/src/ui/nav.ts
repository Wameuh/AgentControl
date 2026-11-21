// ---------------------------------------------------------------------------
// Navigation Android : pile d'overlays adossée à l'historique → le bouton
// "retour" ferme le panneau/menu/dialog ouvert au lieu de quitter l'app.
// Scrim (drawer/sheet) et bottom-sheet ⋮ sont créés ici aussi.
// ---------------------------------------------------------------------------

import { $, esc } from "../core/dom";
import { isAndroid } from "../core/chrome";
import { t } from "../i18n";
import { icon, type IconName } from "./icons";
import { hintOnce } from "../mobile/hints";

type ScrimOwner = "drawer" | "sheet" | "queue";

export interface SheetItem {
  label: string;
  icon?: IconName;
  danger?: boolean;
  fn: () => void;
}

export class OverlayNav {
  /** Pile par jetons : chaque entrée history porte un n unique → popstate
   *  compare les positions au lieu de dépiler à l'aveugle. Les entrées
   *  « mortes » (overlay refermé sans back) restent dans l'historique
   *  comme waypoints transparents : atterrir dessus ferme les overlays
   *  plus récents — aucun back() programmatique, aucune suppression à
   *  faire fuiter (Chrome fusionne les back() rapprochés). */
  private stack: { id: string; n: number }[] = [];
  private seq = 0;
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
      } else if (this.scrimOwner === "queue") {
        // Même motif que rawCloseOverlay : le ✕ porte le close complet.
        $("qp-close").click();
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
      const n = (history.state as { n?: number } | null)?.n;
      // Ferme tous les overlays plus récents que l'entrée atteinte (nul =
      // entrée racine hors nav → tout ferme). Idempotent : un waypoint
      // mort laisse la pile alignée quelles que soient les séquences.
      while (true) {
        const top = this.stack[this.stack.length - 1];
        if (!top || (n != null && top.n <= n)) break;
        if (this.backGuard && !this.backGuard(top.id)) {
          try { history.pushState({ ac: top.id, n: top.n }, ""); } catch { /* noop */ }
          return;
        }
        this.stack.pop();
        this.rawCloseOverlay(top.id);
      }
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
    const n = ++this.seq;
    this.stack.push({ id, n });
    try { history.pushState({ ac: id, n }, ""); } catch { /* noop */ }
  }

  /** Retire l'overlay de la pile — l'entrée history devient un waypoint
   *  mort (le popstate qui l'atteindra fermera ce qu'il y a au-dessus).
   *  Pas de back() : la fermeture visuelle est déjà faite par l'appelant. */
  close(id: string) {
    if (!isAndroid()) return;
    for (let i = this.stack.length - 1; i >= 0; i--) {
      if (this.stack[i].id === id) { this.stack.splice(i, 1); return; }
    }
  }

  private rawCloseOverlay(id: string) {
    if (id === "drawer") {
      $("sidebar").classList.remove("open");
      this.scrimOff("drawer");
    } else if (id === "sheet" || id === "fctx" || id === "sctx" || id === "evctx") {
      this.omenu.hidden = true;
      $("fctx").hidden = true;
      $("sctx").hidden = true;
      $("evctx").hidden = true;
      this.scrimOff("sheet");
    } else if (id === "fileview") {
      $("file-viewer").classList.remove("open");
      stopVideos($("file-viewer"));
    } else if (id === "files") {
      // Passe par le ✕ : closeFilePanel porte le close complet
      // (vidéo, vue, recherche — et les watches FS de l'explorateur).
      $("file-close").click();
    } else if (id === "term") {
      // Passe par le handler de ✕ : wantOpen retombe + nav.close.
      $("btn-term-hide").click();
    } else if (id === "queue") {
      // Idem : le ✕ du panneau file synchronise qOpen côté composer.
      $("qp-close")?.click();
    } else if (id === "settings") {
      // Passe par le handler de ✕ : hidden + levée de l'inert + nav.close.
      $("settings-close").click();
    } else if (id === "connview") {
      // Même motif : le ✕ de la vue connexion porte le close complet.
      $("connview-close").click();
    } else if (id === "map") {
      // Idem : le ✕ du diagramme rend l'inert + purge le poll timer.
      $("map-close").click();
    } else if (id === "components") {
      // Idem : le ✕ du panneau composants porte le close complet.
      $("comp-close").click();
    } else if (id === "logs") {
      // Idem : le ✕ du panneau logs porte le close complet (poll stoppé).
      $("logs-close").click();
    } else if (id === "plugin") {
      // Idem : le ✕ de la vue plugin démonte l'iframe (token éphémère).
      $("pv-close")?.click();
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

  /** Voile modal pour un overlay hors nav (file d'attente en bottom-sheet) —
   *  le tap dehors ferme via le ✕ de l'overlay (scrim.onclick). */
  showScrim(owner: ScrimOwner) { this.scrimOn(owner); }
  /** No-op si le voile appartient à un autre overlay. */
  hideScrim(owner: ScrimOwner) { this.scrimOff(owner); }

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
    // Indice de découverte (une seule fois) : les actions des lignes de
    // session — suppression comprise — vivent dans le menu d'appui long.
    if (isAndroid() && document.querySelector("#session-list .sess")) {
      hintOnce("longpress", t("hint_longpress"));
    }
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
        (!inDrawer && t0.clientX <= 26 && !(e.target as HTMLElement).closest("#file-panel, #omenu, #fctx, #sctx, #evctx"));
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
