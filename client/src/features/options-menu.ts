// ---------------------------------------------------------------------------
// Menu « Options » de la titlebar (desktop) : un bouton libellé remplace
// les cinq icônes carte / journaux / composants / paramètres / à propos.
// Les boutons d'origine restent dans le DOM (masqués en CSS) : le menu,
// le ⋮ Android et les tests les cliquent — leurs handlers ne changent pas.
// Le diagramme garde son onglet dans la barre latérale.
// ---------------------------------------------------------------------------

import { $, esc } from "../core/dom";
import { isAndroid } from "../core/chrome";
import { connLabel } from "../core/conn";
import { t } from "../i18n";
import { icon, type IconName } from "../ui/icons";

interface MenuItem {
  label: string;
  icon: IconName;
  /** Raccourci affiché (« Ctrl , ») ou précision (nom d'hôte). */
  hint?: string;
  kbd?: boolean;
  fn: () => void;
  sep?: boolean;
}

export interface OptionsMenuDeps {
  compsCapable: () => boolean;
  /** Connexion liée (libellé d'hôte du groupe serveur), null = aucune. */
  connKey: () => string | null;
  openSettings: () => void;
  openComponents: () => void;
  openOrchestrator: () => void;
  openUpdates: (anchor: HTMLElement) => void;
  /** Quitter les Paramètres (garde « non enregistré ») avant une autre vue. */
  leaveSettings: () => Promise<boolean>;
}

const MOD = /Mac OS X|Macintosh/i.test(navigator.userAgent) ? "Cmd" : "Ctrl";

export class OptionsMenu {
  constructor(private deps: OptionsMenuDeps) {}

  private pop() {
    return $("tb-menu-pop");
  }

  private btn() {
    return $("tb-options");
  }

  isOpen() {
    return !this.pop().hidden;
  }

  init() {
    if (isAndroid()) return;
    this.btn().onclick = () => (this.isOpen() ? this.close() : this.open());
    document.addEventListener("mousedown", (e) => {
      if (!this.isOpen()) return;
      const tgt = e.target as Node;
      if (this.pop().contains(tgt) || this.btn().contains(tgt)) return;
      this.close();
    });
    this.pop().addEventListener("keydown", (e) => {
      const items = [...this.pop().querySelectorAll<HTMLButtonElement>("button")];
      const i = items.indexOf(document.activeElement as HTMLButtonElement);
      if (e.key === "ArrowDown" || e.key === "ArrowUp") {
        e.preventDefault();
        const d = e.key === "ArrowDown" ? 1 : -1;
        items[(i + d + items.length) % items.length]?.focus();
      } else if (e.key === "Escape") {
        e.stopPropagation();
        this.close();
        this.btn().focus();
      }
    });
    // Journaux : Ctrl/Cmd+Maj+L (Préférences : Ctrl/Cmd+, côté settings).
    window.addEventListener("keydown", (e) => {
      if ((e.ctrlKey || e.metaKey) && e.shiftKey && !e.altKey && e.key.toLowerCase() === "l") {
        e.preventDefault();
        void this.run(() => $("tb-logs").click());
      }
    });
  }

  private items(): MenuItem[] {
    const d = this.deps;
    const out: MenuItem[] = [
      { label: t("settings_title"), icon: "sliders", hint: `${MOD} ,`, kbd: true, fn: () => d.openSettings() },
    ];
    if (d.compsCapable()) {
      const ck = d.connKey();
      const host = ck && ck !== "web" ? connLabel(ck) : "";
      out.push(
        { label: t("comp_h"), icon: "tool", hint: host, fn: () => void this.run(d.openComponents) },
        { label: t("orch_h"), icon: "users", fn: () => void this.run(d.openOrchestrator) },
      );
    }
    out.push(
      { label: t("logs"), icon: "scroll", hint: `${MOD} ⇧ L`, kbd: true, fn: () => void this.run(() => $("tb-logs").click()) },
      { label: t("upd_menu"), icon: "up", fn: () => d.openUpdates(this.btn()) },
      { label: t("about_title"), icon: "info", fn: () => $("tb-about").click(), sep: true },
    );
    return out;
  }

  /** Ouvre une autre vue en passant par la garde des Paramètres. */
  private async run(fn: () => void) {
    if (await this.deps.leaveSettings()) fn();
  }

  open() {
    const pop = this.pop();
    pop.innerHTML = "";
    for (const it of this.items()) {
      if (it.sep) pop.appendChild(document.createElement("hr"));
      const b = document.createElement("button");
      b.type = "button";
      b.setAttribute("role", "menuitem");
      b.innerHTML =
        `${icon(it.icon)}<span class="tbm-l">${esc(it.label)}</span>` +
        (it.hint ? `<span class="${it.kbd ? "tbm-kbd" : "tbm-hint"}">${esc(it.hint)}</span>` : "");
      b.onclick = () => {
        this.close();
        it.fn();
      };
      pop.appendChild(b);
    }
    pop.hidden = false;
    const r = this.btn().getBoundingClientRect();
    const w = pop.getBoundingClientRect().width;
    pop.style.left = `${Math.max(8, Math.min(r.right - w, window.innerWidth - w - 8))}px`;
    pop.style.top = `${r.bottom + 6}px`;
    this.btn().setAttribute("aria-expanded", "true");
    pop.querySelector<HTMLButtonElement>("button")?.focus();
  }

  close() {
    if (!this.isOpen()) return;
    this.pop().hidden = true;
    this.btn().setAttribute("aria-expanded", "false");
  }
}
