// ---------------------------------------------------------------------------
// Dialogue « capacités des agents » (item #268) : pour chaque profil
// déclaré par `list_agents` sur la connexion choisie — modes, modèles,
// config_options avec labels et valeur courante, date du dernier probe
// (agent_caps.json) et bouton « re-sonder » (RPC refresh_agent_caps,
// spawn jetable côté démon). Les `extra_models` du profil
// ([agents.<id>.extra_models] / registre auto — ex. opus de claude)
// sont fusionnés à caps.models, marqués « extra » : ce sont des
// compléments d'annonce, pas des entrées ACP.
// ---------------------------------------------------------------------------

import { $, dlgErr, esc } from "../core/dom";
import { connLabel } from "../core/conn";
import { call } from "../core/rpc";
import { t } from "../i18n";
import type { AgentCaps, AgentOpt, ConfigOption, ConfigOpt } from "../core/types";

/** Modèles fusionnés pour l'affichage : annonce ACP (caps.models)
 *  + extras du profil non déjà présents. */
function mergedModels(a: AgentOpt): { v: string; label: string; extra: boolean }[] {
  const out: { v: string; label: string; extra: boolean }[] = [];
  const seen = new Set<string>();
  for (const m of a.caps?.models ?? []) {
    out.push({ v: m.value, label: m.name || m.value, extra: false });
    seen.add(m.value);
  }
  for (const e of a.extra_models ?? []) {
    if (seen.has(e.value)) continue;
    out.push({ v: e.value, label: e.label || e.value, extra: true });
    seen.add(e.value);
  }
  return out;
}

function fmtOpts(
  opts: ConfigOpt[] | undefined,
  cur: string | undefined,
): string {
  if (!opts?.length) return `<span class="ac-none">${esc(t("caps_none"))}</span>`;
  return opts
    .map((o) => {
      const curMark =
        cur && o.value === cur
          ? ` <span class="ac-cur">${esc(t("caps_current"))}</span>`
          : "";
      const desc = o.description
        ? ` <span class="ac-desc">— ${esc(o.description)}</span>`
        : "";
      return `<li><code>${esc(o.value)}</code> ${esc(o.name || o.value)}${curMark}${desc}</li>`;
    })
    .join("");
}

function fmtConfigOptions(cos: ConfigOption[] | undefined): string {
  if (!cos?.length) return `<span class="ac-none">${esc(t("caps_none"))}</span>`;
  return cos
    .map((o) => {
      const cur = o.currentValue
        ? ` <span class="ac-cur">${esc(t("caps_current"))}: ${esc(o.currentValue)}</span>`
        : "";
      return `<details class="ac-co"><summary><code>${esc(o.id)}</code> ${esc(o.name)}${cur}</summary><ul>${fmtOpts(o.options, o.currentValue)}</ul></details>`;
    })
    .join("");
}

function fmtUpdated(at?: string): string {
  if (!at) return esc(t("caps_never"));
  const d = new Date(at);
  return Number.isNaN(d.getTime()) ? esc(at) : d.toLocaleString();
}

export class AgentCapsDialog {
  /** Connexions connues (clés) pour le select serveur (injecté). */
  knownConns: () => string[] = () => [];

  private agents: AgentOpt[] = [];
  private conn = "";
  private probing = new Set<string>();
  private installing = new Set<string>();

  open(conn?: string) {
    const dlg = $("dlg-agentcaps") as HTMLDialogElement;
    if (dlg.open) return;
    dlgErr("ac-err", null);
    // Select serveur : toutes les connexions connues, présélection =
    // conn passée (session affichée) sinon la première.
    const cs = $("ac-conn") as HTMLSelectElement;
    cs.innerHTML = "";
    const conns = this.knownConns();
    for (const k of conns) {
      const o = document.createElement("option");
      o.value = k;
      o.textContent = connLabel(k) || k;
      cs.appendChild(o);
    }
    cs.value = conn && conns.includes(conn) ? conn : (conns[0] ?? "");
    this.conn = cs.value;
    // Select serveur masqué en mono-connexion (comme dlg-new).
    ($("ac-conn-wrap") as HTMLElement).hidden = conns.length < 2;
    dlg.showModal();
    void this.reload();
  }

  private async reload() {
    const list = $("ac-list");
    list.innerHTML = `<p class="ac-loading">${esc(t("agents_loading"))}</p>`;
    try {
      this.agents = await call<AgentOpt[]>(
        "list_agents",
        this.conn ? { conn: this.conn } : {},
      );
      this.render();
    } catch (e) {
      list.innerHTML = `<p class="ac-err">${esc(String(e))}</p>`;
    }
  }

  private render() {
    const list = $("ac-list");
    list.innerHTML = "";
    if (!this.agents.length) {
      list.innerHTML = `<p class="ac-none">${esc(t("agents_failed"))}</p>`;
      return;
    }
    for (const a of this.agents) {
      const caps: AgentCaps | undefined = a.caps;
      const card = document.createElement("section");
      card.className = "ac-card";
      const badges: string[] = [];
      const installable =
        a.available === false && a.unavailable?.installable === true;
      if (a.available === false)
        badges.push(`<span class="badge warn">${esc(t("agent_missing"))}</span>`);
      if (a.via) badges.push(`<span class="badge">${esc(a.via)}</span>`);
      const models = mergedModels(a);
      // Profil indisponible mais installable (registre agents_auto,
      // #277) : bouton « installer » — le démon exécute le npm i -g
      // déclaré (install_agent_adapter), le rescan le voit ensuite.
      const installBtn = installable
        ? `<button type="button" class="ac-probe ac-install" ${this.installing.has(a.id) ? "disabled" : ""}>${esc(this.installing.has(a.id) ? t("agent_installing") : t("agent_install"))}</button>`
        : "";
      card.innerHTML = `
        <header class="ac-head">
          <strong>${esc(a.name)}</strong> <code>${esc(a.id)}</code>
          ${badges.join(" ")}
          <span class="ac-sp"></span>
          ${installBtn}
          <button type="button" class="ac-probe" ${this.probing.has(a.id) || a.available === false ? "disabled" : ""}>
            ${esc(this.probing.has(a.id) ? t("caps_probing") : t("caps_reprobe"))}
          </button>
        </header>
        <div class="ac-meta">${esc(t("caps_updated"))}: ${fmtUpdated(caps?.updated_at)}</div>
        <div class="ac-grid">
          <div><h5>${esc(t("caps_modes"))}</h5><ul>${fmtOpts(caps?.modes, caps?.current_mode)}</ul></div>
          <div><h5>${esc(t("caps_models"))}${caps?.current_model ? ` <span class="ac-cur">${esc(t("caps_current"))}: ${esc(caps.current_model)}</span>` : ""}</h5>
            ${models.length ? `<ul>${models.map((m) => `<li><code>${esc(m.v)}</code> ${esc(m.label)}${m.extra ? ` <span class="badge">${esc(t("caps_extra"))}</span>` : ""}${caps?.current_model === m.v ? ` <span class="ac-cur">${esc(t("caps_current"))}</span>` : ""}</li>`).join("")}</ul>` : `<span class="ac-none">${esc(t("caps_none"))}</span>`}
          </div>
          <div class="ac-co-wrap"><h5>${esc(t("caps_config"))}</h5>${fmtConfigOptions(caps?.config_options)}</div>
        </div>`;
      const btn = card.querySelector<HTMLButtonElement>(".ac-probe:not(.ac-install)");
      if (btn) btn.onclick = () => void this.probe(a.id);
      const ibtn = card.querySelector<HTMLButtonElement>(".ac-install");
      if (ibtn)
        ibtn.onclick = () => {
          ibtn.title = [
            a.unavailable?.searched?.length
              ? `${t("agent_searched")}: ${a.unavailable.searched.join(", ")}`
              : "",
            a.unavailable?.hint ?? "",
          ]
            .filter(Boolean)
            .join(" — ");
          void this.install(a.id);
        };
      list.appendChild(card);
    }
  }

  /** Re-sonde un profil : refresh_agent_caps (probe jetable côté
   *  démon) puis rechargement de list_agents — le démon a déjà
   *  ré-appliqué les caps aux sessions vivantes du profil. */
  private async probe(agent: string) {
    if (this.probing.has(agent)) return;
    this.probing.add(agent);
    this.render();
    dlgErr("ac-err", null);
    try {
      await call("refresh_agent_caps", {
        agent,
        ...(this.conn ? { conn: this.conn } : {}),
      });
    } catch (e) {
      dlgErr("ac-err", String(e));
      this.probing.delete(agent);
      this.render();
      return;
    }
    this.probing.delete(agent);
    await this.reload();
  }

  /** Installe l'adaptateur ACP du profil côté démon
   *  (install_agent_adapter — npm i -g du registre, #277) puis
   *  recharge : le rescan PATH rend le profil available sans
   *  restart. Erreur (npm absent, EACCES, réseau) → ac-err. */
  private async install(agent: string) {
    if (this.installing.has(agent)) return;
    this.installing.add(agent);
    this.render();
    dlgErr("ac-err", null);
    try {
      await call("install_agent_adapter", {
        agent,
        ...(this.conn ? { conn: this.conn } : {}),
      });
    } catch (e) {
      dlgErr("ac-err", String(e));
      this.installing.delete(agent);
      this.render();
      return;
    }
    this.installing.delete(agent);
    await this.reload();
  }

  init() {
    const dlg = $("dlg-agentcaps") as HTMLDialogElement;
    $("ac-close").onclick = () => dlg.close();
    $("ac-refresh").onclick = () => void this.reload();
    ($("ac-conn") as HTMLSelectElement).onchange = (e) => {
      this.conn = (e.target as HTMLSelectElement).value;
      void this.reload();
    };
    dlg.addEventListener("close", () => {
      this.probing.clear();
      this.installing.clear();
      this.agents = [];
    });
  }
}
