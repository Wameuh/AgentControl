// ---------------------------------------------------------------------------
// Panneau des paramètres : rendu dans la zone de discussion (overlay plein
// #chat, pas un dialogue modal) — préférences persistées en localStorage et
// vérification manuelle des mises à jour.
// ---------------------------------------------------------------------------

import { getVersion } from "@tauri-apps/api/app";
import { $ } from "../core/dom";
import { applyTheme, isAndroid } from "../core/chrome";
import { inlineImages } from "../core/format";
import { cmpVer } from "../core/conn";
import { setLang, getLang, t, type Lang } from "../i18n";
import { nav } from "../ui/nav";
import { checkUpdate, performClientUpdate } from "./update";

/** Préférence d'envoi : "queue" (file FIFO, défaut) | "steer" (injection
 *  mid-turn quand l'agent tourne ; idle → envoi normal côté démon). */
export type SendPref = "queue" | "steer";
export const sendPref = (): SendPref =>
  localStorage.getItem("ac-sendmode") === "steer" ? "steer" : "queue";

export class SettingsPanel {
  /** Langue changée → sidebar + header à retraduire (injecté). */
  langChanged: () => void = () => {};
  /** Mode d'envoi changé → hints du composer à mettre à jour (injecté). */
  sendModeChanged: () => void = () => {};

  private isOpen() {
    return !$("settings-view").hidden;
  }

  /** Rend les éléments recouverts de #chat inertes (clics + tabulation) —
   *  le <dialog> modal captait le focus, un <div> doit le faire à la main. */
  private setChatInert(v: boolean) {
    for (const el of [...$("chat").children]) {
      if (el.id !== "settings-view") (el as HTMLElement).inert = v;
    }
  }

  open() {
    if (this.isOpen()) return;
    // Contrôles synchronisés sur l'état courant à chaque ouverture.
    ($("s-theme") as HTMLSelectElement).value =
      document.documentElement.classList.contains("light") ? "light" : "dark";
    ($("s-lang") as HTMLSelectElement).value = getLang();
    ($("s-sendmode") as HTMLSelectElement).value = sendPref();
    ($("s-autoconn") as HTMLInputElement).checked =
      localStorage.getItem("ac-autoconnect") !== "0";
    ($("s-images") as HTMLInputElement).checked = inlineImages();
    $("s-update-status").textContent = "";
    ($("s-doupdate") as HTMLButtonElement).hidden = true;
    void getVersion()
      .then((v) => ($("s-version").textContent = `v${v}`))
      .catch(() => ($("s-version").textContent = ""));
    const view = $("settings-view");
    view.hidden = false;
    this.setChatInert(true);
    // Le focus peut rester dans #input (ouverture par clic programmatique
    // depuis la sheet ⋮) → frappes invisibles envoyées à l'agent. Le panneau
    // prend le focus : tabindex=-1, sans scroll.
    view.focus({ preventScroll: true });
    nav.push("settings");
  }

  close() {
    if (!this.isOpen()) return;
    $("settings-view").hidden = true;
    this.setChatInert(false);
    nav.close("settings");
    // Focus rendu au champ (pas sous Android : ouvrirait le clavier virtuel).
    if (!isAndroid()) ($("input") as HTMLTextAreaElement).focus();
  }

  toggle() {
    if (this.isOpen()) this.close();
    else this.open();
  }

  /** "Vérifier les mises à jour" : remote vs local → statut + bouton maj. */
  private async runUpdateCheck() {
    const btn = $("s-checkupdate") as HTMLButtonElement;
    const status = $("s-update-status");
    const go = $("s-doupdate") as HTMLButtonElement;
    go.hidden = true;
    status.textContent = t("update_checking");
    btn.disabled = true;
    try {
      const { local, remote } = await checkUpdate();
      if (cmpVer(remote, local) > 0) {
        status.textContent = t("update_available", { v: remote });
        go.hidden = false;
        go.onclick = async () => {
          go.disabled = true;
          try {
            await performClientUpdate(remote);
          } finally {
            go.disabled = false;
          }
        };
      } else {
        status.textContent = t("update_uptodate", { v: local });
      }
    } catch {
      status.textContent = t("update_failed");
    } finally {
      btn.disabled = false;
    }
  }

  init() {
    $("tb-settings").onclick = () => this.toggle();
    $("settings-close").onclick = () => this.close();
    // Échap ferme le panneau (les <dialog> l'avaient nativement, pas un div)
    // — sauf si un dialogue est ouvert par-dessus (About…) : Échap = lui.
    window.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && this.isOpen() && !document.querySelector("dialog[open]"))
        this.close();
    });

    ($("s-theme") as HTMLSelectElement).onchange = (e) =>
      applyTheme((e.target as HTMLSelectElement).value as "dark" | "light");
    ($("s-lang") as HTMLSelectElement).onchange = (e) => {
      setLang((e.target as HTMLSelectElement).value as Lang);
      this.langChanged();
    };
    ($("s-sendmode") as HTMLSelectElement).onchange = (e) => {
      localStorage.setItem(
        "ac-sendmode",
        (e.target as HTMLSelectElement).value === "steer" ? "steer" : "queue",
      );
      this.sendModeChanged();
    };
    ($("s-autoconn") as HTMLInputElement).onchange = (e) =>
      localStorage.setItem(
        "ac-autoconnect",
        (e.target as HTMLInputElement).checked ? "1" : "0",
      );
    ($("s-images") as HTMLInputElement).onchange = (e) =>
      localStorage.setItem(
        "ac-inline-images",
        (e.target as HTMLInputElement).checked ? "1" : "0",
      );
    ($("s-checkupdate") as HTMLButtonElement).onclick = () => this.runUpdateCheck();
  }
}
