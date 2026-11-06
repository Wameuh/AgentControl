// ---------------------------------------------------------------------------
// Groupes nommés par connexion : persistés côté client (les sessions portent
// leur groupe dans meta.json ; le store conserve aussi les groupes VIDES).
// Un groupe est un CHEMIN "a/b/c" : les sous-groupes vivent dans le nom —
// le démon n'a pas besoin de connaître la hiérarchie (set_session_group
// prend la chaîne telle quelle). Les anciens groupes sans "/" restent des
// groupes racine : rétrocompatible.
// ---------------------------------------------------------------------------

import { $, showErr } from "../core/dom";
import { call } from "../core/rpc";
import { app } from "../core/state";
import type { SessionMeta } from "../core/types";
import { t } from "../i18n";

const GKEY = (c: string) => `ac-groups:${c}`;

/** Profondeur max d'un chemin — garde-fou contre une saisie/libellé de
 *  méta pathologique (rendu récursif + poids visuel). Au-delà, les
 *  segments excédentaires sont tronqués à la normalisation. */
const GRP_MAX_DEPTH = 8;

/** Normalise un chemin de groupe : trim les segments, supprime les segments
 *  vides ("a//b" → "a/b", "/a/" → "a"), borne la profondeur. "" = pas de
 *  groupe. */
export const grpNorm = (p: string): string =>
  p.split("/").map((s) => s.trim()).filter(Boolean).slice(0, GRP_MAX_DEPTH).join("/");

/** Dernier segment du chemin ("a/b" → "b"). */
export const grpLeaf = (p: string): string => p.slice(p.lastIndexOf("/") + 1);

/** Chemin parent ("a/b" → "a", "a" → ""). */
export const grpParent = (p: string): string =>
  p.includes("/") ? p.slice(0, p.lastIndexOf("/")) : "";

/** Tous les préfixes ancêtres, racine d'abord ("a/b/c" → ["a", "a/b"]). */
export const grpAncestors = (p: string): string[] => {
  const out: string[] = [];
  let acc = "";
  for (const seg of p.split("/")) {
    acc = acc ? `${acc}/${seg}` : seg;
    out.push(acc);
  }
  return out.slice(0, -1);
};

/** p est-il dans le sous-arbre de root (root inclus) ? */
export const inSubtree = (p: string, root: string): boolean =>
  p === root || p.startsWith(root + "/");

export class GroupStore {
  /** Rafraîchit la sidebar après une mutation (injecté — évite la dépendance
   *  circulaire groups ↔ sidebar). */
  refresh: () => void = () => {};

  stored(conn: string): string[] {
    try {
      return (JSON.parse(localStorage.getItem(GKEY(conn)) ?? "[]") as string[])
        .map(grpNorm).filter(Boolean);
    } catch { return []; }
  }
  private save(conn: string, grps: string[]) {
    localStorage.setItem(GKEY(conn), JSON.stringify([...new Set(grps)].sort()));
  }
  /** Enregistre un chemin ET ses ancêtres : un parent explicite survit à la
   *  disparition de ses enfants (même règle que les groupes vides gardés). */
  add(conn: string, name: string) {
    const p = grpNorm(name);
    if (p) this.save(conn, [...this.stored(conn), ...grpAncestors(p), p]);
  }

  /** Purge les groupes nommés + états de collapse persistés d'une connexion
   *  supprimée (appelé par ConnectionManager.delete). */
  clearConn(conn: string) {
    localStorage.removeItem(GKEY(conn));
    localStorage.removeItem(`ac-grp-${conn}`);
    for (const k of Object.keys(localStorage)) {
      if (k.startsWith(`ac-grp-${conn}::`)) localStorage.removeItem(k);
    }
  }

  /** Réécrit le préfixe de tous les chemins stockés sous `from` (inclus) vers
   *  `to` — sous-arbres des groupes vides compris. Les clés de collapse
   *  suivent le chemin complet : elles sont migrées au passage (sinon un
   *  header renommé hériterait de l'état plié d'un groupe sans rapport). */
  private remapStored(conn: string, from: string, to: string | undefined) {
    from = grpNorm(from); // garde-fou : appelants futurs au chemin brut
    const next = this.stored(conn).flatMap((g) => {
      if (!inSubtree(g, from)) return [g];
      return to === undefined ? [] : [to + g.slice(from.length)];
    });
    this.save(conn, next);
    for (const k of Object.keys(localStorage)) {
      const pre = `ac-grp-${conn}::${from}`;
      if (k === pre || k.startsWith(pre + "/")) {
        const v = localStorage.getItem(k)!;
        localStorage.removeItem(k);
        if (to !== undefined) {
          localStorage.setItem(`ac-grp-${conn}::${to}${k.slice(pre.length)}`, v);
        }
      }
    }
  }

  // Dialogue groupe : mode "session" (assigner), "rename" (renommer/dissoudre),
  // "new" (créer un groupe — parent renseigné = sous-groupe de ce chemin).
  private target: SessionMeta | null = null;
  private mode: "session" | "rename" | "new" = "session";
  private ctx = { conn: "", name: "", parent: "" };

  openDlg(mode: "session" | "rename" | "new", s: SessionMeta | null, conn = "", name = "", parent = "") {
    this.mode = mode;
    this.target = s;
    this.ctx = { conn, name, parent };
    const dlg = $("dlg-group") as HTMLDialogElement;
    dlg.querySelector("h3")!.textContent =
      t(mode === "rename" ? "rename_group" : mode === "new" ? (parent ? "new_subgroup" : "new_group") : "group_h");
    // En mode "new" avec parent : l'input est prérempli du préfixe parent/
    // — l'utilisateur n'a qu'à taper le nom du sous-groupe. En rename, le
    // chemin complet est éditable (renomme la feuille OU déplace : "a/b" →
    // "x" remonte le groupe à la racine, descendants compris).
    ($("g-name") as HTMLInputElement).value =
      mode === "session" ? (s?.group ?? "") : mode === "new" ? (parent ? parent + "/" : "") : name;
    ($("g-none") as HTMLButtonElement).hidden = mode === "new";
    ($("g-none") as HTMLButtonElement).textContent =
      t(mode === "rename" ? "dissolve_group" : "group_none");
    const dl = $("g-list");
    dl.innerHTML = "";
    call<SessionMeta[]>("list_sessions").then((ls) => {
      const conns = mode === "session" ? [s?.conn ?? ""] : [conn];
      const all = new Set(conns.flatMap((c) => this.stored(c)));
      // Les chemins issus des sessions contribuent leurs ancêtres aussi :
      // proposer "a" quand seule "a/b" existe évite de recréer le parent.
      for (const g of ls.filter((x) => x.group).map((x) => x.group!)) {
        const ng = grpNorm(g); // méta mal formée ("a//b") → pas de suggestion fantôme
        if (!ng) continue;
        all.add(ng);
        for (const a of grpAncestors(ng)) all.add(a);
      }
      for (const g of all) { const o = document.createElement("option"); o.value = g; dl.appendChild(o); }
    }).catch(() => {}); // suggestions en rab — fusion multi-conns, pas de toast
    dlg.showModal();
  }

  async applyAssign(s: SessionMeta, group?: string) {
    const g = group ? grpNorm(group) : undefined;
    try {
      await call("set_session_group", { session: s.id, conn: s.conn, group: g });
      // Local à jour tout de suite : les opérations chaînées sur un
      // sous-arbre (rename/dissolve bouclent) relisent s.group avant
      // que refresh() n'ait resynchronisé la méta démon.
      s.group = g;
      // Store aligné sur la réalité démon : en cas d'échec RPC, pas de
      // groupe fantôme persisté (ni ses ancêtres explicites).
      if (g) this.add(s.conn ?? "default", g);
    } catch (er) {
      showErr(String(er));
    }
  }

  /** Membres du sous-arbre : sessions dont le groupe est `path` ou un de
   *  ses descendants (le collapse/dissolve/déplacement agissent sur tout
   *  le sous-arbre — la sémantique choisie, documentée ici). */
  private subtreeMembers(conn: string, path: string): SessionMeta[] {
    return app.lastSessions.filter(
      (s) => (s.conn ?? "default") === conn && s.group && inSubtree(grpNorm(s.group), path),
    );
  }
  subtreeSize(conn: string, path: string): number {
    return this.subtreeMembers(conn, path).length;
  }

  /** Dissout un sous-arbre entier : toutes les sessions sous `path` (le
   *  nœud inclus) sont dégroupées, les chemins stockés du sous-arbre et
   *  leurs états de collapse sont oubliés (menu ctx ET dialogue rename). */
  async dissolve(conn: string, path: string) {
    const p = grpNorm(path);
    for (const s of this.subtreeMembers(conn, p)) await this.applyAssign(s, undefined);
    this.remapStored(conn, p, undefined);
  }

  /** Renomme/déplace un sous-arbre : `to` est le nouveau CHEMIN COMPLET
   *  (rename "a/b"→"a/c" change la feuille ; "a/b"→"x" la remonte à la
   *  racine). Préfixe remplacé sur les sessions, les chemins stockés et
   *  les clés de collapse. Fusion autorisée : si `to` existe déjà, les
   *  deux sous-arbres cohabitent sous le même chemin. */
  async rename(conn: string, from: string, to: string): Promise<boolean> {
    const src = grpNorm(from); // les appelants passent le chemin tel quel
    const dst = grpNorm(to);
    if (!src) return false;
    if (!dst) { await this.dissolve(conn, src); return true; }
    if (dst === src) return true;
    if (inSubtree(dst, src)) return false; // un groupe ne peut pas entrer dans son propre sous-arbre
    for (const s of this.subtreeMembers(conn, src)) {
      // Suffixe lu sur le chemin NORMALISÉ (le brut peut être mal formé).
      await this.applyAssign(s, dst + grpNorm(s.group!).slice(src.length));
    }
    this.remapStored(conn, src, dst);
    this.add(conn, dst);
    return true;
  }

  /** Déplace un sous-arbre sous un nouveau parent ("" = racine) — DnD. */
  async move(conn: string, src: string, dstParent: string): Promise<boolean> {
    const s = grpNorm(src), dp = grpNorm(dstParent);
    if (grpParent(s) === dp) return true; // déjà à cet endroit
    if (dp && inSubtree(dp, s)) return false; // pas dans son propre sous-arbre
    return this.rename(conn, s, dp ? `${dp}/${grpLeaf(s)}` : grpLeaf(s));
  }

  init() {
    ($("dlg-group") as HTMLDialogElement).addEventListener("close", async (e) => {
      const rv = (e.target as HTMLDialogElement).returnValue;
      if (rv === "cancel") return;
      const name = ($("g-name") as HTMLInputElement).value.trim();
      if (this.mode === "session" && this.target) {
        await this.applyAssign(this.target, rv === "none" ? undefined : name || undefined);
      } else if (this.mode === "rename" && this.ctx.name) {
        if (rv === "none" || !name) {
          // Champ vidé (ou bouton Dissoudre) = dissolution du SOUS-ARBRE
          // entier — sémantique « vide = dissoudre » héritée du rendu
          // plat, étendue aux descendants : assumée (le dialogue
          // affiche le chemin complet, la cible est sans ambiguïté).
          await this.dissolve(this.ctx.conn, this.ctx.name);
        } else if (!(await this.rename(this.ctx.conn, this.ctx.name, name))) {
          showErr(t("group_path_invalid"));
        }
      } else if (this.mode === "new" && name) {
        this.add(this.ctx.conn, name);
      }
      this.refresh();
    });
    ($("g-ok") as HTMLButtonElement).onclick = () => {
      ($("dlg-group") as HTMLDialogElement).close("ok");
    };
    // type=button → fermeture explicite ; Enter active g-ok (seul submit restant).
    $("g-none").onclick = () => ($("dlg-group") as HTMLDialogElement).close("none");
    $("g-cancel").onclick = () => ($("dlg-group") as HTMLDialogElement).close("cancel");
  }
}
