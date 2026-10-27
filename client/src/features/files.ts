// ---------------------------------------------------------------------------
// Explorateur de fichiers du workspace (list_dir/read_file/delete_path) :
// panneau latéral, arborescence paresseuse, viewer texte/image, menu
// contextuel et téléchargement par tranches.
// ---------------------------------------------------------------------------

import { invoke } from "@tauri-apps/api/core";
import { save } from "@tauri-apps/plugin-dialog";
import { appDataDir, downloadDir } from "@tauri-apps/api/path";
import { $, copyText, esc, showErr } from "../core/dom";
import { fmtSize, highlight, IMG_MIME } from "../core/format";
import { call } from "../core/rpc";
import { app } from "../core/state";
import { isAndroid } from "../core/chrome";
import type { DirEntry } from "../core/types";
import { t } from "../i18n";
import { icon, type IconName } from "../ui/icons";
import { bindCtx, hideMenu, menuBtn, placeMenu } from "../ui/ctxmenu";
import { nav } from "../ui/nav";

export class FileExplorer {
  private ctx = $("fctx");
  private treeOpen = false;
  private deps = {
    /** Ligne de progression de téléchargement dans le fil. */
    sysline: (_text: string, _ic?: IconName): HTMLElement => document.createElement("div"),
  };

  bind(d: Partial<typeof this.deps>) {
    Object.assign(this.deps, d);
  }

  /** L'arbre suit le workspace de la session nouvellement ouverte. */
  onSessionOpened() {
    if (this.treeOpen) void this.loadTreeRoot();
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
    $("file-view").textContent = "…";
    // Marque le noeud de l'arborescence correspondant
    for (const n of $("file-tree").querySelectorAll<HTMLElement>(".fnode")) {
      n.classList.toggle("sel", n.dataset.rel === path);
    }
    const ext = /\.([a-zA-Z0-9]+)$/.exec(path)?.[1]?.toLowerCase() ?? "";
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
    // Le viewer glissé par-dessus doit être dépilé aussi (pile back propre).
    nav.close("fileview");
    $("file-viewer").classList.remove("open");
    nav.close("files");
    $("file-panel").hidden = true;
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
      `<span class="fico">${en.dir ? "\u{1f4c1}" : "\u{1f4c4}"}</span>` +
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
    if (el.isConnected) {
      el.after(box);
      arrow.textContent = "\u25be";
    }
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
      hd.innerHTML = `<span class="fico">\u{1f4c1}</span><span class="fname">${esc(r.dir)}</span>`;
      hd.title = r.dir;
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
    $("file-tree").hidden = !v;
    $("btn-tree-refresh").hidden = !v;
    if (v && !$("file-panel").hidden) void this.loadTreeRoot();
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
          const nx = el.nextElementSibling;
          if (nx?.classList.contains("fchildren")) nx.remove();
          el.remove();
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
      nav.close("fileview");
      $("file-viewer").classList.remove("open");
    };
    // Redimensionnement du panneau par son bord gauche
    const panel = $("file-panel");
    const hd = $("file-resize");
    hd.addEventListener("mousedown", (e) => {
      e.preventDefault();
      const move = (ev: MouseEvent) => {
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
    $("btn-tree").onclick = () => this.setTreeVisible($("file-tree").hidden);
    $("btn-tree-refresh").onclick = () => void this.loadTreeRoot();

    // Le menu se ferme au clic ailleurs / Échap / scroll de l'arbre — mais un
    // clic DANS le menu ne doit pas le fermer avant l'action (delete armé).
    this.ctx.addEventListener("click", (e) => e.stopPropagation());
    window.addEventListener("click", () => this.hideFileCtx());
    window.addEventListener("keydown", (e) => {
      if (e.key === "Escape") this.hideFileCtx();
    });
    $("file-tree").addEventListener("scroll", () => this.hideFileCtx());
  }
}
