// ---------------------------------------------------------------------------
// Viewer PDF du panneau fichiers : pdfjs-dist rend chaque page sur <canvas>
// dans #file-view — scroll continu, zoom +/-, pages rendues à la demande
// (IntersectionObserver) et recyclées hors écran. pdf.js est importé en
// différé : son chunk (lourd) ne charge qu'à la première ouverture d'un .pdf.
//
// Sécurité : le PDF vient du démon distant = données non fiables → rendu
// canvas uniquement, aucune injection HTML ; le layer d'annotations/liens
// n'est pas rendu (aucune navigation externe possible). pdf.js ≥5 n'a plus
// d'option isEvalSupported : le chemin eval a été supprimé upstream — le JS
// embarqué d'un PDF n'est jamais exécuté ici (la sandbox quickjs n'est ni
// chargée ni vendue, wasm/quickjs-eval.* exclus du vendoring).
//
// Assets pdf.js vendues dans public/pdfjs/ (cmaps, standard_fonts, wasm
// jbig2/openjpeg/qcms + fallbacks nowasm, iccs — à recopier depuis
// node_modules/pdfjs-dist à chaque montée de version de la dépendance).
// ---------------------------------------------------------------------------

import { invoke, isWeb } from "../core/bridge";
import { wsClient } from "../core/wsclient";
import { appDataDir, join } from "@tauri-apps/api/path";
import { $, esc, showErr } from "../core/dom";
import { fmtSize } from "../core/format";
import { b64decode } from "../core/encoding";
import { call } from "../core/rpc";
import { app } from "../core/state";
import { t } from "../i18n";
import type { PDFDocumentProxy, PDFPageProxy, RenderTask } from "pdfjs-dist";

/** Cap d'aperçu — aligné sur le CAP binaire du démon (read_file : `data`
 *  omis au-delà). Les plus gros tombent sur « ouvrir en externe ». */
const PDF_MAX = 12 * 1024 * 1024;

const ZOOM_MIN = 0.3;
const ZOOM_MAX = 4;
const ZOOM_STEP = 1.25;

/** Pages rendues conservées hors écran — au-delà, les plus anciennes sont
 *  libérées (un canvas rendu à fort zoom coûte plusieurs Mio). */
const RENDERED_MAX = 14;

/** Surface max d'un canvas rendu (~16 Mpx, convention viewer pdf.js). */
const MAX_CANVAS_PX = 16_777_216;

type PdfjsModule = typeof import("pdfjs-dist");

let pdfjs: PdfjsModule | null = null;

/** Charge pdf.js à la demande et pointe le worker sur l'asset bundlé
 *  (même origine → CSP script-src 'self' suffit ; si le Worker module est
 *  indisponible, pdf.js retombe seul sur le « fake worker » main-thread). */
async function pdfLib(): Promise<PdfjsModule> {
  if (!pdfjs) {
    const m = await import("pdfjs-dist");
    const w = await import("pdfjs-dist/build/pdf.worker.min.mjs?url");
    m.GlobalWorkerOptions.workerSrc = w.default;
    pdfjs = m;
  }
  return pdfjs;
}

/** Assets auxiliaires résolus depuis la racine de l'app (public/pdfjs/) —
 *  servis en statique côté démon web comme côté Tauri. */
function assetUrls() {
  const base = new URL("pdfjs/", document.baseURI).href;
  return {
    cMapUrl: `${base}cmaps/`,
    cMapPacked: true,
    standardFontDataUrl: `${base}standard_fonts/`,
    wasmUrl: `${base}wasm/`,
    iccUrl: `${base}iccs/`,
  };
}

interface PdfPageBox {
  n: number;
  box: HTMLDivElement;
  /** Dims du viewport à scale=1 — placeholder jusqu'au 1er getPage. */
  w: number;
  h: number;
  rendered: boolean;
  visible: boolean;
  /** Rendu en vol (getPage + RenderTask) — exclut les entrées concurrentes. */
  busy: boolean;
  task: RenderTask | null;
  /** Compteur de recencement pour l'éviction des pages hors écran. */
  stamp: number;
}

export class PdfView {
  private doc: PDFDocumentProxy | null = null;
  private pages: PdfPageBox[] = [];
  private pageObjs = new Map<number, PDFPageProxy>();
  private boxPages = new Map<HTMLElement, PdfPageBox>();
  private observer: IntersectionObserver | null = null;
  private zoom = 1;
  /** Invalidation des vols async (open/zoom/detach successifs). */
  private gen = 0;
  private stamp = 0;
  // Contexte du fichier ouvert (figé) — pour le fallback « externe ».
  private rel = "";
  private name = "";
  private sid = "";
  private conn: string | undefined;
  private size = 0;

  /** Ouvre un .pdf dans #file-view — cycle complet : lecture binaire,
   *  rendu lazy, statut, erreurs affichées en place. Ne jette pas. */
  async open(rel: string) {
    this.detach();
    if (!app.current) return;
    const gen = ++this.gen;
    this.rel = rel;
    // Nom assaini pour write_download côté Windows : basename sur les
    // deux séparateurs, composants vides/./.. rejetés (le « \ » est un
    // caractère ordinaire sous POSIX — un nom distant peut y injecter
    // une traversée hors de appDataDir/downloads).
    this.name =
      rel.split(/[\\/]/).filter((s) => s && s !== "." && s !== "..").pop()
      ?? "file.pdf";
    this.sid = app.current.id;
    this.conn = app.current.conn;
    const view = $("file-view");
    try {
      const r = await call<{
        data?: string; size?: number; truncated?: boolean; path: string;
      }>("read_file", {
        session: this.sid, conn: this.conn, path: rel, binary: true,
      });
      if (gen !== this.gen) return;
      $("file-path").title = r.path;
      this.size = r.size ?? 0;
      if (r.data == null || this.size > PDF_MAX) {
        this.tooBig();
        return;
      }
      const pdf = await pdfLib();
      const doc = await pdf
        .getDocument({ data: b64decode(r.data), ...assetUrls() })
        .promise;
      if (gen !== this.gen) {
        void doc.loadingTask.destroy().catch(() => {});
        return;
      }
      this.doc = doc;
      await this.buildPages(doc);
      if (gen !== this.gen) return;
      this.syncStatus();
    } catch (e) {
      if (gen !== this.gen) return;
      this.detach();
      view.innerHTML =
        `<div class="fv-msg">${esc(t("pdf_error"))} — ${esc(String(e))}</div>`;
    }
  }

  /** Fermeture/nettoyage : annule les rendus en vol, détruit le document
   *  (libère le worker) et masque les contrôles zoom. Idempotent. */
  detach() {
    this.gen++;
    this.observer?.disconnect();
    this.observer = null;
    for (const pg of this.pages) pg.task?.cancel();
    this.pages = [];
    this.pageObjs.clear();
    this.boxPages.clear();
    const doc = this.doc;
    this.doc = null;
    if (doc) void doc.loadingTask.destroy().catch(() => {});
    $("fv-zin").hidden = true;
    $("fv-zout").hidden = true;
    $("fv-enc").hidden = false;
    $("file-view").querySelector(".fv-pdf")?.remove();
  }

  /** > cap d'aperçu : message + bouton « ouvrir avec l'appli système ». */
  private tooBig() {
    const view = $("file-view");
    view.innerHTML = "";
    const msg = document.createElement("div");
    msg.className = "fv-msg";
    msg.textContent = t("pdf_too_big", { size: fmtSize(this.size) }) + " ";
    const btn = document.createElement("button");
    btn.className = "fv-pdfext";
    btn.textContent = t("pdf_open_ext");
    btn.onclick = () => void this.openExternal(btn);
    msg.appendChild(btn);
    view.appendChild(msg);
    $("fv-status").textContent = fmtSize(this.size);
    $("fv-enc").hidden = true;
  }

  /** Construit les conteneurs de page (placeholders aux dims de la page 1,
   *  corrigés au rendu pour les formats mixtes) et l'observateur lazy. */
  private async buildPages(doc: PDFDocumentProxy) {
    const gen = this.gen;
    const p1 = await doc.getPage(1);
    if (gen !== this.gen) return;
    const v1 = p1.getViewport({ scale: 1 });
    this.pageObjs.set(1, p1);
    const dw = v1.width || 612;
    const dh = v1.height || 792;
    // Zoom initial : page ajustée à la largeur disponible du viewer.
    const avail = Math.max($("file-view").clientWidth - 28, 120);
    this.zoom = Math.min(ZOOM_MAX, Math.max(ZOOM_MIN, avail / dw));
    const host = document.createElement("div");
    host.className = "fv-pdf";
    this.pages = [];
    for (let n = 1; n <= doc.numPages; n++) {
      const box = document.createElement("div");
      box.className = "fv-pdfpage";
      const pg: PdfPageBox = {
        n, box, w: dw, h: dh,
        rendered: false, visible: false, busy: false, task: null, stamp: 0,
      };
      this.sizeBox(pg);
      host.appendChild(box);
      this.pages.push(pg);
      this.boxPages.set(box, pg);
    }
    const view = $("file-view");
    view.innerHTML = "";
    view.appendChild(host);
    $("fv-zin").hidden = false;
    $("fv-zout").hidden = false;
    $("fv-enc").hidden = true;
    this.observer = new IntersectionObserver(
      (ents) => this.onIntersect(ents),
      { root: view, rootMargin: "80% 0px" },
    );
    for (const pg of this.pages) this.observer.observe(pg.box);
  }

  /** Placeholder/canvas redimensionné au zoom courant. */
  private sizeBox(pg: PdfPageBox) {
    pg.box.style.width = `${Math.ceil(pg.w * this.zoom)}px`;
    pg.box.style.height = `${Math.ceil(pg.h * this.zoom)}px`;
  }

  private onIntersect(ents: IntersectionObserverEntry[]) {
    for (const e of ents) {
      const pg = this.boxPages.get(e.target as HTMLElement);
      if (!pg) continue;
      pg.visible = e.isIntersecting;
      if (pg.visible) void this.renderPage(pg);
    }
  }

  /** Rend une page sur un canvas neuf : getPage (mémoïsé) → viewport
   *  zoom → canvas dpr-aware → RenderTask (annulable au zoom/détach). Un
   *  canvas frais à chaque rendu : la tâche annulée peut encore peindre
   *  brièvement — dans le canvas détaché, jamais dans le nouveau. */
  private async renderPage(pg: PdfPageBox) {
    const doc = this.doc;
    if (!doc || pg.rendered || pg.busy) return;
    pg.busy = true;
    const gen = this.gen;
    const zoom = this.zoom;
    let rt: RenderTask | null = null;
    try {
      let page = this.pageObjs.get(pg.n);
      if (!page) {
        page = await doc.getPage(pg.n);
        if (gen !== this.gen) {
          pg.busy = false;
          return;
        }
        this.pageObjs.set(pg.n, page);
        // Cache borné (ordre d'insertion = plus ancien en tête) — un
        // proxy oublié se re-fetche simplement au prochain scroll.
        const first = this.pageObjs.keys().next().value;
        if (this.pageObjs.size > 32 && first != null) this.pageObjs.delete(first);
        const v = page.getViewport({ scale: 1 });
        pg.w = v.width || pg.w;
        pg.h = v.height || pg.h;
      }
      const vp = page.getViewport({ scale: zoom });
      // Garde-fou mémoire : ~16 Mpx max par canvas (comme le viewer
      // pdf.js) — zoom 4 × dpr 2 sinon ≈ 31 Mpx ≈ 124 Mio par page,
      // trop pour une webview mobile. Le dpr effectif réduit au-delà.
      const dpr = Math.min(
        window.devicePixelRatio || 1, 2,
        Math.sqrt(MAX_CANVAS_PX / Math.max(vp.width * vp.height, 1)),
      );
      const canvas = document.createElement("canvas");
      canvas.width = Math.floor(vp.width * dpr);
      canvas.height = Math.floor(vp.height * dpr);
      canvas.style.width = `${Math.floor(vp.width)}px`;
      canvas.style.height = `${Math.floor(vp.height)}px`;
      pg.box.replaceChildren(canvas);
      pg.box.style.width = canvas.style.width;
      pg.box.style.height = canvas.style.height;
      rt = page.render({
        canvas,
        viewport: vp,
        transform: dpr !== 1 ? [dpr, 0, 0, dpr, 0, 0] : undefined,
      });
      pg.task = rt;
      await rt.promise;
    } catch (e) {
      pg.busy = false;
      if (rt && pg.task === rt) pg.task = null;
      // RenderingCancelledException = zoom/détach — pas une erreur :
      // la page visible se re-rend au zoom courant, l'échec réel
      // s'affiche dans la boîte page.
      if (gen === this.gen && pg.visible) {
        if (/cancel/i.test(String(e)) && zoom !== this.zoom) {
          void this.renderPage(pg);
        } else if (!/cancel/i.test(String(e))) {
          pg.box.innerHTML =
            `<div class="fv-pdferr">${esc(t("pdf_error"))}</div>`;
        }
      }
      return;
    }
    pg.busy = false;
    if (pg.task === rt) pg.task = null;
    if (gen !== this.gen || zoom !== this.zoom) {
      // Rendu tombé à un zoom périmé (la page était busy au moment du
      // zoomBy) — visible → re-rendre tout de suite au zoom courant.
      if (gen === this.gen && pg.visible && !pg.rendered) {
        void this.renderPage(pg);
      }
      return;
    }
    pg.rendered = true;
    pg.stamp = ++this.stamp;
    this.evict();
  }

  /** Zoom +/- : annule les rendus en vol, redimensionne tous les
   *  placeholders et re-rend seulement les pages visibles. */
  private zoomBy(f: number) {
    if (!this.doc) return;
    const z = Math.min(ZOOM_MAX, Math.max(ZOOM_MIN, this.zoom * f));
    if (z === this.zoom) return;
    this.zoom = z;
    for (const pg of this.pages) {
      pg.task?.cancel();
      pg.task = null;
      pg.rendered = false;
      // Canvas à l'ancien zoom retiré — le placeholder reprend la taille
      // cible en attendant le re-rendu (pages visibles tout de suite).
      pg.box.replaceChildren();
      this.sizeBox(pg);
      if (pg.visible) void this.renderPage(pg);
    }
    this.syncStatus();
  }

  /** Trop de canvases rendus hors écran → libère les plus anciens
   *  (rerendus à la demande en rescrollant). */
  private evict() {
    const off = this.pages.filter((p) => p.rendered && !p.visible);
    if (off.length <= RENDERED_MAX) return;
    off.sort((a, b) => a.stamp - b.stamp);
    for (const pg of off.slice(0, off.length - RENDERED_MAX)) {
      pg.rendered = false;
      pg.box.querySelector("canvas")?.remove();
      this.sizeBox(pg);
    }
  }

  private syncStatus() {
    const n = this.doc?.numPages ?? 0;
    $("fv-status").textContent =
      `${n === 1 ? t("pdf_page1") : t("pdf_pages", { n })} · ` +
      `${Math.round(this.zoom * 100)}%`;
  }

  /** « Ouvrir avec l'appli système » : web → download navigateur ;
   *  Tauri → download_path par tranches → write_download sous appDataDir
   *  → opener open_path (PDF hors cap d'aperçu). */
  private async openExternal(btn: HTMLButtonElement) {
    btn.disabled = true;
    const { rel, sid, conn, name } = this;
    try {
      if (isWeb) {
        const q = new URLSearchParams({
          session: sid, path: rel, token: wsClient.token() ?? "",
        });
        const a = document.createElement("a");
        a.href = `/download?${q}`;
        a.download = name;
        a.click();
        btn.textContent = t("dl_done_web", { name });
        return;
      }
      const dest = await join(await appDataDir(), "downloads", name);
      let offset = 0;
      let archive: string | undefined;
      for (;;) {
        const c = await call<{ total: number; data: string; archive?: string }>(
          "download_path",
          { session: sid, conn, path: rel, offset, limit: 1 << 20, archive },
        );
        archive ??= c.archive;
        offset += await invoke<number>("write_download", {
          path: dest, data: c.data, append: offset > 0,
        });
        btn.textContent = t("dl_progress", {
          name, pct: Math.round((offset * 100) / Math.max(c.total, 1)),
        });
        if (offset >= c.total) break;
      }
      // L'utilisateur a changé de fichier entre-temps : le téléchargé
      // reste sur disque mais on n'ouvre rien par surprise.
      if (!btn.isConnected) return;
      await invoke("plugin:opener|open_path", { path: dest });
      btn.textContent = `✓ ${name}`;
    } catch (e) {
      btn.disabled = false;
      btn.textContent = t("pdf_open_ext");
      showErr(String(e));
    }
  }

  init() {
    $("fv-zin").onclick = () => this.zoomBy(ZOOM_STEP);
    $("fv-zout").onclick = () => this.zoomBy(1 / ZOOM_STEP);
  }
}
