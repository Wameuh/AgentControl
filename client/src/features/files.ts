// ---------------------------------------------------------------------------
// Explorateur de fichiers du workspace (list_dir/read_file/delete_path) :
// panneau latéral, arborescence paresseuse, viewer texte/image, menu
// contextuel et téléchargement par tranches.
// ---------------------------------------------------------------------------

import { invoke } from "@tauri-apps/api/core";
import { save } from "@tauri-apps/plugin-dialog";
import { appDataDir, downloadDir } from "@tauri-apps/api/path";
import { $, copyText, esc, showErr } from "../core/dom";
import { fmtSize, highlight, IMG_MIME, VID_MIME } from "../core/format";
import { call } from "../core/rpc";
import { app } from "../core/state";
import { isAndroid, isWindows } from "../core/chrome";
import type { DirEntry } from "../core/types";
import { t } from "../i18n";
import { icon, type IconName } from "../ui/icons";
import { bindCtx, hideMenu, menuBtn, placeMenu } from "../ui/ctxmenu";
import { nav } from "../ui/nav";

/** URL du schéma custom acvideo — forme imposée par plateforme :
 *  http://acvideo.localhost sous Windows/Android, acvideo:// ailleurs. */
function videoUrl(path: string): string {
  const q = new URLSearchParams({ session: app.current?.id ?? "", path });
  if (app.current?.conn) q.set("conn", app.current.conn);
  const base = isAndroid() || isWindows() ? "http://acvideo.localhost/" : "acvideo://localhost/";
  return `${base}?${q}`;
}

/** Stoppe le <video> du viewer — le stream démon s'arrête avec lui
 *  (sinon la lecture/bufferisation continue sur un élément détaché). */
function stopVideo() {
  const v = $("file-view").querySelector("video");
  if (v) { v.pause(); v.removeAttribute("src"); v.load(); }
}

export class FileExplorer {
  private ctx = $("fctx");
  private treeOpen = false;
  /** cwd absolu de la session (rempli par loadTreeRoot) — sert à
   *  reconstruire le chemin absolu des résultats de recherche. */
  private treeRootDir = "";
  private deps = {
    /** Ligne de progression de téléchargement dans le fil. */
    sysline: (_text: string, _ic?: IconName): HTMLElement => document.createElement("div"),
  };

  bind(d: Partial<typeof this.deps>) {
    Object.assign(this.deps, d);
  }

  /** L'arbre suit le workspace de la session nouvellement ouverte. */
  onSessionOpened() {
    if (this.treeOpen) {
      this.clearSearch();
      void this.loadTreeRoot();
    }
  }

  async openFilePanel(path: string) {
    if (!app.current) return;
    if ($("file-panel").hidden) { $("file-panel").hidden = false; nav.push("files"); }
    // Android : le viewer glisse par-dessus la liste (navigation plein écran).
    if (isAndroid()) {
      $("file-viewer").classList.add("open");
      $("fv-name").textContent = path.split("/").pop() ?? path;
      nav.push("fileview");
    }
    $("file-path").textContent = path;
    $("file-path").title = path;
    stopVideo();
    $("file-view").textContent = "…";
    // Marque le noeud de l'arborescence correspondant
    for (const n of $("file-tree").querySelectorAll<HTMLElement>(".fnode")) {
      n.classList.toggle("sel", n.dataset.rel === path);
    }
    const ext = /\.([a-zA-Z0-9]+)$/.exec(path)?.[1]?.toLowerCase() ?? "";
    if (VID_MIME[ext]) {
      // Lecteur streamé : le schéma acvideo sert des ranges HTTP
      // (tranches read_file démon) — la webview ne télécharge que ce
      // qu'elle bufferise (métadonnées + premières secondes, ranges de
      // seek ensuite), jamais le fichier entier.
      const view = $("file-view");
      view.innerHTML = "";
      const v = document.createElement("video");
      v.className = "fv-vid";
      v.controls = true;
      v.preload = "metadata";
      v.playsInline = true;
      v.onerror = () => {
        view.innerHTML = `<div class="fv-msg">${esc(t("video_error"))}</div>`;
      };
      v.src = videoUrl(path);
      view.appendChild(v);
      return;
    }
    try {
      if (IMG_MIME[ext]) {
        const r = await call<{
          data?: string; size?: number; truncated?: boolean;
          content?: string; path: string;
        }>("read_file", { session: app.current.id, path, binary: true });
        $("file-path").title = r.path;
        if (r.data != null) {
          // Propriétés DOM — r.data (démon distant) jamais injecté en markup.
          const img = document.createElement("img");
          img.className = "fv-img";
          img.src = `data:${IMG_MIME[ext]};base64,${r.data}`;
          img.alt = path;
          const view = $("file-view");
          view.innerHTML = "";
          view.appendChild(img);
        } else if (r.content != null) {
          // Vieux démon sans mode binaire → affichage texte historique.
          $("file-view").innerHTML =
            highlight(r.content, ext) + (r.truncated ? "\n… (tronqué)" : "");
        } else {
          $("file-view").innerHTML =
            `<div class="fv-msg">${esc(t("img_too_big", { size: fmtSize(r.size ?? 0) }))}</div>`;
        }
        return;
      }
      const r = await call<{ content: string; truncated: boolean; path: string }>("read_file", {
        session: app.current.id,
        path,
      });
      $("file-path").title = r.path;
      $("file-view").innerHTML =
        highlight(r.content, ext) + (r.truncated ? "\n… (tronqué)" : "");
    } catch (e) {
      $("file-view").textContent = String(e);
    }
  }

  private closeFilePanel() {
    // Coupe la lecture en cours — sinon le stream acvideo continue en
    // tâche de fond après fermeture du panneau.
    stopVideo();
    // Le viewer glissé par-dessus doit être dépilé aussi (pile back propre).
    nav.close("fileview");
    $("file-viewer").classList.remove("open");
    nav.close("files");
    $("file-panel").hidden = true;
    this.clearSearch();
  }

  /** Nœud de l'arbre : dossier (pliable) ou fichier (ouvre le viewer). */
  private treeNode(rel: string, name: string, en: DirEntry, depth: number, abs: string): HTMLElement {
    const d = document.createElement("div");
    d.className = "fnode" + (en.dir ? " is-dir" : "") + (name.startsWith(".") ? " fdot" : "");
    d.style.paddingLeft = `${6 + depth * 14}px`;
    d.dataset.rel = rel;
    d.dataset.abs = abs;
    d.innerHTML =
      `<span class="farrow">${en.dir ? "\u25b8" : ""}</span>` +
      `<span class="fico">${icon(en.dir ? "folder" : "file")}</span>` +
      `<span class="fname">${esc(name)}</span>` +
      (en.dir ? "" : `<span class="fsize">${fmtSize(en.size)}</span>`);
    d.title = abs;
    if (en.dir) {
      d.onclick = () => this.toggleDir(d);
    } else {
      d.onclick = () => this.openFilePanel(rel);
    }
    bindCtx(d, (x, y) => this.openFileCtx(x, y, d));
    return d;
  }

  /** Ouvre/replie un dossier — chargement paresseux à chaque expansion
   *  (le contenu change pendant que l'agent travaille, pas de cache). */
  private async toggleDir(el: HTMLElement) {
    const arrow = el.querySelector<HTMLElement>(".farrow")!;
    const next = el.nextElementSibling;
    if (next?.classList.contains("fchildren")) {
      next.remove();
      arrow.textContent = "\u25b8";
      return;
    }
    await this.expandDir(el);
  }

  /** Charge les enfants d'un dossier (lazy) et renvoie la boîte `.fchildren`
   *  — ou celle déjà ouverte. null si le nœud a été détaché entre-temps. */
  private async expandDir(el: HTMLElement): Promise<HTMLElement | null> {
    const next = el.nextElementSibling;
    if (next?.classList.contains("fchildren")) return next as HTMLElement;
    const arrow = el.querySelector<HTMLElement>(".farrow")!;
    const rel = el.dataset.rel ?? "";
    arrow.textContent = "…";
    const box = document.createElement("div");
    box.className = "fchildren";
    try {
      const r = await call<{ dir: string; entries: DirEntry[] }>("list_dir", {
        session: app.current!.id,
        path: rel,
      });
      if (!r.entries.length) {
        box.innerHTML = `<div class="fempty">${t("tree_empty")}</div>`;
      }
      const depth = rel ? rel.split("/").length : 0;
      for (const en of r.entries) {
        const childRel = rel ? `${rel}/${en.name}` : en.name;
        box.appendChild(this.treeNode(childRel, en.name, en, depth, `${r.dir}/${en.name}`));
      }
    } catch (e) {
      box.innerHTML = `<div class="ferr">${esc(String(e))}</div>`;
    }
    if (!el.isConnected) return null;
    el.after(box);
    arrow.textContent = "\u25be";
    return box;
  }

  /** Racine = cwd canonique de la session (renvoyé par list_dir ""). */
  private async loadTreeRoot() {
    const tree = $("file-tree");
    tree.innerHTML = "";
    if (!app.current) return;
    try {
      const r = await call<{ dir: string; entries: DirEntry[] }>("list_dir", {
        session: app.current.id,
        path: "",
      });
      const hd = document.createElement("div");
      hd.className = "froot";
      hd.innerHTML = `<span class="fico">${icon("folder")}</span><span class="fname">${esc(r.dir)}</span>`;
      hd.title = r.dir;
      this.treeRootDir = r.dir;
      tree.appendChild(hd);
      for (const en of r.entries) {
        tree.appendChild(this.treeNode(en.name, en.name, en, 0, `${r.dir}/${en.name}`));
      }
      if (!r.entries.length) {
        tree.insertAdjacentHTML("beforeend", `<div class="fempty">${t("tree_empty")}</div>`);
      }
    } catch (e) {
      tree.innerHTML = `<div class="ferr">${esc(String(e))}</div>`;
    }
  }

  private setTreeVisible(v: boolean) {
    this.treeOpen = v;
    $("ft-side").hidden = !v;
    $("btn-tree-refresh").hidden = !v;
    this.clearSearch();
    if (v && !$("file-panel").hidden) void this.loadTreeRoot();
  }

  // -- Recherche par nom de fichier (find_files démon) -----------------------

  private searchSeq = 0;
  private searchTimer = 0;

  /** Debounce la recherche à la frappe. */
  private onSearchInput() {
    window.clearTimeout(this.searchTimer);
    this.searchTimer = window.setTimeout(() => void this.runSearch(), 250);
  }

  /** Vide la recherche et rend sa place à l'arbre (annule aussi la
   *  requête en vol via searchSeq). */
  private clearSearch() {
    window.clearTimeout(this.searchTimer);
    this.searchSeq++;
    const q = $("ft-q") as HTMLInputElement;
    if (q.value) q.value = "";
    $("file-hits").hidden = true;
    $("file-hits").innerHTML = "";
    $("file-tree").hidden = false;
  }

  /** Recherche récursive côté démon ; repli sur le filtrage local des
   *  dossiers déjà ouverts si le démon ne connaît pas find_files. */
  private async runSearch() {
    const q = ($("ft-q") as HTMLInputElement).value.trim();
    const seq = ++this.searchSeq;
    if (!q) {
      $("file-hits").hidden = true;
      $("file-tree").hidden = false;
      return;
    }
    if (!app.current) return;
    const sid = app.current.id;
    $("file-tree").hidden = true;
    $("file-hits").hidden = false;
    try {
      const r = await call<{
        matches: { path: string; dir: boolean; size: number }[];
        truncated: boolean;
      }>("find_files", { session: sid, query: q });
      if (seq !== this.searchSeq || app.current?.id !== sid) return;
      this.renderHits(r.matches, r.truncated);
    } catch (e) {
      if (seq !== this.searchSeq || app.current?.id !== sid) return;
      // Démon sans find_files → erreur « parse: unknown variant …» ;
      // client hybride sans le bras req_from → « méthode inconnue ».
      // Volontairement étroit : « session inconnue » etc. ne doivent
      // pas tomber dans le repli.
      if (/unknown variant|méthode inconnue/i.test(String(e))) {
        this.localSearch(q.toLowerCase());
      } else {
        $("file-hits").innerHTML = `<div class="ferr">${esc(String(e))}</div>`;
      }
    }
  }

  /** Rendu des résultats : clic fichier → viewer, clic dossier → déplie
   *  l'arbre jusqu'au nœud. */
  private renderHits(matches: { path: string; dir: boolean; size: number }[], truncated: boolean) {
    const hits = $("file-hits");
    hits.innerHTML = "";
    for (const m of matches) hits.appendChild(this.hitNode(m.path, m.dir, fmtSize(m.size)));
    if (!matches.length) {
      hits.innerHTML = `<div class="fempty">${esc(t("file_search_none"))}</div>`;
    } else if (truncated) {
      hits.insertAdjacentHTML("beforeend", `<div class="fempty">${esc(t("file_search_trunc"))}</div>`);
    }
  }

  private hitNode(rel: string, dir: boolean, sizeText: string): HTMLElement {
    const name = rel.split("/").pop() ?? rel;
    const parent = rel.includes("/") ? rel.slice(0, rel.lastIndexOf("/")) : "";
    const d = document.createElement("div");
    d.className = "fhit" + (dir ? " is-dir" : "") + (name.startsWith(".") ? " fdot" : "");
    d.dataset.rel = rel;
    d.dataset.abs = this.treeRootDir ? `${this.treeRootDir}/${rel}` : rel;
    d.innerHTML =
      `<span class="fico">${icon(dir ? "folder" : "file")}</span>` +
      `<span class="fname">${esc(name)}</span>` +
      `<span class="fhdir">${esc(parent)}</span>` +
      (dir ? "" : `<span class="fsize">${esc(sizeText)}</span>`);
    d.title = rel;
    d.onclick = () => {
      if (dir) void this.revealDir(rel);
      else void this.openFilePanel(rel);
    };
    bindCtx(d, (x, y) => this.openFileCtx(x, y, d));
    return d;
  }

  /** Vieux démon sans find_files : filtre les nœuds déjà chargés dans
   *  l'arbre (dossiers ouverts seulement), avec un bandeau d'info. */
  private localSearch(lq: string) {
    const hits = $("file-hits");
    hits.innerHTML = `<div class="fempty">${esc(t("file_search_local"))}</div>`;
    for (const n of $("file-tree").querySelectorAll<HTMLElement>(".fnode")) {
      const name = n.dataset.rel?.split("/").pop() ?? "";
      if (!name.toLowerCase().includes(lq)) continue;
      const isDir = n.classList.contains("is-dir");
      const size = n.querySelector(".fsize")?.textContent ?? "";
      hits.appendChild(this.hitNode(n.dataset.rel ?? name, isDir, size));
    }
  }

  /** Déplie les ancêtres de `rel` dans l'arbre (chargement lazy niveau
   *  par niveau), sélectionne le nœud et le rend visible. */
  private async revealDir(rel: string) {
    this.clearSearch();
    const segs = rel.split("/");
    let container: HTMLElement = $("file-tree");
    for (let i = 0; i < segs.length; i++) {
      const cur = segs.slice(0, i + 1).join("/");
      let node: HTMLElement | null = null;
      for (const ch of Array.from(container.children)) {
        const h = ch as HTMLElement;
        if (h.classList.contains("fnode") && h.dataset.rel === cur) {
          node = h;
          break;
        }
      }
      if (!node) return; // renommé/supprimé entre-temps
      if (i === segs.length - 1) {
        for (const n of $("file-tree").querySelectorAll<HTMLElement>(".fnode.sel")) {
          n.classList.remove("sel");
        }
        node.classList.add("sel");
        node.scrollIntoView({ block: "nearest" });
        return;
      }
      if (!node.classList.contains("is-dir")) return;
      const box = await this.expandDir(node);
      if (!box) return;
      container = box;
    }
  }

  private hideFileCtx() {
    hideMenu(this.ctx, "fctx");
  }

  /** Menu contextuel clic droit : copier contenu / chemin absolu / supprimer.
   *  Android : rendu en action-sheet ancré en bas (pas de positionnement x/y). */
  private openFileCtx(x: number, y: number, el: HTMLElement) {
    const rel = el.dataset.rel ?? "";
    const abs = el.dataset.abs ?? rel;
    const isDir = el.classList.contains("is-dir");
    this.ctx.innerHTML = `<div class="fctx-path">${esc(abs)}</div>`;
    if (!isDir) {
      menuBtn(this.ctx, t("ctx_copy"), "", () => {
        this.hideFileCtx();
        void call<{ content: string }>("read_file", { session: app.current!.id, path: rel })
          .then((r) => copyText(r.content))
          .catch((e) => showErr(String(e)));
      });
    }
    menuBtn(this.ctx, t("ctx_copy_path"), "", () => {
      this.hideFileCtx();
      void copyText(abs);
    });
    menuBtn(this.ctx, t("ctx_download"), "", () => {
      this.hideFileCtx();
      void this.downloadEntry(rel);
    });
    menuBtn(this.ctx, t("ctx_delete"), "danger", (b) => {
      // Double-clic armé : "Supprimer" → "confirmer ?" → suppression
      if (!b.dataset.armed) {
        b.dataset.armed = "1";
        b.textContent = t("ctx_confirm");
        return;
      }
      this.hideFileCtx();
      void call("delete_path", { session: app.current!.id, path: rel })
        .then(() => {
          // Le même chemin peut être rendu dans l'arbre ET dans les
          // résultats de recherche — retirer les deux occurrences.
          for (const host of [$("file-tree"), $("file-hits")]) {
            for (const n of host.querySelectorAll<HTMLElement>(".fnode,.fhit")) {
              if (n.dataset.rel !== rel) continue;
              const nx = n.nextElementSibling;
              if (nx?.classList.contains("fchildren")) nx.remove();
              n.remove();
            }
          }
        })
        .catch((e) => showErr(String(e)));
    });
    placeMenu(this.ctx, "fctx", x, y);
  }

  /** Télécharge un fichier/dossier du workspace vers le disque local :
   *  infos (nom suggéré — .zip pour un dossier) → dialogue « Enregistrer
   *  sous » (Android : Download/AgentControl/ → repli app data) → tranches
   *  base64 démon → write_download. Session/conn figées : un changement de
   *  session en cours de route n'envoie pas les tranches sur le mauvais
   *  serveur (la sysline disparaît au switch, le téléchargement finit). */
  private async downloadEntry(rel: string) {
    if (!app.current) return;
    const sid = app.current.id;
    const conn = app.current.conn;
    const info = await call<{ name: string; dir: boolean; size: number | null }>(
      "download_path",
      { session: sid, conn, path: rel, offset: 0, limit: 0 },
    ).catch((e) => {
      showErr(String(e));
      return null;
    });
    if (!info) return;
    let dest: string | null = isAndroid()
      ? `${await downloadDir()}/AgentControl/${info.name}`
      : await save({ defaultPath: info.name, title: t("ctx_download") });
    if (!dest) return; // dialogue annulé
    const sys = this.deps.sysline(t("dl_progress", { name: info.name, pct: 0 }), "loader");
    let offset = 0;
    let archive: string | undefined;
    try {
      for (;;) {
        const c = await call<{ total: number; data: string; archive?: string }>(
          "download_path",
          { session: sid, conn, path: rel, offset, limit: 1 << 20, archive },
        );
        archive ??= c.archive;
        const n = await invoke<number>("write_download", {
          path: dest,
          data: c.data,
          append: offset > 0,
        }).catch(async (e): Promise<number> => {
          // Android : Download public peut refuser (pas de SAF) → app data.
          if (!isAndroid() || offset > 0) throw e;
          dest = `${await appDataDir()}/downloads/${info.name}`;
          return invoke<number>("write_download", { path: dest, data: c.data, append: false });
        });
        offset += n;
        sys.innerHTML = `${icon("loader")} ${esc(t("dl_progress", {
          name: info.name,
          pct: Math.round((offset * 100) / Math.max(c.total, 1)),
        }))}`;
        if (offset >= c.total) break;
      }
      sys.innerHTML = `${icon("check")} ${esc(t("dl_done", { name: info.name, dest }))}`;
    } catch (e) {
      sys.innerHTML = `${icon("warn")} ${esc(t("dl_failed", { name: info.name, err: String(e) }))}`;
      showErr(String(e));
    }
  }

  init() {
    $("file-close").onclick = () => this.closeFilePanel();
    // Retour viewer → liste (Android)
    $("fv-back").onclick = () => {
      stopVideo();
      nav.close("fileview");
      $("file-viewer").classList.remove("open");
    };
    // Redimensionnement du panneau par son bord gauche
    const panel = $("file-panel");
    const hd = $("file-resize");
    hd.addEventListener("mousedown", (e) => {
      e.preventDefault();
      const move = (ev: MouseEvent) => {
        // Bouton relâché hors fenêtre → mouseup perdu : auto-relâcher.
        if (ev.buttons === 0) {
          up();
          return;
        }
        const w = Math.min(
          Math.max(window.innerWidth - ev.clientX, 240),
          window.innerWidth - 120,
        );
        panel.style.width = `${w}px`;
        localStorage.setItem("ac-filepanel-w", String(w));
      };
      const up = () => {
        window.removeEventListener("mousemove", move);
        window.removeEventListener("mouseup", up);
      };
      window.addEventListener("mousemove", move);
      window.addEventListener("mouseup", up);
    });
    const saved = Number(localStorage.getItem("ac-filepanel-w"));
    if (saved > 240) panel.style.width = `${saved}px`;

    // Chemins de fichiers cliquables dans les messages → panneau latéral.
    $("events").addEventListener("click", (e) => {
      const el = (e.target as HTMLElement).closest<HTMLElement>(".flink");
      if (el?.dataset.path) void this.openFilePanel(el.dataset.path);
    });

    // 📁 dans le header : ouvre le panneau complet (arbre + viewer)
    $("btn-files").onclick = () => {
      const p = $("file-panel");
      const opening = p.hidden;
      if (opening) {
        p.hidden = false;
        nav.push("files");
        // L'arbre + le viewer ont besoin de largeur (desktop seulement :
        // sur Android le panneau est fixe plein écran, pas de width inline).
        if (!isAndroid()) {
          const w = Math.max(Number(p.style.width.replace("px", "")) || 0, 620);
          p.style.width = `${Math.min(w, window.innerWidth - 120)}px`;
        }
        this.setTreeVisible(true);
      } else {
        this.closeFilePanel();
      }
    };
    // 🗂 dans le panneau : bascule juste la colonne arborescence
    $("btn-tree").onclick = () => this.setTreeVisible($("ft-side").hidden);
    $("btn-tree-refresh").onclick = () => void this.loadTreeRoot();

    // Recherche par nom : debounce à la frappe, Échap efface (sans
    // déclencher les raccourcis globaux).
    const fq = $("ft-q");
    fq.addEventListener("input", () => this.onSearchInput());
    fq.addEventListener("keydown", (e) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        this.clearSearch();
      }
    });

    // Le menu se ferme au clic ailleurs / Échap / scroll de l'arbre — mais un
    // clic DANS le menu ne doit pas le fermer avant l'action (delete armé).
    this.ctx.addEventListener("click", (e) => e.stopPropagation());
    window.addEventListener("click", () => this.hideFileCtx());
    window.addEventListener("keydown", (e) => {
      if (e.key === "Escape") this.hideFileCtx();
    });
    $("file-tree").addEventListener("scroll", () => this.hideFileCtx());
    $("file-hits").addEventListener("scroll", () => this.hideFileCtx());
  }
}
