// ---------------------------------------------------------------------------
// Explorateur de fichiers du workspace (list_dir/read_file/write_file/
// delete_path) : panneau latéral, arborescence paresseuse, viewer
// texte/image/vidéo — texte chargé par tranches (fichiers de toute taille) —
// éditeur intégré (coloration live, encodage, garde anti-écrasement mtime),
// menu contextuel et téléchargement par tranches.
// ---------------------------------------------------------------------------

import { invoke, isWeb, listen } from "../core/bridge";
import { wsClient } from "../core/wsclient";
import { save } from "@tauri-apps/plugin-dialog";
import { appDataDir, downloadDir } from "@tauri-apps/api/path";
import { $, copyText, esc, showErr } from "../core/dom";
import { fmtSize, highlight, IMG_MIME, VID_MIME } from "../core/format";
import {
  b64decode,
  b64encode,
  defaultEncoding,
  encode,
  ENCODINGS,
  makeDecoder,
  sniffBinary,
  sniffBom,
  type Encoding,
} from "../core/encoding";
import { call, callBg } from "../core/rpc";
import { app } from "../core/state";
import { pushRecentFile, recentsId } from "../core/recent-files";
import { isAndroid, isWindows } from "../core/chrome";
import type { DirEntry } from "../core/types";
import { t } from "../i18n";
import { icon, type IconName } from "../ui/icons";
import { MdView } from "./md-view";
import { PdfView } from "./pdf-view";
import { bindCtx, hideMenu, menuBtn, placeMenu } from "../ui/ctxmenu";
import { nav } from "../ui/nav";

/** URL du schéma custom acvideo — forme imposée par plateforme :
 *  http://acvideo.localhost sous Windows/Android, acvideo:// ailleurs.
 *  En mode web : endpoint HTTP natif /media du démon (ranges réelles,
 *  token en query — le navigateur ne peut pas poser de header sur <video>). */
function videoUrl(path: string): string {
  const q = new URLSearchParams({ session: app.current?.id ?? "", path });
  if (isWeb) {
    q.set("token", wsClient.token() ?? "");
    return `/media?${q}`;
  }
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

/** Tranche réseau du pipeline progressif. */
const PAGE = 256 * 1024;
/** Au-delà, pas de re-coloration en édition (highlight() est O(n)). */
const HL_MAX = 1500 * 1024;
/** Gouttière de numéros : recomptée à la frappe — capée pour la perf. */
const NUMS_MAX = 300_000;

/** Fichier texte ouvert dans le viewer — chargé progressivement. */
interface CurFile {
  rel: string;
  abs: string;              // chemin résolu renvoyé par le démon
  ext: string;
  sid: string;              // session/conn figées à l'ouverture (le write
  conn?: string;            //   vise toujours le bon serveur)
  enc: Encoding;
  bom: boolean;             // BOM détectée sur la 1re tranche → ré-écrite
  crlf: boolean;            // fins de ligne CRLF → ré-écrites à la sauvegarde
  size: number;             // taille disque à la dernière tranche lue
  mtimeMs: number;          // mtime disque à la dernière tranche lue (garde)
  loaded: number;           // octets du fichier chargés (curseur read)
  decoder: ((b: Uint8Array, last?: boolean) => string) | null;
  chunks: string[];         // texte décodé par tranche (jamais re-joint)
  binary: boolean;          // sniffé binaire → vue seule, pas d'édition
  fetching: boolean;        // tranche réseau en vol
  done: boolean;            // loaded >= size
  gen: number;              // ++ à chaque save → tranches en vol invalidées
}

export class FileExplorer {
  private ctx = $("fctx");
  private treeOpen = false;
  /** Chargement racine en vol (ou résolu) — await par revealInTree avant
   *  de déplier l'arbre, sinon expandTo tomberait sur un arbre vide. */
  private treeReady: Promise<void> | null = null;
  /** Watch FS actifs côté démon, clé `${sid}\0${rel}` — posés au
   *  dépliage d'un dossier affiché, retirés au repli/fermeture/switch.
   *  session+conn épinglés : l'unwatch vise toujours le bon démon même
   *  après un switch de session. */
  private watches = new Map<string, { sid: string; conn?: string; rel: string }>();
  /** Conns dont le démon ne connaît pas watch_dir (vieux) : plus de
   *  tentatives — le refresh manuel reste le recours (compat). */
  private watchDead = new Set<string>();
  /** cwd absolu de la session (rempli par loadTreeRoot) — sert à
   *  reconstruire le chemin absolu des résultats de recherche. */
  private treeRootDir = "";
  private cur: CurFile | null = null;
  private editOn = false;
  private dirty = false;
  private pdf = new PdfView();
  private mdView = new MdView({
    text: () => this.cur?.chunks.join("") ?? "",
    done: () => this.cur?.done ?? true,
    openFile: (rel) => void this.openFilePanel(rel),
    more: () => this.maybeMore(),
  });
  private cleanRes: ((ok: boolean) => void) | null = null;
  private saving = false;
  private hlTimer: ReturnType<typeof setTimeout> | undefined;
  private numsTimer: ReturnType<typeof setTimeout> | undefined;
  private deps = {
    /** Ligne de progression de téléchargement dans le fil. */
    sysline: (_text: string, _ic?: IconName): HTMLElement => document.createElement("div"),
  };

  bind(d: Partial<typeof this.deps>) {
    Object.assign(this.deps, d);
  }

  /** Préférence « rendu Markdown par défaut » changée dans les
   *  paramètres — propagée au viewer pour basculer la vue courante. */
  setMdRender(on: boolean) {
    this.mdView.setRender(on);
  }

  /** L'arbre suit le workspace de la session nouvellement ouverte. */
  onSessionOpened() {
    // Les watches épinglent l'ancienne session — les lâcher même si le
    // panneau est masqué (reconnexion : ils sont morts côté démon de
    // toute façon, l'unwatch tombe alors à 0, sans effet).
    this.unwatchAll();
    if (this.treeOpen) {
      this.clearSearch();
      this.treeReady = this.loadTreeRoot();
    }
  }

  async openFilePanel(path: string, opts?: { edit?: boolean; enc?: Encoding }) {
    if (!app.current) return;
    if (!(await this.ensureClean())) return;
    this.resetView();
    // Ouverture réelle (la garde dirty est passée) → tête des récents
    // de la palette quick-open. Chemins relatifs et absolus enregistrés.
    pushRecentFile(recentsId(app.current.id, app.current.conn), path);
    if ($("file-panel").hidden) {
      $("file-panel").hidden = false;
      nav.push("files");
      // Ré-ouverture : les watches ont été lâchés à la fermeture et
      // l'arbre affiché peut être périmé — reload si la colonne est on.
      if (this.treeOpen) this.treeReady = this.loadTreeRoot();
    }
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
    if (ext === "pdf") {
      // Rendu canvas pdf.js — module dédié (hors pipeline texte/image).
      await this.pdf.open(path);
      return;
    }
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
        const r = await callBg<{
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
      // Texte : pipeline progressif — la 1re page s'affiche tout de suite,
      // la suite arrive au fil du scroll (ou « tout charger »).
      const cur: CurFile = {
        rel: path, abs: "", ext,
        sid: app.current.id, conn: app.current.conn,
        enc: opts?.enc ?? defaultEncoding(),
        bom: false, crlf: false, size: 0, mtimeMs: 0, loaded: 0,
        decoder: null, chunks: [], binary: false,
        fetching: false, done: false, gen: 0,
      };
      this.cur = cur;
      this.mdView.attach(path, ext);
      this.syncEncSelect();
      this.updateStatus();
      await this.fetchChunk(cur);
      if (opts?.edit && this.cur === cur && this.editable()) this.enterEdit();
    } catch (e) {
      this.mdView.detach();
      $("file-view").textContent = String(e);
    }
  }

  /** Ramène viewer + état d'édition à zéro (changement de fichier). */
  private resetView() {
    this.cur = null;
    this.editOn = false;
    this.dirty = false;
    this.mdView.detach();
    this.pdf.detach();
    $("file-edwrap").hidden = true;
    $("file-view").hidden = false;
    $("file-view").innerHTML = "";
    $("fv-edit").hidden = true;
    $("fv-edit").classList.remove("on");
    $("fv-save").hidden = true;
    $("fv-conflict").hidden = true;
    $("fv-loadall").hidden = true;
    $("fv-status").textContent = "";
  }

  // ------------------------------------------------------------------
  // Chargement progressif (read_file offset/limit → base64 → décodage)
  // ------------------------------------------------------------------

  /** Récupère la tranche suivante et l'ajoute au rendu courant. Sérialisé :
   *  un fetch en vol fait attendre l'appelant (loadAll ne peut pas spinner) ;
   *  un save en vol aussi — une tranche lue pendant write_file serait un
   *  mélange ancien/nouveau (la taille inchangée passerait le garde gen). */
  private async fetchChunk(cur: CurFile) {
    while (cur.fetching || this.saving) await new Promise((r) => setTimeout(r, 30));
    if (this.cur !== cur || cur.done || cur.binary) return;
    cur.fetching = true;
    const gen = cur.gen;
    try {
      const r = await callBg<{
        path: string; offset: number; size: number; data: string;
        mtime_ms?: number;
      }>("read_file", {
        session: cur.sid, conn: cur.conn, path: cur.rel,
        offset: cur.loaded, limit: PAGE,
      });
      // Tranche de l'ANCIEN fichier : saveEdit a bumpé gen pendant le vol
      // → on jette, le prochain fetch repart du nouveau curseur.
      if (this.cur !== cur || cur.gen !== gen) return;
      const bytes = b64decode(r.data ?? "");
      // Course avec un autre changement de curseur : la tranche attendue
      // n'est plus à cet offset → on la jette pareil.
      if (r.offset !== cur.loaded) return;
      cur.size = r.size;
      if (r.mtime_ms != null) cur.mtimeMs = r.mtime_ms;
      cur.loaded = r.offset + bytes.length;
      if (cur.abs === "") {
        // Première tranche : chemin résolu + BOM (vide → rien à sniffer).
        cur.abs = r.path;
        $("file-path").title = r.path;
        const bomEnc = sniffBom(bytes);
        cur.bom = bomEnc != null;
        if (bomEnc) cur.enc = bomEnc;
        this.syncEncSelect();
      }
      if (!bytes.length) {
        cur.done = true;
        this.updateStatus();
        return;
      }
      cur.decoder ??= makeDecoder(cur.enc);
      const text = cur.decoder(bytes, cur.loaded >= cur.size);
      if (cur.chunks.length === 0) cur.crlf = text.includes("\r\n");
      if (cur.loaded === bytes.length && sniffBinary(text)) {
        cur.binary = true;
        cur.done = true;
        this.mdView.detach();
        $("file-view").innerHTML =
          `<div class="fv-msg">${esc(t("binary_file", { size: fmtSize(cur.size) }))}</div>`;
        this.updateStatus();
        return;
      }
      cur.chunks.push(text);
      if (cur.loaded >= cur.size) cur.done = true;
      this.appendChunk(cur, text);
      this.updateStatus();
    } catch (e) {
      cur.done = true;
      if (this.cur === cur) {
        // En mode rendu, #file-view est masqué → l'erreur serait invisible
        // (« … » éternel ou document tronqué en silence) : retomber en vue
        // source comme pour un binaire.
        this.mdView.detach();
        $("file-view").textContent = String(e);
        this.updateStatus();
      }
    } finally {
      cur.fetching = false;
    }
  }

  /** Proche du bas de la zone chargée → tranche suivante. */
  private maybeMore() {
    const cur = this.cur;
    if (!cur || cur.fetching || cur.done || cur.binary) return;
    void this.fetchChunk(cur);
  }

  /** « Tout charger » : tranches séquentielles jusqu'au bout du fichier. */
  private async loadAll() {
    const cur = this.cur;
    if (!cur) return;
    while (this.cur === cur && !cur.done && !cur.binary) {
      await this.fetchChunk(cur);
    }
  }

  /** Ajoute le texte décodé à la représentation visible (viewer coloré
   *  ou buffer d'édition + overlay). */
  private appendChunk(cur: CurFile, text: string) {
    if (this.editOn) {
      const ta = $("file-edit") as HTMLTextAreaElement;
      // setRangeText préserve le caret — append en fond pendant la frappe.
      ta.setRangeText(text, ta.value.length, ta.value.length);
      this.scheduleHl();
      this.scheduleNums();
    } else {
      $("file-view").insertAdjacentHTML("beforeend", highlight(text, cur.ext));
    }
    this.mdView.changed();
  }

  /** Éditable : texte non-binaire dont le chemin résolu reste dans le cwd
   *  de la session (write_file est borné au workspace côté démon). Le
   *  `path` renvoyé par read_file n'est pas canonisé → segments `..`
   *  normalisés ici avant le test de préfixe. */
  private editable(): boolean {
    const cur = this.cur;
    if (!cur || cur.binary || !app.current) return false;
    if (!cur.abs) return !cur.rel.startsWith("/"); // en attente du 1er octet
    const norm = (p: string) =>
      "/" + p.split("/").reduce<string[]>((acc, seg) => {
        if (seg === "..") acc.pop();
        else if (seg !== "." && seg !== "") acc.push(seg);
        return acc;
      }, []).join("/");
    const cwd = norm(app.current.cwd).replace(/\/+$/, "");
    const abs = norm(cur.abs);
    return abs.startsWith(cwd + "/") || abs === cwd;
  }

  private updateStatus() {
    const st = $("fv-status");
    const cur = this.cur;
    if (!cur) { st.textContent = ""; return; }
    if (this.dirty) {
      st.textContent = `● ${t("fv_dirty")}`;
    } else if (!cur.done && !cur.binary) {
      st.textContent = `${fmtSize(cur.loaded)} / ${fmtSize(cur.size)}`;
    } else {
      st.textContent = fmtSize(cur.size);
    }
    ($("fv-save") as HTMLButtonElement).disabled = !this.dirty;
    $("fv-edit").hidden = !(this.editOn || this.editable());
    $("fv-loadall").hidden = cur.done || cur.binary;
  }

  private syncEncSelect() {
    const cur = this.cur;
    if (cur) ($("fv-enc") as HTMLSelectElement).value = cur.enc;
  }

  // ------------------------------------------------------------------
  // Éditeur : textarea transparente sur overlay coloré + gouttière
  // ------------------------------------------------------------------

  private enterEdit() {
    const cur = this.cur;
    // `abs` vide = première tranche pas encore arrivée → on attend
    // (éditer un buffer vide sur un fichier existant = insertion en tête).
    if (!cur || !this.editable() || !cur.abs) return;
    this.editOn = true;
    this.mdView.setEditing(true);
    const ta = $("file-edit") as HTMLTextAreaElement;
    ta.value = cur.chunks.join("");
    $("file-view").hidden = true;
    $("file-edwrap").hidden = false;
    $("fv-edit").classList.add("on");
    $("fv-save").hidden = false;
    this.refreshHl();
    this.refreshNums();
    this.updateStatus();
    ta.focus();
  }

  private exitEdit() {
    if (!this.editOn) return;
    void (async () => {
      if (!(await this.ensureClean())) return;
      this.exitEditNow();
    })();
  }

  /** Sortie effective : le viewer re-rend le contenu courant (sauvegardé
   *  = buffer édité, abandonné = chunks disque). */
  private exitEditNow() {
    const cur = this.cur;
    this.editOn = false;
    this.dirty = false;
    $("file-edwrap").hidden = true;
    $("file-view").hidden = false;
    $("fv-edit").classList.remove("on");
    $("fv-save").hidden = true;
    $("fv-conflict").hidden = true;
    if (cur) {
      const view = $("file-view");
      view.innerHTML = "";
      for (const c of cur.chunks) {
        view.insertAdjacentHTML("beforeend", highlight(c, cur.ext));
      }
    }
    // Le buffer sauvegardé peut différer des chunks affichés avant
    // l'édition → rendu Markdown à refaire avant de le ré-afficher.
    this.mdView.changed();
    this.mdView.setEditing(false);
    this.updateStatus();
  }

  private onEditInput() {
    if (!this.editOn) return;
    this.dirty = true;
    this.updateStatus();
    this.scheduleHl();
    this.scheduleNums();
  }

  /** Re-coloration debouncée : highlight() est un regex global O(n) —
   *  au-delà de HL_MAX on bascule en miroir plain (le texte reste éditable). */
  private scheduleHl() {
    clearTimeout(this.hlTimer);
    this.hlTimer = setTimeout(() => this.refreshHl(), 140);
  }

  private refreshHl() {
    const cur = this.cur;
    if (!cur) return;
    const ta = $("file-edit") as HTMLTextAreaElement;
    const hl = $("file-hl");
    // \n final : le textarea « voit » une ligne de plus quand le texte
    // finit par \n — le pre doit avoir la même hauteur de scroll.
    if (ta.value.length > HL_MAX) {
      hl.textContent = ta.value + "\n";
    } else {
      hl.innerHTML = highlight(ta.value, cur.ext) + "\n";
    }
  }

  private scheduleNums() {
    clearTimeout(this.numsTimer);
    this.numsTimer = setTimeout(() => this.refreshNums(), 150);
  }

  private refreshNums() {
    const ta = $("file-edit") as HTMLTextAreaElement;
    const nums = $("file-nums");
    // Comptage sans split : au-delà du cap on sort tôt (pas d'allocation
    // de tableau de lignes sur un fichier énorme).
    const v = ta.value;
    let lines = v === "" ? 0 : 1;
    for (let i = 0; i < v.length && lines <= NUMS_MAX; i++) {
      if (v.charCodeAt(i) === 10) lines++;
    }
    if (lines > NUMS_MAX) {
      nums.hidden = true;
      return;
    }
    nums.hidden = false;
    nums.textContent = Array.from({ length: Math.max(lines, 1) }, (_, i) => i + 1).join("\n");
  }

  /** Scroll du textarea → overlay + gouttière suivent ; proche du bas →
   *  tranche suivante du fichier. */
  private syncEditScroll() {
    const ta = $("file-edit") as HTMLTextAreaElement;
    $("file-hl").scrollTop = ta.scrollTop;
    $("file-hl").scrollLeft = ta.scrollLeft;
    $("file-nums").scrollTop = ta.scrollTop;
    if (ta.scrollTop + ta.clientHeight >= ta.scrollHeight * 0.85) this.maybeMore();
  }

  /** Le buffer = préfixe du fichier disque : sauvegarder remplace les
   *  `cur.loaded` premiers octets par le buffer encodé — inutile d'avoir
   *  tout chargé pour sauver (le reliquat disque est conservé). */
  private async saveEdit(force = false) {
    const cur = this.cur;
    if (!cur || !this.editOn || this.saving) return;
    this.saving = true;
    // Invalide les tranches réseau en vol : elles portent du contenu de
    // l'ancien fichier et souilleraient le buffer édité.
    cur.gen++;
    const ta = $("file-edit") as HTMLTextAreaElement;
    // Le textarea normalise les fins de ligne en \n — on ré-écrit \r\n
    // si le fichier était CRLF (sinon diffs parasites sur tout le fichier).
    const text = cur.crlf ? ta.value.replace(/\r?\n/g, "\r\n") : ta.value;
    const bytes = encode(cur.enc, text, cur.bom);
    const b = $("fv-save") as HTMLButtonElement;
    b.disabled = true;
    let saved = false;
    try {
      const r = await call<{ path: string; size: number; mtime_ms?: number }>(
        "write_file",
        {
          session: cur.sid, conn: cur.conn, path: cur.rel,
          offset: 0, delete_len: cur.loaded, data: b64encode(bytes),
          ...(force || !cur.mtimeMs ? {} : { if_mtime_ms: cur.mtimeMs }),
        },
      );
      if (r.mtime_ms != null) cur.mtimeMs = r.mtime_ms;
      cur.size = r.size;
      cur.loaded = bytes.length;
      cur.chunks = [ta.value];
      // Frontière octets neuve : une éventuelle séquence multi-octets à
      // cheval sur l'ancienne frontière ne doit pas se rejouer.
      cur.decoder = null;
      this.dirty = false;
      $("fv-conflict").hidden = true;
      saved = true;
    } catch (e) {
      if (String(e).includes("conflit:")) {
        $("fv-conflict").hidden = false;
      } else {
        showErr(String(e));
      }
    } finally {
      this.saving = false;
      this.updateStatus();
      // « ✓ enregistré » posé après updateStatus (il réécrit le statut).
      if (saved) $("fv-status").textContent = `✓ ${t("fv_saved")}`;
    }
  }

  /** Bandeau conflit : disque changé depuis la lecture — Recharger
   *  (ré-ouvre, jette le buffer) ou Écraser (renvoi sans garde). */
  private async reloadFile() {
    const cur = this.cur;
    this.dirty = false;
    const edit = this.editOn;
    if (cur) await this.openFilePanel(cur.rel, { edit, enc: cur.enc });
  }

  // ------------------------------------------------------------------
  // Garde « modifications non enregistrées »
  // ------------------------------------------------------------------

  /** false → l'action appelante doit s'arrêter (dialog affiché). */
  private ensureClean(): Promise<boolean> {
    if (!this.dirty) return Promise.resolve(true);
    const dlg = $("dlg-filedirty") as HTMLDialogElement;
    if (dlg.open) {
      // Un ensureClean est déjà en cours — la même résolution s'applique.
      return new Promise((res) => {
        const prev = this.cleanRes;
        this.cleanRes = (ok) => { prev?.(ok); res(ok); };
      });
    }
    return new Promise((res) => {
      this.cleanRes = res;
      dlg.showModal();
    });
  }

  private resolveClean(ok: boolean) {
    const r = this.cleanRes;
    this.cleanRes = null;
    const dlg = $("dlg-filedirty") as HTMLDialogElement;
    if (dlg.open) dlg.close();
    r?.(ok);
  }

  private closeFilePanel() {
    void (async () => {
      if (!(await this.ensureClean())) return;
      // Coupe la lecture en cours — sinon le stream acvideo continue en
      // tâche de fond après fermeture du panneau.
      stopVideo();
      this.resetView();
      // Le viewer glissé par-dessus doit être dépilé aussi (pile back propre).
      nav.close("fileview");
      $("file-viewer").classList.remove("open");
      nav.close("files");
      $("file-panel").hidden = true;
      this.clearSearch();
      // Panneau fermé : aucun dossier affiché → zéro watch.
      this.unwatchAll();
    })();
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

  // ------------------------------------------------------------------
  // Auto-refresh via watch FS démon (watch_dir → push dir-changed)
  // ------------------------------------------------------------------

  /** Abonne un dossier affiché — fire-and-forget : plafond démon
   *  atteint ou vieux démon sans watch_dir → non suivi, silencieux. */
  private watchDir(sid: string, conn: string | undefined, rel: string) {
    const ck = conn ?? "";
    if (this.watchDead.has(ck)) return;
    const key = `${sid}\0${rel}`;
    if (this.watches.has(key)) return;
    this.watches.set(key, { sid, conn, rel });
    void callBg<{ watching?: boolean }>("watch_dir", {
      session: sid, conn, path: rel,
    })
      .then((r) => {
        if (r.watching === false) this.watches.delete(key);
      })
      .catch((e) => {
        this.watches.delete(key);
        // Démon trop vieux pour watch_dir → ne plus jamais tenter
        // (la variante inconnue revient en « parse: … »).
        if (/unknown variant|parse:|inconnue/i.test(String(e))) {
          this.watchDead.add(ck);
        }
      });
  }

  /** Désabonne un dossier — fire-and-forget. */
  private unwatchDir(sid: string, conn: string | undefined, rel: string) {
    if (!this.watches.delete(`${sid}\0${rel}`)) return;
    void callBg("unwatch_dir", { session: sid, conn, path: rel }).catch(() => {});
  }

  /** Boîte d'enfants retirée de l'affichage : tous les dossiers dépliés
   *  qu'elle contient ne sont plus montrés → unwatch du sous-arbre. */
  private unwatchBox(box: HTMLElement, sid: string, conn: string | undefined) {
    for (const n of box.querySelectorAll<HTMLElement>(".fnode.is-dir")) {
      const nx = n.nextElementSibling;
      if (nx?.classList.contains("fchildren") && n.dataset.rel != null) {
        this.unwatchDir(sid, conn, n.dataset.rel);
      }
    }
  }

  /** Tous les watches (fermeture/masquage/switch) — un appel par
   *  session, `path` absent = tous ses watches côté démon. */
  private unwatchAll() {
    const bySid = new Map<string, string | undefined>();
    for (const [k, w] of this.watches) {
      bySid.set(w.sid, w.conn);
      this.watches.delete(k);
    }
    for (const [sid, conn] of bySid) {
      void callBg("unwatch_dir", { session: sid, conn }).catch(() => {});
    }
  }

  /** `.fchildren` déplié du dossier `rel`, null si replié ou absent. */
  private expandedBox(rel: string): HTMLElement | null {
    for (const n of $("file-tree").querySelectorAll<HTMLElement>(".fnode")) {
      if (n.dataset.rel === rel) {
        const nx = n.nextElementSibling;
        return nx?.classList.contains("fchildren") ? (nx as HTMLElement) : null;
      }
    }
    return null;
  }

  /** Push dir_changed : recharge chirurgicalement le dossier déplié —
   *  scroll, sélection et sous-arbres dépliés préservés (leurs watches
   *  propres les rafraîchiront), contenu inchangé = zéro retouche DOM.
   *  Panneau/arbre masqué ou autre session : ignoré — le rechargement
   *  à l'affichage couvre la staleness (dirty sans travail pour rien). */
  private onDirChanged(m: { session?: string; path?: string }) {
    if (!app.current || m.session !== app.current.id) return;
    if (!this.treeOpen || $("file-panel").hidden) return;
    const rel = m.path ?? "";
    const box = rel === "" ? $("file-tree") : this.expandedBox(rel);
    if (!box) return;
    void this.refreshBox(box, rel);
  }

  /** Reconcile des entrées directes de `box` avec le list_dir courant :
   *  ajouts insérés triés, disparitions retirées (+ leur sous-arbre et
   *  ses watches), tailles rafraîchies — rien d'autre n'est touché. */
  private async refreshBox(box: HTMLElement, rel: string) {
    const sid = app.current?.id;
    if (!sid) return;
    let r: { dir: string; entries: DirEntry[] };
    try {
      r = await callBg<{ dir: string; entries: DirEntry[] }>("list_dir", {
        session: sid, path: rel,
      });
    } catch {
      return; // dossier parti/perms — le watch du parent fera le ménage
    }
    // Arbre reconstruit ou session changée pendant l'appel → la boîte
    // détachée n'est plus la cible.
    if (!box.isConnected || app.current?.id !== sid) return;
    const depth = rel ? rel.split("/").length : 0;
    const conn = app.current.conn;
    const cur = new Map<string, HTMLElement>();
    for (const ch of Array.from(box.children)) {
      const h = ch as HTMLElement;
      // .fnew = saisie de nom en cours : pas de data-rel, hors listing —
      // le reconcile ne doit pas tuer la frappe de l'utilisateur.
      if (h.classList.contains("fnode") && !h.classList.contains("fnew")) {
        cur.set(h.dataset.rel ?? "", h);
      }
    }
    for (const en of r.entries) {
      const childRel = rel ? `${rel}/${en.name}` : en.name;
      const el = cur.get(childRel);
      if (el) {
        cur.delete(childRel);
        if (el.classList.contains("is-dir") !== en.dir) {
          // fichier ↔ dossier : le nœud est recréé (classe + handler
          // distincts), sa boîte d'enfants et ses watches partent —
          // le sien inclus s'il était déplié.
          const nx = el.nextElementSibling;
          if (nx?.classList.contains("fchildren")) {
            this.unwatchBox(nx as HTMLElement, sid, conn);
            nx.remove();
            this.unwatchDir(sid, conn, childRel);
          }
          const node = this.treeNode(childRel, en.name, en, depth, `${r.dir}/${en.name}`);
          if (el.classList.contains("sel")) node.classList.add("sel");
          el.replaceWith(node);
        } else if (!en.dir) {
          const sz = el.querySelector(".fsize");
          if (sz) sz.textContent = fmtSize(en.size);
        }
        continue;
      }
      this.insertSorted(box, this.treeNode(childRel, en.name, en, depth, `${r.dir}/${en.name}`));
    }
    // Entrées disparues : nœud + boîte d'enfants + watches (sous-arbre
    // déplié ET le dossier lui-même, sinon fuite de watch démon).
    for (const el of cur.values()) {
      const nx = el.nextElementSibling;
      if (nx?.classList.contains("fchildren")) {
        this.unwatchBox(nx as HTMLElement, sid, conn);
        nx.remove();
        const rel = el.dataset.rel;
        if (rel != null) this.unwatchDir(sid, conn, rel);
      }
      el.remove();
    }
    box.querySelector(":scope > .fempty")?.remove();
    box.querySelector(":scope > .ferr")?.remove();
    if (!box.querySelector(":scope > .fnode")) {
      box.insertAdjacentHTML("beforeend", `<div class="fempty">${t("tree_empty")}</div>`);
    }
  }

  /** Ouvre/replie un dossier — chargement paresseux à chaque expansion
   *  (le contenu change pendant que l'agent travaille, pas de cache). */
  private async toggleDir(el: HTMLElement) {
    const arrow = el.querySelector<HTMLElement>(".farrow")!;
    const next = el.nextElementSibling;
    if (next?.classList.contains("fchildren")) {
      // Repli : le dossier et tout son sous-arbre déplié quittent
      // l'affichage → unwatch (descendants d'abord, puis le dossier).
      const box = next as HTMLElement;
      const sid = app.current?.id;
      const conn = app.current?.conn;
      if (sid) {
        this.unwatchBox(box, sid, conn);
        const rel = el.dataset.rel;
        if (rel != null) this.unwatchDir(sid, conn, rel);
      }
      box.remove();
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
    const sid = app.current?.id;
    const conn = app.current?.conn;
    if (!sid) { arrow.textContent = "\u25b8"; return null; }
    arrow.textContent = "…";
    const box = document.createElement("div");
    box.className = "fchildren";
    try {
      const r = await callBg<{ dir: string; entries: DirEntry[] }>("list_dir", {
        session: sid,
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
    // Dossier désormais affiché → watch FS (silencieux si non suivi).
    this.watchDir(sid, conn, rel);
    return box;
  }

  /** Racine = cwd canonique de la session (renvoyé par list_dir ""). */
  private async loadTreeRoot() {
    const tree = $("file-tree");
    // Reconstruction complète : les dossiers dépliés affichés changent
    // → tous les watches repartent (le re-watch de "" suit juste après).
    this.unwatchAll();
    tree.innerHTML = "";
    if (!app.current) return;
    try {
      const r = await callBg<{ dir: string; entries: DirEntry[] }>("list_dir", {
        session: app.current.id,
        path: "",
      });
      const hd = document.createElement("div");
      hd.className = "froot";
      hd.dataset.rel = "";
      hd.dataset.abs = r.dir;
      hd.innerHTML = `<span class="fico">${icon("folder")}</span><span class="fname">${esc(r.dir)}</span>`;
      hd.title = r.dir;
      this.treeRootDir = r.dir;
      bindCtx(hd, (x, y) => this.openFileCtx(x, y, hd));
      tree.appendChild(hd);
      for (const en of r.entries) {
        tree.appendChild(this.treeNode(en.name, en.name, en, 0, `${r.dir}/${en.name}`));
      }
      if (!r.entries.length) {
        tree.insertAdjacentHTML("beforeend", `<div class="fempty">${t("tree_empty")}</div>`);
      }
      // La racine est toujours affichée quand l'arbre l'est → watchée.
      this.watchDir(app.current.id, app.current.conn, "");
    } catch (e) {
      tree.innerHTML = `<div class="ferr">${esc(String(e))}</div>`;
    }
  }

  private setTreeVisible(v: boolean) {
    this.treeOpen = v;
    $("ft-side").hidden = !v;
    $("btn-tree-refresh").hidden = !v;
    this.clearSearch();
    if (v && !$("file-panel").hidden) this.treeReady = this.loadTreeRoot();
    // Arbre masqué : plus aucun dossier affiché → zéro watch (dirty
    // sans travail ; le re-affichage recharge de toute façon).
    if (!v) this.unwatchAll();
  }

  /** Palette quick-open : révèle un dossier dans l'arbre — panneau et
   *  arborescence ouverts, chargement racine attendu avant le dépliage.
   *  btn-files.click() fait déjà setTreeVisible(true) → pas de second
   *  loadTreeRoot à chaîner ici. */
  async revealInTree(rel: string) {
    if ($("file-panel").hidden) $("btn-files").click();
    else if (!this.treeOpen) this.setTreeVisible(true);
    await this.treeReady;
    void this.revealDir(rel);
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
      const r = await callBg<{
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
    const node = await this.expandTo(rel);
    if (!node) return;
    for (const n of $("file-tree").querySelectorAll<HTMLElement>(".fnode.sel")) {
      n.classList.remove("sel");
    }
    node.classList.add("sel");
    node.scrollIntoView({ block: "nearest" });
  }

  /** Déplie les ancêtres de `rel` et renvoie son nœud — null si un
   *  segment a disparu entre-temps ou n'est pas un dossier. */
  private async expandTo(rel: string): Promise<HTMLElement | null> {
    const segs = rel.split("/");
    let container: HTMLElement = $("file-tree");
    let node: HTMLElement | null = null;
    for (let i = 0; i < segs.length; i++) {
      const cur = segs.slice(0, i + 1).join("/");
      node = null;
      for (const ch of Array.from(container.children)) {
        const h = ch as HTMLElement;
        if (h.classList.contains("fnode") && h.dataset.rel === cur) {
          node = h;
          break;
        }
      }
      if (!node) return null;
      if (i < segs.length - 1) {
        if (!node.classList.contains("is-dir")) return null;
        const box = await this.expandDir(node);
        if (!box) return null;
        container = box;
      }
    }
    return node;
  }

  private hideFileCtx() {
    hideMenu(this.ctx, "fctx");
  }

  /** Menu contextuel clic droit : nouveau fichier/dossier, éditer /
   *  copier contenu / chemin absolu / supprimer. Android : rendu en
   *  action-sheet ancré en bas. */
  private openFileCtx(x: number, y: number, el: HTMLElement) {
    const rel = el.dataset.rel ?? "";
    const abs = el.dataset.abs ?? rel;
    const isDir = el.classList.contains("is-dir");
    // Racine (en-tête cwd) ou fond de l'arbre : création seulement.
    const isRoot = el.classList.contains("froot") || el.id === "file-tree";
    this.ctx.innerHTML = abs ? `<div class="fctx-path">${esc(abs)}</div>` : "";
    menuBtn(this.ctx, t("ctx_new_file"), "", () => {
      this.hideFileCtx();
      void this.beginCreate(el, false);
    });
    menuBtn(this.ctx, t("ctx_new_folder"), "", () => {
      this.hideFileCtx();
      void this.beginCreate(el, true);
    });
    if (isRoot) {
      if (abs) {
        menuBtn(this.ctx, t("ctx_copy_path"), "", () => {
          this.hideFileCtx();
          void copyText(abs);
        });
      }
      placeMenu(this.ctx, "fctx", x, y);
      return;
    }
    if (!isDir) {
      menuBtn(this.ctx, t("ctx_edit"), "", () => {
        this.hideFileCtx();
        void this.openFilePanel(rel, { edit: true });
      });
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

  /** Entrée ctx « nouveau » : dossier cible = le dossier cliqué, le
   *  parent d'un fichier, ou la racine (en-tête cwd / fond de l'arbre).
   *  Déplie la cible puis ouvre la saisie inline du nom. */
  private async beginCreate(el: HTMLElement, dir: boolean) {
    if (!app.current) return;
    const rel = el.dataset.rel ?? "";
    const isDir =
      el.classList.contains("is-dir") ||
      el.classList.contains("froot") ||
      el.id === "file-tree";
    const parentRel = isDir
      ? rel
      : rel.includes("/")
        ? rel.slice(0, rel.lastIndexOf("/"))
        : "";
    this.clearSearch();
    let box: HTMLElement | null = $("file-tree");
    if (parentRel) {
      // Le dossier peut être encore replié (ctx depuis un résultat de
      // recherche) — dépliage lazy niveau par niveau.
      const node = await this.expandTo(parentRel);
      if (!node || !node.classList.contains("is-dir")) {
        showErr(t("tree_gone"));
        return;
      }
      box = await this.expandDir(node);
      if (!box || !box.isConnected) return;
    }
    // L'entrée va remplir la boîte — « (vide) »/erreur n'ont plus lieu
    // d'être. Enfants directs seulement : le .fempty d'un sous-dossier
    // déplié ne doit pas disparaître (restauré dans close() si annulé).
    box.querySelector(":scope > .fempty")?.remove();
    box.querySelector(":scope > .ferr")?.remove();
    this.inlineNameRow(box, parentRel, dir);
  }

  /** Ligne de saisie du nom dans `box` : Enter crée via create_path,
   *  Échap/perte de focus annule. Après création le nœud est inséré à
   *  sa place triée (pas de rechargement) et un fichier s'ouvre en
   *  édition dans le viewer. */
  private inlineNameRow(box: HTMLElement, parentRel: string, dir: boolean) {
    if (!app.current) return; // session fermée pendant le dépliage
    const depth = parentRel ? parentRel.split("/").length : 0;
    const row = document.createElement("div");
    row.className = "fnode fnew";
    row.style.paddingLeft = `${6 + depth * 14}px`;
    row.innerHTML =
      `<span class="farrow"></span>` +
      `<span class="fico">${icon(dir ? "folder" : "file")}</span>`;
    const inp = document.createElement("input");
    inp.className = "fnew-name";
    inp.placeholder = t(dir ? "new_folder_ph" : "new_file_ph");
    row.appendChild(inp);
    // En tête de boîte — sous l'en-tête .froot quand c'est la racine.
    const first = box.id === "file-tree"
      ? box.querySelector(".froot")?.nextElementSibling
      : box.firstElementChild;
    box.insertBefore(row, first ?? null);
    const sid = app.current!.id;
    const conn = app.current!.conn;
    let closed = false;
    // Appel RPC en vol : Chromium dispatche blur quand l'input focalisé
    // devient disabled (WebView2, Android) — sans ce garde le blur
    // fermerait la saisie en plein commit.
    let inflight = false;
    const close = () => {
      if (closed) return;
      closed = true;
      row.remove();
      // Restaure « (vide) » si la boîte reste sans entrée après annulation.
      if (box.isConnected && !box.querySelector(":scope > .fnode")) {
        box.insertAdjacentHTML("beforeend", `<div class="fempty">${t("tree_empty")}</div>`);
      }
    };
    const commit = async () => {
      if (closed || inflight) return;
      const name = inp.value.trim();
      if (!name) { close(); return; } // vide = annuler
      if (/[\\/]/.test(name) || name === "." || name === "..") {
        showErr(t("name_invalid"));
        inp.focus();
        return;
      }
      // Session changée pendant la saisie → annuler plutôt que créer
      // dans le mauvais workspace.
      if (app.current?.id !== sid) { close(); return; }
      const rel = parentRel ? `${parentRel}/${name}` : name;
      inflight = true;
      inp.disabled = true;
      try {
        await call("create_path", { session: sid, conn, path: rel, dir });
      } catch (e) {
        inflight = false;
        inp.disabled = false;
        showErr(String(e));
        inp.focus();
        return;
      }
      closed = true;
      row.remove();
      // Boîte remplacée ou session changée pendant l'appel : l'entrée
      // existe mais l'arbre visible n'est plus le bon — rien à insérer.
      if (app.current?.id !== sid || !box.isConnected) return;
      // Un push dir_changed a pu insérer l'entrée pendant le RPC :
      // réutiliser le nœud présent plutôt que la doubler.
      let node: HTMLElement | undefined;
      for (const n of box.querySelectorAll<HTMLElement>(":scope > .fnode")) {
        if (n.dataset.rel === rel) { node = n; break; }
      }
      if (!node) {
        const en: DirEntry = { name, dir, size: 0 };
        node = this.treeNode(rel, name, en, depth,
          this.treeRootDir ? `${this.treeRootDir}/${rel}` : rel);
        this.insertSorted(box, node);
      }
      for (const n of $("file-tree").querySelectorAll<HTMLElement>(".fnode.sel")) {
        n.classList.remove("sel");
      }
      node.classList.add("sel");
      node.scrollIntoView({ block: "nearest" });
      if (!dir) void this.openFilePanel(rel, { edit: true });
    };
    inp.addEventListener("keydown", (e) => {
      e.stopPropagation();
      if (e.key === "Enter") void commit();
      else if (e.key === "Escape") close();
    });
    inp.addEventListener("click", (e) => e.stopPropagation());
    // Perte de focus = annuler : pas de création surprise d'un nom à
    // moitié tapé. Pas pendant l'appel (blur synthétique du disabled).
    inp.addEventListener("blur", () => { if (!inflight) close(); });
    inp.focus();
    inp.scrollIntoView({ block: "nearest" });
  }

  /** Insère `node` dans `box` selon le tri du démon : dossiers d'abord,
   *  puis nom alpha insensible à la casse. */
  private insertSorted(box: HTMLElement, node: HTMLElement) {
    const dir = node.classList.contains("is-dir");
    const name = (node.dataset.rel?.split("/").pop() ?? "").toLowerCase();
    for (const ch of Array.from(box.children)) {
      const h = ch as HTMLElement;
      if (!h.classList.contains("fnode")) continue;
      const hDir = h.classList.contains("is-dir");
      if (!hDir && dir) { box.insertBefore(node, h); return; }
      if (hDir === dir) {
        const hName = (h.dataset.rel?.split("/").pop() ?? "").toLowerCase();
        if (hName > name) { box.insertBefore(node, h); return; }
      }
    }
    box.appendChild(node);
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
    if (isWeb) {
      // Navigateur : téléchargement HTTP natif via /download — le
      // navigateur gère dossier de destination, progression, reprise.
      const q = new URLSearchParams({
        session: sid, path: rel, token: wsClient.token() ?? "",
      });
      const a = document.createElement("a");
      a.href = `/download?${q}`;
      a.download = info.name;
      a.click();
      this.deps.sysline(t("dl_done_web", { name: info.name }), "check");
      return;
    }
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
    // Push FS du démon : rafraîchit le dossier déplié concerné (reconcile
    // sans clobber — un event par watch, debouncé côté démon).
    void listen<{ session?: string; path?: string }>("dir-changed", (m) =>
      this.onDirChanged(m.payload),
    );

    // État initial du select d'encodage (options i18n-free, fixes).
    const encSel = $("fv-enc") as HTMLSelectElement;
    encSel.innerHTML = ENCODINGS.map(
      (e) => `<option value="${e.id}">${e.label}</option>`,
    ).join("");
    encSel.value = defaultEncoding();
    encSel.onchange = () => {
      const cur = this.cur;
      if (!cur) return;
      const enc = encSel.value as Encoding;
      // Encodage changé → re-télécharge et re-décode depuis le disque
      // (dirty guardé : re-décoder écraserait les éditions en cours).
      void (async () => {
        if (!(await this.ensureClean())) {
          this.syncEncSelect();
          return;
        }
        await this.openFilePanel(cur.rel, { enc, edit: this.editOn });
      })();
    };

    this.mdView.init();
    this.pdf.init();
    $("fv-edit").onclick = () => {
      if (this.editOn) this.exitEdit();
      else this.enterEdit();
    };
    ($("fv-save") as HTMLButtonElement).onclick = () => void this.saveEdit();
    $("fv-loadall").onclick = () => void this.loadAll();
    $("fv-reload").onclick = () => void this.reloadFile();
    $("fv-overwrite").onclick = () => void this.saveEdit(true);

    const ta = $("file-edit") as HTMLTextAreaElement;
    ta.addEventListener("input", () => this.onEditInput());
    ta.addEventListener("scroll", () => this.syncEditScroll());
    ta.addEventListener("keydown", (e) => {
      if (e.key === "Tab") {
        e.preventDefault();
        ta.setRangeText("\t", ta.selectionStart, ta.selectionEnd, "end");
        this.onEditInput();
      } else if ((e.ctrlKey || e.metaKey) && (e.key === "s" || e.key === "S")) {
        e.preventDefault();
        void this.saveEdit();
      } else if (e.key === "Escape") {
        e.preventDefault();
        this.exitEdit();
      }
    });
    $("file-view").addEventListener("scroll", (e) => {
      const el = e.target as HTMLElement;
      if (el.scrollTop + el.clientHeight >= el.scrollHeight * 0.85) this.maybeMore();
    });

    // Dialogue dirty : rester / abandonner / enregistrer-puis-continuer.
    // Fermé par Esc/back sans bouton → « rester » (la promesse ne pend pas).
    ($("dlg-filedirty") as HTMLDialogElement).addEventListener("close", () => {
      if (this.cleanRes) this.resolveClean(false);
    });
    $("fd-stay").onclick = () => this.resolveClean(false);
    $("fd-discard").onclick = () => {
      this.dirty = false;
      this.resolveClean(true);
    };
    $("fd-save").onclick = () => {
      void this.saveEdit().then(() => {
        // Conflit → dirty toujours vrai : le bandeau prend le relais.
        this.resolveClean(!this.dirty);
      });
    };

    // Back Android : véto quand le buffer est sale — la fermeture ne doit
    // pas perdre les éditions sans confirmation.
    nav.backGuard = (id) => {
      if ((id === "fileview" || id === "files") && this.dirty) {
        void this.ensureClean().then((ok) => {
          if (!ok) return;
          this.exitEditNow();
          if (id === "fileview") {
            nav.close("fileview");
            $("file-viewer").classList.remove("open");
          } else {
            this.closeFilePanel();
          }
        });
        return false;
      }
      return true;
    };

    $("file-close").onclick = () => this.closeFilePanel();
    // Retour viewer → liste (Android) : en édition, sort d'abord du mode.
    $("fv-back").onclick = () => {
      if (this.editOn) {
        this.exitEdit();
        return;
      }
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
    $("btn-tree-refresh").onclick = () => {
      this.treeReady = this.loadTreeRoot();
    };

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
    // Clic droit sur le fond de l'arbre = création à la racine du
    // workspace. Le filtre n'accepte que le conteneur lui-même et les
    // lignes d'état : le touch des nœuds remonterait sinon en bubbling
    // et doublerait le menu (ils ont leur propre bindCtx).
    bindCtx($("file-tree"), (x, y) => this.openFileCtx(x, y, $("file-tree")),
      (e) => {
        const t = e.target as HTMLElement;
        // Enfants directs seulement : un .fempty dans un .fchildren
        // appartient au contexte du sous-dossier, pas à la racine.
        return t === $("file-tree") ||
          (t.parentElement === $("file-tree") &&
            (t.classList.contains("fempty") || t.classList.contains("ferr")));
      });
  }
}
