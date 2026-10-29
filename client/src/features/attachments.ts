// ---------------------------------------------------------------------------
// Pièces jointes du composer : bouton « + » (file picker natif, Android
// compris), glisser-déposer sur la zone chat/composer, collage depuis le
// presse-papier. Chaque fichier part en tranches dans
// <cwd>/.ac-uploads/<ts>-<nom> via les RPC fichiers existants
// (create_path exclusif + write_file par offset — pas de nouveau RPC) ;
// le message envoyé à l'agent liste les chemins absolus, lisibles avec
// ses outils habituels. Rétention : le dossier vit dans le workspace de
// la session — visible dans l'explorateur, purgé avec lui.
// ---------------------------------------------------------------------------

import { $, esc, notify, showErr } from "../core/dom";
import { icon, qIco } from "../ui/icons";
import { call } from "../core/rpc";
import { app } from "../core/state";
import { isAndroid } from "../core/chrome";
import { fmtSize } from "../core/format";
import { t } from "../i18n";

/// Taille max d'une pièce jointe — au-delà : refus + toast.
const MAX_BYTES = 256 * 1024 * 1024;
/// Tranche d'upload (octets bruts) — base64 ×1.34 bien sous le plafond
/// write_file (~16 Mio de JSON par appel).
const CHUNK = 4 * 1024 * 1024;
/// Dossier d'upload dans le workspace (borné cwd côté démon — l'agent
/// lit les pièces avec ses outils fichiers habituels).
const DIR = ".ac-uploads";

type Status = "up" | "ok" | "err";
/** Sortie volontaire de la boucle d'upload (retrait chip / changement
 *  de session) — distingue l'annulation d'un vrai échec RPC. */
const ABORT = Symbol("att-abort");

interface Att {
  /** Nom affiché (sanitizé : basename + caractères sûrs). */
  name: string;
  size: number;
  /** Session propriétaire — les RPC (upload comme delete) la visent
   *  même si l'affichage a changé entre-temps. */
  sid: string;
  /** Conn du chip — épinglée à l'attach (un switch de conn en vol ne
   *  doit pas dérouter les tranches vers un autre démon). */
  conn: string | undefined;
  /** Chemin relatif au cwd cible (`.ac-uploads/…`) — fixé au create. */
  rel: string;
  /** Chemin absolu résolu renvoyé par create_path — va au message. */
  abs: string;
  status: Status;
  /** Octets uploadés (progression du chip). */
  sent: number;
  /** Retrait demandé : la boucle d'upload s'arrête au prochain tour. */
  aborted: boolean;
  el: HTMLElement;
}

export class Attachments {
  /** Capacité « fichiers » du démon de la conn (injecté — démons sans
   *  write_file/create_path → bouton + masqué, drop averti). */
  capable: (conn: string | undefined) => boolean = () => true;
  private items: Att[] = [];
  /** Session propriétaire des chips affichés (les chemins visent son cwd). */
  private sid: string | null = null;
  /** Chips garés par session non affichée — repris à son retour, même
   *  scoping que le brouillon du composer. Les uploads en vol inclus
   *  continuent : chaque Att épingle sid+conn, la boucle n'a pas besoin
   *  de l'affichage pour finir. */
  private parked = new Map<string, Att[]>();
  /** Compteur dragenter/dragleave — évite le clignotement en traversant
   *  les enfants de #chat. */
  private dragDepth = 0;

  // ---- API consommée par le composer ----

  /** Uploads encore en vol — sendInput refuse tant que des chemins
   *  manquent (sinon le message partirait sans ses pièces). */
  pending(): boolean {
    return this.items.some((a) => a.status === "up");
  }

  /** Pièces prêtes : {name, path absolu} pour le message à l'agent. */
  ready(): { name: string; path: string }[] {
    return this.items
      .filter((a) => a.status === "ok" && a.abs)
      .map((a) => ({ name: a.name, path: a.abs }));
  }

  /** Envoi accepté : les chips de la session qui a envoyé partent (les
   *  fichiers RESTENT — l'agent les lit pendant le tour). Elle peut
   *  ne plus être affichée si l'utilisateur a basculé pendant l'envoi :
   *  ses chips sont alors garés, pas rendus — on jette sa liste. */
  sent(sid: string) {
    if (sid === this.sid) {
      this.items = [];
      this.render();
    } else {
      this.parked.delete(sid);
    }
  }

  /** La session affichée a changé : ses chips sont garés sous son id
   *  (uploads en vol poursuivis), ceux de la session affichée reviennent. */
  onSessionChanged() {
    this.syncSession();
  }

  /** Affiche ou non les contrôles (session ouverte + démon capable). */
  syncVisible() {
    const on = !!app.current && this.capable(app.current.conn);
    ($("btn-attach") as HTMLButtonElement).hidden = !on;
  }

  init() {
    this.sid = app.current?.id ?? null;
    const pick = $("attach-pick") as HTMLInputElement;
    $("btn-attach").onclick = () => pick.click();
    pick.addEventListener("change", () => {
      if (pick.files?.length) this.addFiles([...pick.files]);
      pick.value = ""; // re-choisir le même fichier doit re-firer
    });
    // Collage (image copiée, fichier depuis un gestionnaire) → upload.
    $("input").addEventListener("paste", (e) => {
      const files = e.clipboardData?.files;
      if (files && files.length) {
        e.preventDefault();
        this.addFiles([...files]);
      }
    });
    // Glisser-déposer : pas de DnD fichier sur Android (bouton seul).
    if (isAndroid()) return;
    const chat = $("chat");
    const isFiles = (e: DragEvent) =>
      e.dataTransfer?.types.includes("Files") ?? false;
    chat.addEventListener("dragenter", (e) => {
      if (!isFiles(e)) return;
      e.preventDefault();
      this.dragDepth++;
      chat.classList.add("att-drop");
      $("drop-hint").hidden = false;
    });
    chat.addEventListener("dragover", (e) => {
      if (!isFiles(e)) return;
      e.preventDefault(); // requis pour autoriser le drop
      if (e.dataTransfer) e.dataTransfer.dropEffect = "copy";
    });
    chat.addEventListener("dragleave", (e) => {
      if (!isFiles(e)) return;
      if (--this.dragDepth <= 0) this.endDrag();
    });
    chat.addEventListener("drop", (e) => {
      if (!isFiles(e)) return;
      e.preventDefault();
      this.endDrag();
      const files = e.dataTransfer?.files;
      if (files?.length) this.addFiles([...files]);
    });
  }

  private endDrag() {
    this.dragDepth = 0;
    $("chat").classList.remove("att-drop");
    $("drop-hint").hidden = true;
  }

  /** Les chips appartiennent à la session qui les a reçues : changement
   *  de session → les siens sont garés (un upload en vol continue — ses
   *  tranches visent toujours la bonne session), ceux de la session
   *  affichée sont réinstallés. */
  private syncSession() {
    const cur = app.current?.id ?? null;
    if (cur === this.sid) return;
    if (this.sid && this.items.length) this.parked.set(this.sid, this.items);
    this.items = (cur && this.parked.get(cur)) || [];
    if (cur) this.parked.delete(cur);
    this.sid = cur;
    this.render();
  }

  /** Session supprimée : ses chips garés partent avec elle — upload en
   *  vol abandonné (sa boucle nettoiera son reliquat), pièce déjà posée
   *  retirée au mieux pour ne pas orpheliner .ac-uploads. */
  dropSession(id: string) {
    const items = this.parked.get(id);
    if (!items) return;
    this.parked.delete(id);
    for (const a of items) {
      a.aborted = true;
      if (a.status !== "up") this.removeRemote(a);
    }
  }

  addFiles(files: File[]) {
    this.syncSession();
    if (!this.sid) {
      notify("warn", t("att_no_session"));
      return;
    }
    if (!this.capable(app.current?.conn)) {
      notify("warn", t("att_unsupported"));
      return;
    }
    for (const f of files) {
      if (f.size > MAX_BYTES) {
        notify("warn", t("att_too_big", { name: f.name, max: fmtSize(MAX_BYTES) }));
        continue;
      }
      void this.attachOne(f);
    }
  }

  // ---- Pipeline d'un fichier : chip → create exclusif → chunks ----

  private async attachOne(file: File) {
    const a: Att = {
      name: sanitizeName(file.name),
      size: file.size,
      sid: this.sid!,
      conn: app.current?.conn,
      rel: "",
      abs: "",
      status: "up",
      sent: 0,
      aborted: false,
      el: document.createElement("div"),
    };
    this.items.push(a);
    this.render();
    const conn = a.conn;
    try {
      // Le dossier d'uploads : create exclusif → « existe déjà » =
      // présent, toute autre erreur est fatale à l'upload.
      await call("create_path", {
        session: a.sid,
        path: DIR,
        dir: true,
        conn,
      }).catch((e) => {
        if (!/existe déjà/i.test(String(e))) throw e;
      });
      // Nom unique : .ac-uploads/<ts>-<nom> ; collision → suffixe -2,
      // -3… avant l'extension (create_path est O_EXCL — jamais
      // d'écrasement silencieux).
      const stamp = Date.now().toString(36);
      for (let n = 0; ; n++) {
        const rel = `${DIR}/${stamp}-${suffixName(a.name, n)}`;
        try {
          const r = await call<{ path: string }>("create_path", {
            session: a.sid,
            path: rel,
            conn,
          });
          a.rel = rel;
          a.abs = r.path;
          break;
        } catch (e) {
          if (!/existe déjà/i.test(String(e))) throw e;
          if (n >= 99) throw e;
        }
      }
      while (a.sent < file.size) {
        if (a.aborted) throw ABORT;
        const buf = await file.slice(a.sent, a.sent + CHUNK).arrayBuffer();
        await call("write_file", {
          session: a.sid,
          path: a.rel,
          conn,
          offset: a.sent,
          delete_len: 0,
          data: b64(buf),
        });
        a.sent += buf.byteLength;
        this.paintChip(a);
      }
      a.status = "ok";
    } catch (e) {
      if (e === ABORT || a.aborted) {
        // Retrait utilisateur ou changement de session : nettoyer le
        // reliquat, pas de toast (action voulue).
        this.removeRemote(a);
        this.dropChip(a);
        return;
      }
      a.status = "err";
      showErr(t("att_fail", { name: a.name }) + " — " + String(e));
      this.removeRemote(a); // reliquat partiel : inutilisable par l'agent
    }
    this.paintChip(a);
  }

  /** Suppression distante best-effort (reliquat ou pièce retirée) — un
   *  échec (session morte, droits) n'est pas remonté. Vise la session
   *  du chip, pas l'affichée (elle a pu changer entre-temps). */
  private removeRemote(a: Att) {
    if (!a.rel) return;
    void call("delete_path", {
      session: a.sid,
      path: a.rel,
      conn: a.conn,
    }).catch(() => {});
  }

  private dropChip(a: Att) {
    let i = this.items.indexOf(a);
    if (i >= 0) {
      this.items.splice(i, 1);
    } else {
      // Chip garé (session non affichée — abort ou erreur en vol) :
      // le retirer de la liste de sa session.
      for (const items of this.parked.values()) {
        i = items.indexOf(a);
        if (i >= 0) {
          items.splice(i, 1);
          break;
        }
      }
    }
    this.render();
  }

  /** ✕ du chip : annule l'upload en vol ou retire la pièce prête (le
   *  fichier déjà posé est supprimé — rien d'orphelin dans .ac-uploads). */
  private removeChip(a: Att) {
    a.aborted = true;
    if (a.status === "up") return; // la boucle le verra et nettoiera
    if (a.status === "ok") this.removeRemote(a);
    this.dropChip(a);
  }

  // ---- Rendu : barre de chips (nom, taille, progression, ✕) ----

  private render() {
    const bar = $("attach-bar");
    // Rebuild complet : les chips des items retirés ne doivent pas
    // rester dans le DOM (dropChip, sent, changement de session).
    bar.innerHTML = "";
    for (const a of this.items) {
      bar.appendChild(a.el);
      this.paintChip(a);
    }
    bar.hidden = !this.items.length;
  }

  private paintChip(a: Att) {
    const pct = a.size ? Math.round((a.sent / a.size) * 100) : 0;
    a.el.className = `att-chip att-${a.status}`;
    a.el.innerHTML =
      icon("file") +
      `<span class="n">${esc(a.name)}</span>` +
      `<span class="s">${
        a.status === "up" ? `${fmtSize(a.sent)} / ${fmtSize(a.size)}`
        : a.status === "err" ? t("att_err")
        : fmtSize(a.size)
      }</span>` +
      (a.status === "up" ? `<i class="pg" style="width:${pct}%"></i>` : "");
    const x = qIco("close", t("att_remove"));
    x.className = "x";
    x.onclick = () => this.removeChip(a);
    a.el.appendChild(x);
  }
}

/// Nom sûr pour le workspace : basename (aucun `../`) + caractères
/// réduits aux usuels ; vide → « fichier ».
function sanitizeName(name: string): string {
  const base = name.split(/[\\/]/).pop() ?? "";
  const safe = base.replace(/[^\p{L}\p{N} ._+()-]/gu, "_").trim();
  return safe && safe !== "." && safe !== ".." ? safe : "fichier";
}

/// Collision : `rapport.pdf` → `rapport-2.pdf`, `rapport-3.pdf`…
/// (n=0 = nom tel quel).
function suffixName(name: string, n: number): string {
  if (!n) return name;
  const dot = name.lastIndexOf(".");
  if (dot <= 0) return `${name}-${n + 1}`;
  return `${name.slice(0, dot)}-${n + 1}${name.slice(dot)}`;
}

/// Base64 d'un ArrayBuffer (String.fromCharCode borné par sous-tableaux
/// — le spread direct dépasserait la pile d'arguments sur 4 Mio).
function b64(buf: ArrayBuffer): string {
  const u8 = new Uint8Array(buf);
  let s = "";
  for (let i = 0; i < u8.length; i += 0x8000) {
    s += String.fromCharCode(...u8.subarray(i, i + 0x8000));
  }
  return btoa(s);
}
