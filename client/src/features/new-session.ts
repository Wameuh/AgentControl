// ---------------------------------------------------------------------------
// Dialogue "nouvelle session" (ou attache d'une session native) : choix du
// serveur, de l'agent, workspace, modèle, assignation de groupe.
// ---------------------------------------------------------------------------

import { $, dlgErr, notify, showErr } from "../core/dom";
import { call } from "../core/rpc";
import { app } from "../core/state";
import type { AgentCaps, AgentOpt, SessionMeta } from "../core/types";
import { t } from "../i18n";
import { costFor, ensureCosts } from "./modelcosts";
import { catalogFor, ensureCatalog } from "./modelcatalog";
import type { GroupStore } from "./groups";

/** Valeur sentinelle des selects mode/modèle : saisie libre. */
const CUSTOM = "__custom";

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

  /** Agents du dernier chargement — `caps` alimente les selects par profil. */
  private agents: AgentOpt[] = [];

  /** Valeurs annoncées par le profil choisi pour `kind` ("mode"|"model") :
   *  options de config (id|category, portent des labels) en priorité,
   *  complétées par la liste brute modes/models du cache. `cur` = la
   *  valeur courante rapportée par l'agent (présélection). */
  private capValues(kind: "mode" | "model"): {
    vals: { v: string; label: string }[];
    cur?: string;
  } {
    const caps: AgentCaps | undefined = this.agents.find(
      (a) => a.id === ($("f-agent") as HTMLSelectElement).value,
    )?.caps;
    const co = caps?.config_options?.find((o) => o.id === kind || o.category === kind);
    const vals = (co?.options ?? []).map((o) => ({ v: o.value, label: o.name || o.value }));
    for (const o of (kind === "mode" ? caps?.modes : caps?.models) ?? []) {
      if (!vals.some((x) => x.v === o.value)) vals.push({ v: o.value, label: o.name || o.value });
    }
    return {
      vals,
      cur:
        co?.currentValue ||
        (kind === "mode" ? caps?.current_mode : caps?.current_model) ||
        undefined,
    };
  }

  /** Valeur effective d'un champ mode/model : le select quand il est
   *  visible (hors entrée « custom »), sinon la saisie libre. */
  private fieldValue(kind: "mode" | "model"): string {
    const sel = $(`f-${kind}-sel`) as HTMLSelectElement;
    const inp = $(`f-${kind}`) as HTMLInputElement;
    return sel.hidden || sel.value === CUSTOM ? inp.value : sel.value;
  }

  /** configId de la config_option « thought_level » du profil choisi
   *  (id announced, pas nécessairement la chaîne littérale). */
  private thoughtCfgId: string | null = null;

  /** Bascule f-mode/f-model en <select> alimenté par les capacités de
   *  l'agent choisi — l'entrée « custom » du select rend la saisie
   *  libre ; agent sans annonce → input tel quel (comportement inchangé). */
  private applyAgentCaps() {
    const conn = ($("f-conn") as HTMLSelectElement).value;
    const agent = ($("f-agent") as HTMLSelectElement).value;
    for (const kind of ["mode", "model"] as const) {
      const inp = $(`f-${kind}`) as HTMLInputElement;
      const sel = $(`f-${kind}-sel`) as HTMLSelectElement;
      const prev = this.fieldValue(kind);
      const { vals, cur } = this.capValues(kind);
      sel.innerHTML = "";
      if (!vals.length) {
        sel.hidden = true;
        inp.hidden = false;
        continue;
      }
      // Option « défaut » en tête : valeur vide = laisser l'agent
      // décider — jamais availableModes[0] imposé à la place du défaut.
      const curLabel = cur ? vals.find((it) => it.v === cur)?.label : undefined;
      const d = document.createElement("option");
      d.value = "";
      d.textContent = curLabel ? `${t("opt_default")} (${curLabel})` : t("opt_default");
      sel.appendChild(d);
      for (const it of vals) {
        const o = document.createElement("option");
        o.value = it.v;
        const cost = kind === "model" ? costFor(it.v, conn, agent) : undefined;
        o.textContent = it.label + (cost ? ` · ${cost.label}` : "");
        if (cost?.detail) o.title = cost.detail;
        sel.appendChild(o);
      }
      const c = document.createElement("option");
      c.value = CUSTOM;
      c.textContent = t("opt_custom");
      sel.appendChild(c);
      sel.hidden = false;
      // Conserver la valeur déjà saisie : dans la liste → option ; hors
      // liste mais non vide → « custom » (le texte reste visible).
      // Vide → « défaut » (première option), rien n'est envoyé.
      if (vals.some((it) => it.v === prev)) sel.value = prev;
      else if (prev) sel.value = CUSTOM;
      inp.hidden = sel.value !== CUSTOM;
    }
    // thought_level : borné par l'annonce du profil (valeurs propres à
    // chaque modèle — l'agent tranchera au spawn), option « défaut » =
    // ne rien envoyer. Masqué quand le profil n'annonce rien.
    const tsel = $("f-thought-sel") as HTMLSelectElement;
    const twrap = $("f-thought-wrap") as HTMLElement;
    const tcaps: AgentCaps | undefined = this.agents.find(
      (a) => a.id === ($("f-agent") as HTMLSelectElement).value,
    )?.caps;
    const topt = tcaps?.config_options?.find(
      (o) => o.id === "thought_level" || o.category === "thought_level",
    );
    tsel.innerHTML = "";
    this.thoughtCfgId = topt?.id ?? null;
    if (!topt?.options?.length) {
      twrap.hidden = true;
    } else {
      twrap.hidden = false;
      const d = document.createElement("option");
      d.value = "";
      const curLabel = topt.currentValue
        ? topt.options.find((o) => o.value === topt.currentValue)?.name
        : undefined;
      d.textContent = curLabel
        ? `${t("opt_default")} (${curLabel})`
        : t("opt_default");
      tsel.appendChild(d);
      for (const o of topt.options) {
        const el = document.createElement("option");
        el.value = o.value;
        el.textContent = o.name || o.value;
        tsel.appendChild(el);
      }
    }
    // Coûts du profil affiché — re-render des selects à l'arrivée
    // (suffixe « · coût » des options modèle, cf. renderModelList).
    ensureCosts(conn, agent, () => {
      if (($("dlg-new") as HTMLDialogElement).open) this.applyAgentCaps();
    });
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
    this.agents = agents;
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
    this.applyAgentCaps();
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
    $("f-status").hidden = true; // bandeau d'attente d'un appel encore en vol
    const effConn = cs.disabled ? (conn ?? "") : cs.value;
    await this.fillAgents(effConn, ref?.agent);
    if (ref) ($("f-cwd") as HTMLInputElement).value = ref.cwd;
    // Suggestions de modèles : union des config_options connues des sessions
    this.modelSessions = await call<SessionMeta[]>("list_sessions").catch(() => [] as SessionMeta[]);
    this.renderModelList();
    ($("dlg-new") as HTMLDialogElement).showModal();
  }

  /** Sessions sources des suggestions de modèles (union des
   *  config_options « model »), mémorisées pour renderModelList. */
  private modelSessions: SessionMeta[] = [];

  /** Remplit le datalist #model-list : dédup par valeur, libellé
   *  « nom · coût » quand le catalogue du profil de la session source
   *  est connu (model_costs). Re-rendu à l'arrivée des coûts. */
  private renderModelList() {
    const dl = $("model-list");
    dl.innerHTML = "";
    const seen = new Set<string>();
    const pairs = new Set<string>();
    for (const s of this.modelSessions) {
      for (const co of s.config_options ?? []) {
        if (co.id !== "model" && co.category !== "model") continue;
        for (const o of co.options) {
          if (seen.has(o.value)) continue;
          seen.add(o.value);
          const cost = costFor(o.value, s.conn ?? "", s.agent);
          const el = document.createElement("option");
          el.value = o.value;
          el.label = (o.name || o.value) + (cost ? ` · ${cost.label}` : "");
          if (cost?.detail) el.title = cost.detail;
          dl.appendChild(el);
        }
      }
      if (s.agent) pairs.add(`${s.conn ?? ""} ${s.agent}`);
    }
    // Variantes catalogue-only des profils représentés (model_catalog) —
    // badgées, applicables via le side-channel démon à la création.
    for (const p of pairs) {
      const i = p.indexOf(" ");
      const conn = p.slice(0, i);
      const agent = p.slice(i + 1);
      const cat = catalogFor(conn, agent);
      if (cat) {
        for (const e of Object.values(cat)) {
          if (seen.has(e.value)) continue;
          seen.add(e.value);
          const el = document.createElement("option");
          el.value = e.value;
          el.label = `${e.label} · ${t("catalog_badge")}`;
          dl.appendChild(el);
        }
      }
    }
    // Coûts + catalogues des profils représentés — re-render du datalist
    // à l'arrivée (dialogue encore ouvert seulement).
    for (const p of pairs) {
      const i = p.indexOf(" ");
      const conn = p.slice(0, i);
      const agent = p.slice(i + 1);
      const rerender = () => {
        if (($("dlg-new") as HTMLDialogElement).open) this.renderModelList();
      };
      ensureCosts(conn, agent, rerender);
      ensureCatalog(conn, agent, rerender);
    }
  }

  /** Session créée côté démon dont la réponse RPC s'est perdue
   *  (timeout 60 s / tunnel coupé APRÈS l'insert — le démon répond
   *  dès l'insertion, le spawn tourne en tâche de fond) : retrouvée
   *  par agent (+ sid natif pour un attach), cwd compatible et
   *  création de moins de 5 min. La plus récente gagne ; `conn`
   *  filtrée quand elle est connue (multi-serveurs). */
  private async findLateCreation(
    agent: string,
    cwd: string,
    conn: string | undefined,
    nativeId?: string,
  ): Promise<string | null> {
    const metas = await call<SessionMeta[]>("list_sessions").catch(
      () => [] as SessionMeta[],
    );
    const now = Date.now();
    // cwd saisi « ~… » ou relatif : le démon stocke le chemin expansé —
    // comparaison par suffixe pour ne pas rater la session créée.
    const tail = cwd.replace(/^~(?=\/|$)/, "").replace(/^\/+/, "");
    const cands = metas
      .filter(
        (m) =>
          m.agent === agent &&
          (!nativeId || m.native_session_id === nativeId) &&
          (!conn || !m.conn || m.conn === conn) &&
          (m.cwd === cwd ||
            (!!tail && (m.cwd === tail || m.cwd.endsWith("/" + tail)))) &&
          now - Date.parse(m.created_at) < 5 * 60_000,
      )
      .sort((a, b) => b.created_at.localeCompare(a.created_at));
    return cands[0]?.id ?? null;
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

    $("f-agent").addEventListener("change", () => this.applyAgentCaps());
    for (const kind of ["mode", "model"] as const) {
      ($(`f-${kind}-sel`) as HTMLSelectElement).onchange = (e) => {
        const sel = e.target as HTMLSelectElement;
        ($(`f-${kind}`) as HTMLInputElement).hidden = sel.value !== CUSTOM;
      };
    }

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
      // Attente bornée à 60 s côté RPC : le compteur rend l'attente
      // visible au lieu de 60 s de silence après create_wait.
      const t0 = Date.now();
      const tick = setInterval(() => {
        fst.textContent = `${t("create_wait")} (${Math.round((Date.now() - t0) / 1000)} s)`;
      }, 1000);
      // L'erreur est capturée, pas affichée tout de suite : le démon a
      // pu créer la session puis perdre la réponse (timeout, tunnel
      // coupé) — une sonde list_sessions la retrouve avant de déclarer
      // l'échec (un échec de création ne doit jamais être muet).
      let callErr: unknown = null;
      let res = await (attach
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
            mode: this.fieldValue("mode") || undefined,
            model: this.fieldValue("model") || undefined,
            prompt: ($("f-prompt") as HTMLTextAreaElement).value || undefined,
          })
      ).catch((er) => {
        callErr = er;
        return null;
      });
      clearInterval(tick);
      fst.hidden = true;
      btn.disabled = false;
      btn.classList.remove("spin");
      btn.textContent = label0;
      if (!res && callErr != null) {
        fst.textContent = t("create_probe");
        fst.hidden = false;
        const late = await this.findLateCreation(
          ($("f-agent") as HTMLSelectElement).value,
          cwd,
          grp?.conn ?? selConn,
          attach || undefined,
        );
        fst.hidden = true;
        if (late) {
          res = { session: late };
          notify("warn", t("create_late"));
        }
      }
      if (!res) {
        // Dialogue détaché (Échap) pendant l'appel : une erreur dans
        // f-err serait invisible — basculer en toast.
        if (dlg.open) dlgErr("f-err", String(callErr));
        else showErr(String(callErr));
        return;
      }
      // thought_level choisi à la création : la session vient d'être
      // insérée (spawn en tâche de fond) — set_config_option tombe en
      // pending_config et est rejoué post-handshake, APRÈS le modèle
      // (ordonné model→options côté démon). Ordre imposé : model PUIS
      // thought_level, jamais l'inverse.
      const thought = attach
        ? ""
        : ($("f-thought-sel") as HTMLSelectElement).value;
      if (thought && this.thoughtCfgId) {
        call("set_config_option", {
          session: res.session,
          conn: grp?.conn ?? selConn ?? "",
          config_id: this.thoughtCfgId,
          value: thought,
        }).catch((er) => showErr(String(er)));
      }
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
