// ---------------------------------------------------------------------------
// Panneau des paramètres : rendu dans la zone de discussion (overlay plein
// #chat, pas un dialogue modal) — préférences persistées en localStorage et
// vérification manuelle des mises à jour.
// ---------------------------------------------------------------------------

import { getVersion, isWeb } from "../core/bridge";
import { $ } from "../core/dom";
import { applyTheme, applyAccent, accentPref, applyBgAnim, bgAnimOn, isAndroid, type Accent } from "../core/chrome";
import { defaultEncoding } from "../core/encoding";
import { inlineImages } from "../core/format";
import { cmpVer } from "../core/conn";
import { setLang, getLang, t, type Lang } from "../i18n";
import { nav } from "../ui/nav";
import { checkUpdate, performClientUpdate, rcChannel } from "./update";
import {
  sounds,
  sndEnabled,
  sndTypeOn,
  sndVolume,
  sndOnlyUnfocused,
  type SoundKind,
} from "./sounds";

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
  /** Thème changé → les terminaux ouverts re-pick les couleurs (injecté). */
  themeChanged: () => void = () => {};
  /** Le panneau s'apprête à s'ouvrir : les autres overlays de #chat
   *  (vue connexion, carte) doivent se fermer — deux panneaux pleins
   *  empilés seraient inertes l'un sous l'autre (injecté par le
   *  bootstrap). */
  onOpening: () => void = () => {};

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
    this.onOpening();
    // Contrôles synchronisés sur l'état courant à chaque ouverture.
    ($("s-theme") as HTMLSelectElement).value =
      document.documentElement.classList.contains("light") ? "light" : "dark";
    ($("s-accent") as HTMLSelectElement).value = accentPref();
    ($("s-bganim") as HTMLInputElement).checked = bgAnimOn();
    ($("s-lang") as HTMLSelectElement).value = getLang();
    ($("s-sendmode") as HTMLSelectElement).value = sendPref();
    ($("s-autoconn") as HTMLInputElement).checked =
      localStorage.getItem("ac-autoconnect") !== "0";
    ($("s-images") as HTMLInputElement).checked = inlineImages();
    ($("s-rcchan") as HTMLInputElement).checked = rcChannel();
    ($("s-encoding") as HTMLSelectElement).value = defaultEncoding();
    ($("s-snd") as HTMLInputElement).checked = sndEnabled();
    for (const k of ["done", "question", "error"] as SoundKind[]) {
      ($(`s-snd-${k}`) as HTMLInputElement).checked = sndTypeOn(k);
    }
    ($("s-snd-unfocused") as HTMLInputElement).checked = sndOnlyUnfocused();
    ($("s-snd-vol") as HTMLInputElement).value = String(
      Math.round(sndVolume() * 100),
    );
    this.syncSndOpts();
    $("s-update-status").textContent = "";
    ($("s-doupdate") as HTMLButtonElement).hidden = true;
    // Web : pas d'auto-connexion optionnelle (la conn unique est vitale)
    // ni d'update client (la SPA suit le binaire démon) ni de canal rc.
    ($("s-autoconn").closest(".s-row") as HTMLElement).hidden = isWeb;
    ($("s-rcchan").closest(".s-row") as HTMLElement).hidden = isWeb;
    $("s-update-row").hidden = isWeb;
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
    // L'inert est retiré même si le panneau avait déjà été masqué par un
    // autre chemin (early-return historique : un résiduel gelait #chat
    // sans cause visible — le watchdog core/modal filet de sécurité).
    const was = this.isOpen();
    $("settings-view").hidden = true;
    // Ne lever l'inert que si les autres overlays #chat sont aussi
    // fermés — sinon on ré-expose #input derrière conn-view/map-view
    // (frappes invisibles).
    if ($("conn-view").hidden && $("map-view").hidden) {
      this.setChatInert(false);
    }
    // nav.close est idempotent (entrée absente = no-op) : l'appeler même
    // si le panneau avait été masqué ailleurs purge la pile nav résiduelle.
    nav.close("settings");
    if (!was) return;
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

    ($("s-theme") as HTMLSelectElement).onchange = (e) => {
      applyTheme((e.target as HTMLSelectElement).value as "dark" | "light");
      this.themeChanged();
    };
    ($("s-accent") as HTMLSelectElement).onchange = (e) => {
      applyAccent((e.target as HTMLSelectElement).value as Accent);
      this.themeChanged();
    };
    ($("s-bganim") as HTMLInputElement).onchange = (e) =>
      applyBgAnim((e.target as HTMLInputElement).checked);
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
    ($("s-rcchan") as HTMLInputElement).onchange = (e) =>
      localStorage.setItem(
        "ac-rcchannel",
        (e.target as HTMLInputElement).checked ? "1" : "0",
      );
    ($("s-encoding") as HTMLSelectElement).onchange = (e) =>
      localStorage.setItem("ac-encoding", (e.target as HTMLSelectElement).value);
    ($("s-snd") as HTMLInputElement).onchange = (e) => {
      localStorage.setItem(
        "ac-snd",
        (e.target as HTMLInputElement).checked ? "1" : "0",
      );
      this.syncSndOpts();
    };
    for (const k of ["done", "question", "error"] as SoundKind[]) {
      ($(`s-snd-${k}`) as HTMLInputElement).onchange = (e) =>
        localStorage.setItem(
          `ac-snd-${k}`,
          (e.target as HTMLInputElement).checked ? "1" : "0",
        );
      // « Tester » joue même tout désactivé — le geste débloque l'autoplay
      // de la webview (Android) en plus de donner un aperçu volume réel.
      ($(`s-snd-test-${k}`) as HTMLButtonElement).onclick = () =>
        sounds.test(k);
    }
    ($("s-snd-unfocused") as HTMLInputElement).onchange = (e) =>
      localStorage.setItem(
        "ac-snd-unfocused",
        (e.target as HTMLInputElement).checked ? "1" : "0",
      );
    ($("s-snd-vol") as HTMLInputElement).oninput = (e) =>
      localStorage.setItem(
        "ac-snd-vol",
        (e.target as HTMLInputElement).value,
      );
    ($("s-checkupdate") as HTMLButtonElement).onclick = () => this.runUpdateCheck();
  }

  /** Master off → les sous-options sons sont grisées/inertes — sauf les
   *  boutons « Tester », qui restent cliquables (preview au volume réglé
   *  et déblocage de l'autoplay par le geste utilisateur). */
  private syncSndOpts() {
    const off = !($("s-snd") as HTMLInputElement).checked;
    $("s-snd-opts").classList.toggle("off", off);
    for (const el of $("s-snd-opts").querySelectorAll<HTMLInputElement>(
      "input",
    )) {
      el.disabled = off;
    }
  }
}
