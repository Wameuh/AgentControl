// ---------------------------------------------------------------------------
// Étapes de la tâche publiées par l'agent (skill ac-steps) : liste
// persistée côté démon (steps.json), remplacée ENTIÈRE à chaque
// session_set_steps — l'agent garde sa liste et repousse tout (pas de
// drift d'index). Panneau ouvrable depuis le header (btn-steps) et le
// sheet ⋮ mobile : nom + badge coloré par statut, hint quand vide.
// Live update via l'event `steps` (état complet, pas de delta) ;
// reconstruction à l'ouverture via session_info.steps.
// ---------------------------------------------------------------------------

import { $, esc } from "../core/dom";
import { bgErr, callBg } from "../core/rpc";
import { app } from "../core/state";
import { t } from "../i18n";

export type StepStatus = "todo" | "in_progress" | "done" | "abandoned";

export interface SessionStep {
  name: string;
  status: StepStatus;
}

export class Steps {
  private panel = $("steps-panel");
  private list = $("sp-list");
  private count = $("sp-count");
  private items: SessionStep[] = [];
  /** Session dont `items` est la liste chargée. */
  private sid = "";
  /** Sid avec un session_info en vol — dedup des déclencheurs. */
  private loading = "";

  init() {
    $("btn-steps").onclick = () => this.togglePanel();
    $("sp-close").onclick = () => this.close();
  }

  get isOpen() {
    return !this.panel.hidden;
  }

  togglePanel() {
    if (this.isOpen) {
      this.close();
      return;
    }
    if (!app.current) return;
    this.panel.hidden = false;
    this.renderPanel();
    // Liste potentiellement modifiée par l'agent/depuis un autre client
    // → refresh à chaque ouverture (session_info porte `steps`).
    void this.load();
  }

  close() {
    this.panel.hidden = true;
  }

  /** Chargement de la liste de la session affichée — idempotent, dedup
   *  d'un appel en vol sur le même sid. */
  private async load() {
    const s = app.current;
    if (!s) {
      this.items = [];
      this.sid = "";
      this.renderPanel();
      return;
    }
    if (this.loading === s.id) return;
    this.loading = s.id;
    try {
      const r = await callBg<{ steps?: SessionStep[] }>("session_info", {
        session: s.id,
      });
      if (app.current?.id !== s.id) return; // session changée entre-temps
      this.items = r.steps ?? [];
      this.sid = s.id;
      this.renderPanel();
    } catch (e) {
      // Load de fond : coupure transitoire → silence ; échec
      // applicatif (vieux démon sans `steps` dans la réponse) → toast.
      if (app.current?.id === s.id) bgErr("session_info", e, s.conn ?? "");
    } finally {
      if (this.loading === s.id) this.loading = "";
    }
  }

  /** La session ouverte a fini son chargement — (re)lit ses étapes. */
  sessionOpened() {
    void this.load();
  }

  /** Session affichée changée ou disparue : la liste de l'ancienne ne
   *  doit pas fuiter dans le panneau de la nouvelle ; /clear-rebind
   *  sur la même est suivi par l'event `steps` (vide → clear_steps
   *  côté démon) + ce reload par sécurité. */
  sessionChanged() {
    const same = app.current && app.current.id === this.sid;
    this.items = [];
    if (!app.current) {
      this.sid = "";
      this.close();
      this.renderPanel();
      return;
    }
    if (same) void this.load();
    else this.renderPanel();
  }

  /** Event `steps` live (liste complète — pas de delta) : appliqué
   *  directement, même panneau fermé (le count reste à jour). */
  onStepsEvent(steps: SessionStep[] | undefined) {
    const s = app.current;
    if (!s || !Array.isArray(steps)) return;
    this.items = steps;
    this.sid = s.id;
    this.renderPanel();
  }

  /** Vrai si la session a des étapes connues (badge header). */
  hasSteps() {
    return this.items.length > 0;
  }

  private renderPanel() {
    this.count.textContent = this.items.length ? String(this.items.length) : "";
    this.list.innerHTML = "";
    if (!this.items.length) {
      const d = document.createElement("div");
      d.className = "sp-empty";
      d.textContent = t("steps_none");
      this.list.appendChild(d);
      return;
    }
    for (const st of this.items) {
      const row = document.createElement("div");
      row.className = `sp-row sp-${st.status}`;
      const status =
        st.status === "todo"
          ? t("step_todo")
          : st.status === "in_progress"
            ? t("step_in_progress")
            : st.status === "done"
              ? t("step_done")
              : t("step_abandoned");
      row.innerHTML =
        `<span class="sp-badge">${esc(status)}</span>` +
        `<span class="sp-name">${esc(st.name)}</span>`;
      this.list.appendChild(row);
    }
  }
}
