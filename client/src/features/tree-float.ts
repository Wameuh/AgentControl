// ---------------------------------------------------------------------------
// Arborescence détachable : la colonne #ft-side (recherche + arbre +
// résultats) peut quitter #file-body pour vivre dans une fenêtre
// flottante fixe — draggable par sa barre, redimensionnable par le coin,
// géométrie et mode « détaché » persistés en localStorage.
// Le DOM de l'arbre n'est jamais dupliqué : les handlers, watches FS et
// sélection restent ceux de FileExplorer. Android : no-op complet (le
// layout mobile n'a pas de place pour un panneau flottant — le bouton
// pop-out est masqué en CSS et attach/dock sont gardés).
// ---------------------------------------------------------------------------

import { isAndroid } from "../core/chrome";
import { $ } from "../core/dom";

/** État persisté : `d` = mode détaché, x/y/w/h = géométrie du flottant. */
interface FloatState {
  d?: boolean;
  x?: number;
  y?: number;
  w?: number;
  h?: number;
}

const LS_KEY = "ac-ft-float";
const MIN_W = 240;
const MIN_H = 170;
/** Marge minimale entre la fenêtre et les bords du viewport. */
const EDGE = 8;

export class TreeFloat {
  private root = $("ft-float");
  private bar = $("ft-float-bar");
  private side = $("ft-side");
  private body = $("file-body");
  private split = $("ft-split");
  private detached = false;
  private geo = { x: 0, y: 0, w: 0, h: 0 };
  /** Largeur inline de #ft-side en mode docké — mise de côté au
   *  détachement pour ne pas contraindre la largeur du flottant. */
  private dockedWidth = "";

  constructor(private cbs: { dock(): void; hide(): void }) {}

  isDetached() {
    return this.detached;
  }

  /** Branchement boutons + drag/resize + restauration du mode persisté.
   *  Appelé une fois depuis FileExplorer.init (DOM statique prêt). */
  init() {
    if (isAndroid()) return;
    $("ft-float-dock").onclick = () => this.cbs.dock();
    $("ft-float-close").onclick = () => this.cbs.hide();
    this.bar.addEventListener("pointerdown", (e) => this.startDrag(e));
    const rz = $("ft-float-rz");
    rz.addEventListener("pointerdown", (e) => this.startResize(e));
    // Double-clic sur la poignée = géométrie par défaut.
    rz.addEventListener("dblclick", () => {
      this.geo = this.defaultGeo();
      this.applyGeo();
      this.save();
    });
    // Redimensionnement fenêtre : la fenêtre reste dans le viewport.
    window.addEventListener("resize", () => {
      if (this.detached) this.applyGeo();
    });
    const s = this.load();
    if (s?.d) {
      this.detached = true;
      if (s.w && s.h) {
        this.geo = { x: s.x ?? 0, y: s.y ?? 0, w: s.w, h: s.h };
      } else {
        this.geo = this.defaultGeo();
      }
      this.moveToFloat();
      this.applyGeo();
    }
  }

  /** Détache la colonne dans le flottant (no-op si déjà détaché). */
  attach() {
    if (this.detached || isAndroid()) return;
    this.detached = true;
    if (!this.geo.w) this.geo = this.defaultGeo();
    this.moveToFloat();
    this.applyGeo();
    this.save();
  }

  /** Refold la colonne dans #file-body (avant #ft-split, ordre d'origine). */
  dock() {
    if (!this.detached) return;
    this.detached = false;
    this.moveToDock();
    this.root.hidden = true;
    this.save();
  }

  // -- DOM -----------------------------------------------------------------

  private moveToFloat() {
    this.dockedWidth = this.side.style.width;
    this.side.style.width = "";
    this.root.appendChild(this.side);
  }

  private moveToDock() {
    this.side.style.width = this.dockedWidth;
    this.body.insertBefore(this.side, this.split);
  }

  // -- Géométrie ------------------------------------------------------------

  /** Position par défaut : à gauche du panneau fichiers s'il est affiché,
   *  sinon accolé au bord droit ; sous la barre supérieure. */
  private defaultGeo() {
    const panel = $("file-panel");
    const pw = panel.hidden ? 0 : panel.getBoundingClientRect().width + 8;
    const w = Math.min(320, Math.max(MIN_W, Math.round(window.innerWidth * 0.24)));
    const h = Math.round(window.innerHeight * 0.62);
    return {
      w,
      h,
      x: Math.max(EDGE, window.innerWidth - pw - w - EDGE),
      y: Math.max(EDGE, 84),
    };
  }

  /** Applique la géométrie bornée au viewport (aussi au restore). */
  private applyGeo() {
    const w = Math.min(Math.max(this.geo.w, MIN_W), window.innerWidth - EDGE * 2);
    const h = Math.min(Math.max(this.geo.h, MIN_H), window.innerHeight - EDGE * 2);
    const x = Math.min(Math.max(this.geo.x, EDGE), window.innerWidth - w - EDGE);
    const y = Math.min(Math.max(this.geo.y, EDGE), window.innerHeight - h - EDGE);
    this.geo = { x, y, w, h };
    this.root.style.left = `${x}px`;
    this.root.style.top = `${y}px`;
    this.root.style.width = `${w}px`;
    this.root.style.height = `${h}px`;
  }

  private startDrag(e: PointerEvent) {
    if (e.button !== 0 || (e.target as HTMLElement).closest("button")) return;
    e.preventDefault();
    const sx = e.clientX - this.geo.x;
    const sy = e.clientY - this.geo.y;
    const move = (ev: PointerEvent) => {
      // Bouton/Doigt relâché hors fenêtre → up perdu : auto-relâcher.
      if (ev.buttons === 0) {
        up();
        return;
      }
      this.geo.x = ev.clientX - sx;
      this.geo.y = ev.clientY - sy;
      this.applyGeo();
    };
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      window.removeEventListener("pointercancel", up);
      this.save();
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
    window.addEventListener("pointercancel", up);
  }

  private startResize(e: PointerEvent) {
    if (e.button !== 0) return;
    e.preventDefault();
    e.stopPropagation();
    const sx = e.clientX;
    const sy = e.clientY;
    const { w: w0, h: h0 } = this.geo;
    const move = (ev: PointerEvent) => {
      if (ev.buttons === 0) {
        up();
        return;
      }
      this.geo.w = w0 + ev.clientX - sx;
      this.geo.h = h0 + ev.clientY - sy;
      this.applyGeo();
    };
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      window.removeEventListener("pointercancel", up);
      this.save();
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
    window.addEventListener("pointercancel", up);
  }

  // -- Persistance ----------------------------------------------------------

  private load(): FloatState | null {
    try {
      return JSON.parse(localStorage.getItem(LS_KEY) ?? "null") as FloatState | null;
    } catch {
      return null;
    }
  }

  private save() {
    localStorage.setItem(LS_KEY, JSON.stringify({ d: this.detached, ...this.geo }));
  }
}
