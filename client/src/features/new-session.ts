// ---------------------------------------------------------------------------
// Dialogue "nouvelle session" (ou attache d'une session native) : choix du
// serveur, de l'agent, workspace, modèle, assignation de groupe.
// ---------------------------------------------------------------------------

import { $, dlgErr, showErr } from "../core/dom";
import { call } from "../core/rpc";
import { app } from "../core/state";
import type { AgentOpt, SessionMeta } from "../core/types";
import { t } from "../i18n";
import type { GroupStore } from "./groups";

export class NewSessionDialog {
  /** Rafraîchit la sidebar après création (injecté). */
  refresh: () => void = () => {};
  /** Ouvre la session fraîchement créée (injecté) — l'utilisateur voit le
   *  spawn se dérouler dans le chat (events « agent started », erreurs). */
  openSession: (id: string, conn: string) => void = () => {};
  /** Connexions connues (clés) pour le select serveur (injecté). */
  knownConns: () => string[] = () => [];
  private groups!: GroupStore;

  bind(groups: GroupStore) {
    this.groups = groups;
  }

  // Groupe cible d'une création lancée par le "+" d'un groupe — la session
  // créée y est assignée automatiquement. conn = clé de routage (undefined =
  // connexion par défaut).
  private pendingGroup: { conn?: string; name: string } | null = null;
  setPendingGroup(g: { conn?: string; name: string } | null) {
    this.pendingGroup = g;
  }

  /** Agents disponibles sur une connexion (`conn` vide = défaut côté Rust). */
  private async listAgents(conn: string) {
    return call<AgentOpt[]>("list_agents", conn ? { conn } : {});
  }

  /** Placeholder unique dans le select agent (chargement / erreur). */
  private agentPlaceholder(msg: string) {
    const sel = $("f-agent") as HTMLSelectElement;
    sel.innerHTML = "";
    const o = document.createElement("option");
    o.textContent = msg;
    o.disabled = true;
    sel.appendChild(o);
  }

  private renderAgents(agents: AgentOpt[], keep?: string) {
    const sel = $("f-agent") as HTMLSelectElement;
    sel.innerHTML = "";
    for (const a of agents) {
      const o = document.createElement("option");
      o.value = a.id;
      o.textContent = a.name + (a.available === false ? ` (${t("agent_missing")})` : "");
      o.disabled = a.available === false;
      sel.appendChild(o);
    }
    if (!agents.length) this.agentPlaceholder(t("agents_failed"));
    if (keep && [...sel.options].some((o) => o.value === keep)) sel.value = keep;
  }

  /** Remplit le select : placeholder "chargement…" pendant l'appel ;
   *  erreur → zone interne du dialogue (un toast passerait sous le modal). */
  private async fillAgents(conn: string, keep?: string) {
    this.agentPlaceholder(t("agents_loading"));
    try {
      this.renderAgents(await this.listAgents(conn), keep);
    } catch (e) {
      this.agentPlaceholder(`⚠ ${t("agents_failed")}`);
      dlgErr("f-err", String(e));
    }
  }

  /** Dialogue "nouvelle session". `conn` route list_agents/new_session vers le
   *  bon serveur (groupe "+" → imposé, select désactivé) ; `ref` préremplit
   *  agent + workspace (1er membre du groupe). */
  async open(conn?: string, ref?: SessionMeta) {
    // Serveur cible : connexions actives ; présélection = conn du contexte,
    // sinon celle de la session affichée, sinon la première. Masqué si < 2.
    const cs = $("f-conn") as HTMLSelectElement;
    const conns = this.knownConns();
    cs.innerHTML = "";
    for (const k of conns) {
      const o = document.createElement("option");
      o.value = k;
      o.textContent = k;
      cs.appendChild(o);
    }
    const want = conn ?? app.current?.conn ?? conns[0] ?? "";
    cs.value = conns.includes(want) ? want : (conns[0] ?? "");
    cs.disabled = conn !== undefined;
    ($("f-conn-wrap") as HTMLElement).hidden = conns.length < 2;
    dlgErr("f-err", null); // chaque ouverture repart d'une zone propre
    const effConn = cs.disabled ? (conn ?? "") : cs.value;
    await this.fillAgents(effConn, ref?.agent);
    if (ref) ($("f-cwd") as HTMLInputElement).value = ref.cwd;
    // Suggestions de modèles : union des config_options connues des sessions
    const sess = await call<SessionMeta[]>("list_sessions").catch(() => [] as SessionMeta[]);
    const seen = new Set<string>();
    const dl = $("model-list");
    dl.innerHTML = "";
    for (const s of sess) {
      for (const co of s.config_options ?? []) {
        if (co.id !== "model" && co.category !== "model") continue;
        for (const o of co.options) {
          if (seen.has(o.value)) continue;
          seen.add(o.value);
          const el = document.createElement("option");
          el.value = o.value;
          el.label = o.name || o.value;
          dl.appendChild(el);
        }
      }
    }
    ($("dlg-new") as HTMLDialogElement).showModal();
  }

  // Changement de serveur cible → recharger les agents de ce serveur. Token de
  // séquence : un changement plus récent invalide la réponse encore en vol.
  private agentsReq = 0;

  init() {
    $("f-conn").addEventListener("change", async () => {
      const req = ++this.agentsReq;
      const conn = ($("f-conn") as HTMLSelectElement).value;
      this.agentPlaceholder(t("agents_loading"));
      try {
        const agents = await this.listAgents(conn);
        if (req === this.agentsReq) this.renderAgents(agents);
      } catch (e) {
        if (req !== this.agentsReq) return;
        this.agentPlaceholder(`⚠ ${t("agents_failed")}`);
        dlgErr("f-err", String(e));
      }
    });

    $("btn-new-session").onclick = () => {
      this.pendingGroup = null;
      void this.open();
    };

    // type=button → Enter dans le form valide via f-create (seul submit restant).
    $("f-cancel").onclick = () => ($("dlg-new") as HTMLDialogElement).close("cancel");

    $("f-create").addEventListener("click", async (e) => {
      e.preventDefault();
      const cwd = ($("f-cwd") as HTMLInputElement).value.trim();
      if (!cwd) return;
      const btn = e.currentTarget as HTMLButtonElement;
      const attach = ($("f-attach") as HTMLInputElement).value.trim();
      const grp = this.pendingGroup;
      // Serveur cible : le select (désactivé = conn imposée par le groupe).
      const selConn = ($("f-conn") as HTMLSelectElement).value || undefined;
      // Le spawn peut durer (agent npm, premier prompt) : bouton occupé +
      // erreur visible DANS le dialogue (un toast passerait sous le modal).
      btn.disabled = true;
      btn.classList.add("spin");
      const label0 = btn.textContent;
      btn.textContent = t("creating_agent");
      dlgErr("f-err", null);
      // L'appel peut durer sur un démon distant (spawn/attach synchrone,
      // timeout RPC 60 s) — l'utilisateur doit savoir qu'Échap détache
      // plutôt que croire à un gel.
      const fst = $("f-status");
      fst.textContent = t("create_wait");
      fst.hidden = false;
      const dlg = $("dlg-new") as HTMLDialogElement;
      const res = await (attach
        ? call<{ session: string }>("attach_session", {
            agent: ($("f-agent") as HTMLSelectElement).value,
            cwd,
            conn: grp?.conn ?? selConn,
            native_id: attach,
            title: ($("f-title") as HTMLInputElement).value || cwd.split("/").pop(),
          })
        : call<{ session: string }>("new_session", {
            agent: ($("f-agent") as HTMLSelectElement).value,
            cwd,
            conn: grp?.conn ?? selConn,
            title: ($("f-title") as HTMLInputElement).value || cwd.split("/").pop(),
            mode: ($("f-mode") as HTMLInputElement).value || undefined,
            model: ($("f-model") as HTMLInputElement).value || undefined,
            prompt: ($("f-prompt") as HTMLTextAreaElement).value || undefined,
          })
      ).catch((er) => {
        // Dialogue détaché (Échap) pendant l'appel : une erreur dans
        // f-err serait invisible — basculer en toast.
        if (dlg.open) dlgErr("f-err", String(er));
        else showErr(String(er));
        return null;
      });
      fst.hidden = true;
      btn.disabled = false;
      btn.classList.remove("spin");
      btn.textContent = label0;
      if (!res) return;
      if (grp) {
        // conn "" = connexion par défaut côté routage — la même que celle qui
        // vient de créer la session (évite l'injection de current.conn).
        const gconn = grp.conn ?? selConn ?? "";
        await call("set_session_group", {
          session: res.session,
          conn: gconn,
          group: grp.name,
        }).catch((er) => showErr(String(er)));
        this.groups.add(gconn || "default", grp.name);
        this.pendingGroup = null;
      }
      // Le démon a répondu dès l'insert : le spawn tourne en tâche de fond.
      // Suivi pour le toast d'échec + ouverture directe — le chat montre le
      // boot en direct (« agent started », erreur éventuelle) et un envoi
      // pendant le spawn s'enfile côté démon.
      app.spawnWatch.set(res.session, Date.now());
      ($("dlg-new") as HTMLDialogElement).close();
      await this.refresh();
      this.openSession(res.session, grp?.conn ?? selConn ?? "");
    });
  }
}
