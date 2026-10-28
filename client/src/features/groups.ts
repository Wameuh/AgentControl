// ---------------------------------------------------------------------------
// Groupes nommés par connexion : persistés côté client (les sessions portent
// leur groupe dans meta.json ; le store conserve aussi les groupes VIDES).
// ---------------------------------------------------------------------------

import { $, showErr } from "../core/dom";
import { call } from "../core/rpc";
import { app } from "../core/state";
import type { SessionMeta } from "../core/types";
import { t } from "../i18n";

const GKEY = (c: string) => `ac-groups:${c}`;

export class GroupStore {
  /** Rafraîchit la sidebar après une mutation (injecté — évite la dépendance
   *  circulaire groups ↔ sidebar). */
  refresh: () => void = () => {};

  stored(conn: string): string[] {
    try { return JSON.parse(localStorage.getItem(GKEY(conn)) ?? "[]") as string[]; }
    catch { return []; }
  }
  private save(conn: string, grps: string[]) {
    localStorage.setItem(GKEY(conn), JSON.stringify([...new Set(grps)].sort()));
  }
  add(conn: string, name: string) {
    if (name) this.save(conn, [...this.stored(conn), name]);
  }
  remove(conn: string, name: string) {
    this.save(conn, this.stored(conn).filter((g) => g !== name));
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

  // Dialogue groupe : mode "session" (assigner), "rename" (renommer/dissoudre),
  // "new" (créer un groupe vide dans une connexion).
  private target: SessionMeta | null = null;
  private mode: "session" | "rename" | "new" = "session";
  private ctx = { conn: "", name: "" };

  openDlg(mode: "session" | "rename" | "new", s: SessionMeta | null, conn = "", name = "") {
    this.mode = mode;
    this.target = s;
    this.ctx = { conn, name };
    const dlg = $("dlg-group") as HTMLDialogElement;
    dlg.querySelector("h3")!.textContent =
      t(mode === "rename" ? "rename_group" : mode === "new" ? "new_group" : "group_h");
    ($("g-name") as HTMLInputElement).value = mode === "session" ? (s?.group ?? "") : name;
    ($("g-none") as HTMLButtonElement).hidden = mode === "new";
    ($("g-none") as HTMLButtonElement).textContent =
      t(mode === "rename" ? "dissolve_group" : "group_none");
    const dl = $("g-list");
    dl.innerHTML = "";
    call<SessionMeta[]>("list_sessions").then((ls) => {
      const conns = mode === "session" ? [s?.conn ?? ""] : [conn];
      const all = new Set(conns.flatMap((c) => this.stored(c)));
      for (const g of all) { const o = document.createElement("option"); o.value = g; dl.appendChild(o); }
      for (const g of new Set(ls.filter((x) => x.group).map((x) => x.group!))) {
        const o = document.createElement("option");
        o.value = g;
        dl.appendChild(o);
      }
    });
    dlg.showModal();
  }

  async applyAssign(s: SessionMeta, group?: string) {
    await call("set_session_group", { session: s.id, conn: s.conn, group }).catch((er) =>
      showErr(String(er)),
    );
    if (group) this.add(s.conn ?? "default", group);
  }

  /** Dissout un groupe : dégroupe toutes ses sessions puis oublie le nom
   *  stocké (utilisé par le dialogue rename ET le menu ctx du groupe). */
  async dissolve(conn: string, name: string) {
    const members = app.lastSessions.filter(
      (s) => (s.conn ?? "default") === conn && s.group === name,
    );
    for (const s of members) await this.applyAssign(s, undefined);
    this.remove(conn, name);
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
          await this.dissolve(this.ctx.conn, this.ctx.name);
        } else if (name !== this.ctx.name) {
          const members = app.lastSessions.filter(
            (s) => (s.conn ?? "default") === this.ctx.conn && s.group === this.ctx.name,
          );
          for (const s of members) await this.applyAssign(s, name);
          this.remove(this.ctx.conn, this.ctx.name);
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
