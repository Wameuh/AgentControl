// ---------------------------------------------------------------------------
// Panneau des paramètres : rendu dans la zone de discussion (overlay plein
// #chat, pas un dialogue modal) — préférences persistées en localStorage et
// vérification manuelle des mises à jour.
// ---------------------------------------------------------------------------

import { invoke, getVersion, isWeb } from "../core/bridge";
import { $, notify } from "../core/dom";
import { bindLogView } from "../core/logview";
import { applyTheme, applyAccent, accentPref, applyAccentStrong, accentStrongPref, applyBgAnim, bgAnimOn, isAndroid, type Accent } from "../core/chrome";
import { defaultEncoding } from "../core/encoding";
import { inlineImages } from "../core/format";
import { cmpVer, connKeyOf, connLabel } from "../core/conn";
import { setLang, getLang, t, LANGS, type Lang } from "../i18n";
import { nav } from "../ui/nav";
import type { SavedConn } from "../core/types";
import { renderAccesses, shareQr } from "./pairing";
import { checkUpdate, performClientUpdate, rcChannel } from "./update";
import {
  sounds,
  sndEnabled,
  sndTypeOn,
  sndVolume,
  sndOnlyUnfocused,
  sndPresetId,
  type SoundKind,
} from "./sounds";
import {
  notifications,
  notifEnabled,
  notifTypeOn,
  notifOnlyUnfocused,
} from "./notifications";
import type { AlertKind } from "./state-diff";
import { PRESETS } from "./sound-presets";
import { SttModelsPanel } from "./stt-models";
import { DlModelsPanel } from "./dl-models";
import type { Tts } from "./tts";

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
  /** Préférence « rendu Markdown » changée → MdView du viewer à
   *  re-synchroniser (injecté). */
  mdRenderChanged: (on: boolean) => void = () => {};
  /** Thème changé → les terminaux ouverts re-pick les couleurs (injecté). */
  themeChanged: () => void = () => {};
  /** Le panneau s'apprête à s'ouvrir : les autres overlays de #chat
   *  (vue connexion, carte) doivent se fermer — deux panneaux pleins
   *  empilés seraient inertes l'un sous l'autre (injecté par le
   *  bootstrap). */
  onOpening: () => void = () => {};

  /** Options stagées par id de contrôle (#190) : `apply` relit la
   *  valeur du contrôle au clic « Enregistrer », `reset` recharge le
   *  contrôle depuis la valeur persistée (« Annuler »), `preview`
   *  applique aussi au change (aperçu live thème/accent — reverté par
   *  reset+apply à l'annulation), `onChange` = effet immédiat sans
   *  persistance (preview sonore d'un preset). */
  private staged = new Map<
    string,
    { apply: () => void; reset: () => void; preview: boolean; onChange?: () => void }
  >();
  private dirtyIds = new Set<string>();

  private stage(
    id: string,
    o: {
      apply: () => void;
      reset: () => void;
      preview?: boolean;
      onChange?: () => void;
      event?: "onchange" | "oninput";
    },
  ) {
    const el = $(id) as HTMLInputElement | HTMLSelectElement;
    this.staged.set(id, {
      apply: o.apply,
      reset: o.reset,
      preview: o.preview === true,
      onChange: o.onChange,
    });
    el[o.event ?? "onchange"] = () => this.markDirty(id);
  }

  private markDirty(id: string) {
    const s = this.staged.get(id);
    if (!s) return;
    this.dirtyIds.add(id);
    if (s.preview) s.apply();
    s.onChange?.();
    this.syncDirty();
  }

  private syncDirty() {
    const d = this.dirtyIds.size > 0;
    ($("s-save") as HTMLButtonElement).disabled = !d;
    ($("s-cancel") as HTMLButtonElement).disabled = !d;
    $("s-dirty").hidden = !d;
  }

  /** « Enregistrer » : applique toutes les options modifiées d'un coup —
   *  les previews live sont rejouées (écriture idempotente). */
  private save() {
    for (const id of [...this.dirtyIds]) this.staged.get(id)?.apply();
    this.dirtyIds.clear();
    this.syncDirty();
  }

  /** « Annuler » : recharge les contrôles depuis les valeurs persistées
   *  et revert les aperçus live — plus rien de stagé ni d'appliqué.
   *  Appelé aussi à la fermeture (annulation silencieuse : jamais
   *  d'application partielle, #190). */
  private cancelStaged() {
    if (!this.dirtyIds.size) return;
    for (const id of [...this.dirtyIds]) {
      const s = this.staged.get(id);
      if (!s) continue;
      s.reset();
      if (s.preview) s.apply();
    }
    this.dirtyIds.clear();
    this.syncDirty();
    this.syncSndOpts();
    this.syncNotifOpts();
    this.syncSecStates();
  }
  /** Clé connKey de la connexion actuellement liée — pré-sélectionne le
   *  serveur dans le select « Appairer » (injecté par le bootstrap). */
  activeConnKey: () => string | null = () => null;

  /** Conns SSH du select « Appairer » — snapshot du dernier refresh ;
   *  la valeur de l'option choisie est son index dedans. */
  private pairConns: SavedConn[] = [];

  /** Modèles STT téléchargeables (Android only — le panneau interne
   *  no-op sur les autres plateformes). */
  private stt = new SttModelsPanel();
  /** Modèles TTS téléchargeables — même panneau générique, kind "tts". */
  private ttsModels = new DlModelsPanel({
    kind: "tts",
    hostId: "s-tts-models",
    secStateId: "s-st-tts",
  });
  /** Contrôleur TTS (boutons message + lecture auto) — injecté par le
   *  bootstrap ; la section n'est synchronisée que sur Android. */
  tts?: Tts;

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
    ($("s-accent-int") as HTMLSelectElement).value =
      accentStrongPref() ? "strong" : "soft";
    ($("s-bganim") as HTMLInputElement).checked = bgAnimOn();
    ($("s-lang") as HTMLSelectElement).value = getLang();
    ($("s-sendmode") as HTMLSelectElement).value = sendPref();
    ($("s-autoconn") as HTMLInputElement).checked =
      localStorage.getItem("ac-autoconnect") !== "0";
    if (!isWeb) void this.refreshPairConns();
    ($("s-images") as HTMLInputElement).checked = inlineImages();
    ($("s-rcchan") as HTMLInputElement).checked = rcChannel();
    ($("s-encoding") as HTMLSelectElement).value = defaultEncoding();
    ($("s-mdrender") as HTMLInputElement).checked =
      localStorage.getItem("ac-md-render") !== "0";
    ($("s-snd") as HTMLInputElement).checked = sndEnabled();
    for (const k of ["done", "question", "error"] as SoundKind[]) {
      ($(`s-snd-${k}`) as HTMLInputElement).checked = sndTypeOn(k);
      ($(`s-snd-preset-${k}`) as HTMLSelectElement).value = sndPresetId(k);
    }
    ($("s-snd-unfocused") as HTMLInputElement).checked = sndOnlyUnfocused();
    ($("s-snd-vol") as HTMLInputElement).value = String(
      Math.round(sndVolume() * 100),
    );
    $("s-snd-blocked").hidden = !sounds.blocked();
    this.syncSndOpts();
    // Notifications OS : canal Tauri seulement — bloc masqué en web.
    $("s-notif-block").hidden = isWeb;
    ($("s-notif") as HTMLInputElement).checked = notifEnabled();
    for (const k of ["done", "question", "error"] as AlertKind[]) {
      ($(`s-notif-${k}`) as HTMLInputElement).checked = notifTypeOn(k);
    }
    ($("s-notif-unfocused") as HTMLInputElement).checked = notifOnlyUnfocused();
    this.syncNotifOpts();
    void notifications
      .permBlocked()
      .then((blocked) => {
        $("s-notif-denied").hidden = !blocked || !notifEnabled();
      })
      .catch(() => {});
    $("s-update-status").textContent = "";
    ($("s-doupdate") as HTMLButtonElement).hidden = true;
    // Web : pas de reconnexion optionnelle (la conn unique est vitale) ni
    // d'update client (la SPA suit le binaire démon) → sections masquées.
    $("s-sec-conn").hidden = isWeb;
    $("s-sec-update").hidden = isWeb;
    // Voix on-device : Android natif uniquement (modèles dans filesDir
    // de l'app — ni desktop ni web SPA). STT + TTS partagent le gate.
    const voiceOk = isAndroid() && !isWeb;
    $("s-sec-stt").hidden = !voiceOk;
    $("s-sec-tts").hidden = !voiceOk;
    if (voiceOk) {
      void this.stt.refresh();
      void this.ttsModels.refresh();
      this.tts?.syncSettings();
    }
    void getVersion()
      .then((v) => {
        $("s-version").textContent = `v${v}`;
        this.syncSecStates();
      })
      .catch(() => {
        $("s-version").textContent = "";
        this.syncSecStates();
      });
    this.syncSecStates();
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
    // Modifications stagées non enregistrées → annulation silencieuse
    // (revert des aperçus live inclus) : jamais d'application partielle.
    this.cancelStaged();
    // L'inert est retiré même si le panneau avait déjà été masqué par un
    // autre chemin (early-return historique : un résiduel gelait #chat
    // sans cause visible — le watchdog core/modal filet de sécurité).
    const was = this.isOpen();
    $("settings-view").hidden = true;
    // Ne lever l'inert que si les autres overlays #chat sont aussi
    // fermés — sinon on ré-expose #input derrière conn-view/map-view/
    // components-view (frappes invisibles).
    if ($("conn-view").hidden && $("map-view").hidden && $("components-view").hidden) {
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

  /** Select « Appairer un appareil » : les conns sauvegardées ont pu
   *  changer depuis la dernière ouverture → refetch à chaque open().
   *  Seules les SSH sont listées (pair_resolve refuse les autres kinds). */
  private async refreshPairConns() {
    this.pairConns = (
      await invoke<SavedConn[]>("saved_connections").catch(
        () => [] as SavedConn[],
      )
    ).filter((c) => c.kind === "ssh");
    const sel = $("s-pair-conn") as HTMLSelectElement;
    sel.innerHTML = "";
    const active = this.activeConnKey();
    const android = isAndroid();
    let def = -1;
    this.pairConns.forEach((c, i) => {
      const o = document.createElement("option");
      o.value = String(i);
      const k = connKeyOf(c, android);
      o.textContent = connLabel(k);
      sel.append(o);
      if (k === active) def = i;
    });
    // Défaut : conn liée, sinon la première (la dernière utilisée —
    // connections.json garde la plus récente en tête, cf. autoConnect).
    sel.selectedIndex = def >= 0 ? def : 0;
    const none = this.pairConns.length === 0;
    sel.disabled = none;
    ($("s-pair-go") as HTMLButtonElement).disabled = none;
    $("s-pair-none").hidden = !none;
    $("s-pacc").hidden = none;
    void this.refreshPairAccess();
  }

  /** Liste « Accès appairés » du serveur choisi dans le select — relue à
   *  chaque changement ; erreurs ssh → état « indisponible » silencieux. */
  private async refreshPairAccess() {
    const c =
      this.pairConns[Number(($("s-pair-conn") as HTMLSelectElement).value)];
    if (!c) return;
    await renderAccesses(c, {
      ul: $("s-pacc-list"),
      status: $("s-pacc-state"),
    });
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
          const prev = go.textContent;
          go.textContent = t("update_installing", { v: remote });
          try {
            await performClientUpdate(remote);
          } finally {
            go.disabled = false;
            go.textContent = prev;
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
    this.stt.init();
    this.ttsModels.init();
    this.tts?.bindSettings();
    $("tb-settings").onclick = () => this.toggle();
    $("settings-close").onclick = () => this.close();
    // Échap ferme le panneau (les <dialog> l'avaient nativement, pas un div)
    // — sauf si un dialogue est ouvert par-dessus (About…) : Échap = lui.
    window.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && this.isOpen() && !document.querySelector("dialog[open]"))
        this.close();
    });

    // Options stagées (#190) : appliquées seulement au clic
    // « Enregistrer » — les cosmétiques (thème/accent/fond animé)
    // gardent un aperçu live, reverté à l'annulation.
    this.stage("s-theme", {
      apply: () => {
        applyTheme(
          ($("s-theme") as HTMLSelectElement).value as "dark" | "light",
        );
        this.themeChanged();
        this.syncSecStates();
      },
      reset: () => {
        ($("s-theme") as HTMLSelectElement).value =
          document.documentElement.classList.contains("light")
            ? "light"
            : "dark";
      },
      preview: true,
    });
    this.stage("s-accent", {
      apply: () => {
        applyAccent(($("s-accent") as HTMLSelectElement).value as Accent);
        this.themeChanged();
        this.syncSecStates();
      },
      reset: () => {
        ($("s-accent") as HTMLSelectElement).value = accentPref();
      },
      preview: true,
    });
    this.stage("s-accent-int", {
      apply: () => {
        applyAccentStrong(
          ($("s-accent-int") as HTMLSelectElement).value === "strong",
        );
        this.themeChanged();
        this.syncSecStates();
      },
      reset: () => {
        ($("s-accent-int") as HTMLSelectElement).value =
          accentStrongPref() ? "strong" : "soft";
      },
      preview: true,
    });
    this.stage("s-bganim", {
      apply: () =>
        applyBgAnim(($("s-bganim") as HTMLInputElement).checked),
      reset: () => {
        ($("s-bganim") as HTMLInputElement).checked = bgAnimOn();
      },
      preview: true,
    });
    // Options du select langue construites depuis LANGS (un bloc de
    // dictionnaire = une entrée) — libellés natifs, pas de data-i18n :
    // chaque langue se nomme dans sa propre langue.
    const langSel = $("s-lang") as HTMLSelectElement;
    for (const l of LANGS) {
      const opt = document.createElement("option");
      opt.value = l.id;
      opt.textContent = l.label;
      langSel.append(opt);
    }
    this.stage("s-lang", {
      apply: () => {
        setLang(($("s-lang") as HTMLSelectElement).value as Lang);
        this.langChanged();
        // setLang → applyStatic a retraduit les options → lire leurs
        // libellés après coup pour les résumés de section.
        this.syncSecStates();
      },
      reset: () => {
        ($("s-lang") as HTMLSelectElement).value = getLang();
      },
    });
    this.stage("s-sendmode", {
      apply: () => {
        localStorage.setItem(
          "ac-sendmode",
          ($("s-sendmode") as HTMLSelectElement).value === "steer"
            ? "steer"
            : "queue",
        );
        this.sendModeChanged();
        this.syncSecStates();
      },
      reset: () => {
        ($("s-sendmode") as HTMLSelectElement).value = sendPref();
      },
    });
    this.stage("s-autoconn", {
      apply: () => {
        localStorage.setItem(
          "ac-autoconnect",
          ($("s-autoconn") as HTMLInputElement).checked ? "1" : "0",
        );
        this.syncSecStates();
      },
      reset: () => {
        ($("s-autoconn") as HTMLInputElement).checked =
          localStorage.getItem("ac-autoconnect") !== "0";
      },
    });
    // « Pair… » : même modale que l'icône QR de la vue connexions —
    // shareQr pré-remplit l'endpoint (pair_resolve) et liste les
    // appareils déjà appairés au serveur choisi.
    ($("s-pair-go") as HTMLButtonElement).onclick = () => {
      const c =
        this.pairConns[
          Number(($("s-pair-conn") as HTMLSelectElement).value)
        ];
      if (!c) return;
      void shareQr(c)
        .then(() => {
          // La modale est ouverte — un appairage peut ajouter un
          // appareil : liste rechargée à sa fermeture. (Branché après
          // showModal : si shareQr échoue avant, pas de listener armé.)
          ($("dlg-qr") as HTMLDialogElement).addEventListener(
            "close",
            () => void this.refreshPairAccess(),
            { once: true },
          );
        })
        .catch((e) => notify("error", String(e)));
    };
    ($("s-pair-conn") as HTMLSelectElement).onchange = () =>
      void this.refreshPairAccess();
    ($("s-pacc-ref") as HTMLButtonElement).onclick = () =>
      void this.refreshPairAccess();
    this.stage("s-images", {
      apply: () =>
        localStorage.setItem(
          "ac-inline-images",
          ($("s-images") as HTMLInputElement).checked ? "1" : "0",
        ),
      reset: () => {
        ($("s-images") as HTMLInputElement).checked = inlineImages();
      },
    });
    this.stage("s-rcchan", {
      apply: () =>
        localStorage.setItem(
          "ac-rcchannel",
          ($("s-rcchan") as HTMLInputElement).checked ? "1" : "0",
        ),
      reset: () => {
        ($("s-rcchan") as HTMLInputElement).checked = rcChannel();
      },
    });
    this.stage("s-encoding", {
      apply: () => {
        localStorage.setItem(
          "ac-encoding",
          ($("s-encoding") as HTMLSelectElement).value,
        );
        this.syncSecStates();
      },
      reset: () => {
        ($("s-encoding") as HTMLSelectElement).value = defaultEncoding();
      },
    });
    this.stage("s-mdrender", {
      apply: () => {
        const on = ($("s-mdrender") as HTMLInputElement).checked;
        localStorage.setItem("ac-md-render", on ? "1" : "0");
        this.mdRenderChanged(on);
      },
      reset: () => {
        ($("s-mdrender") as HTMLInputElement).checked =
          localStorage.getItem("ac-md-render") !== "0";
      },
    });
    this.stage("s-snd", {
      apply: () => {
        localStorage.setItem(
          "ac-snd",
          ($("s-snd") as HTMLInputElement).checked ? "1" : "0",
        );
        this.syncSndOpts();
        this.syncSecStates();
      },
      reset: () => {
        ($("s-snd") as HTMLInputElement).checked = sndEnabled();
      },
    });
    for (const k of ["done", "question", "error"] as SoundKind[]) {
      this.stage(`s-snd-${k}`, {
        apply: () =>
          localStorage.setItem(
            `ac-snd-${k}`,
            ($(`s-snd-${k}`) as HTMLInputElement).checked ? "1" : "0",
          ),
        reset: () => {
          ($(`s-snd-${k}`) as HTMLInputElement).checked = sndTypeOn(k);
        },
      });
      // Options des presets construites depuis la bibliothèque — portent
      // data-i18n, retraduites par applyStatic au changement de langue.
      const sel = $(`s-snd-preset-${k}`) as HTMLSelectElement;
      for (const p of PRESETS[k]) {
        const opt = document.createElement("option");
        opt.value = p.id;
        opt.dataset.i18n = p.label;
        opt.textContent = t(p.label);
        sel.append(opt);
      }
      // Choisir un preset le joue aussitôt — aperçu sonore immédiat du
      // choix (onChange sans persistance ; le setItem attend le save).
      this.stage(`s-snd-preset-${k}`, {
        apply: () =>
          localStorage.setItem(
            `ac-snd-preset-${k}`,
            ($(`s-snd-preset-${k}`) as HTMLSelectElement).value,
          ),
        reset: () => {
          ($(`s-snd-preset-${k}`) as HTMLSelectElement).value =
            sndPresetId(k);
        },
        onChange: () => sounds.test(k),
      });
      // « Tester » joue même tout désactivé — le geste débloque l'autoplay
      // de la webview (Android) en plus de donner un aperçu volume réel.
      ($(`s-snd-test-${k}`) as HTMLButtonElement).onclick = () =>
        sounds.test(k);
    }
    this.stage("s-snd-unfocused", {
      apply: () =>
        localStorage.setItem(
          "ac-snd-unfocused",
          ($("s-snd-unfocused") as HTMLInputElement).checked ? "1" : "0",
        ),
      reset: () => {
        ($("s-snd-unfocused") as HTMLInputElement).checked =
          sndOnlyUnfocused();
      },
    });
    // Activation explicite du canal → demande de permission OS au save
    // (geste utilisateur = meilleur contexte pour le prompt natif).
    this.stage("s-notif", {
      apply: () => {
        const on = ($("s-notif") as HTMLInputElement).checked;
        localStorage.setItem("ac-notif", on ? "1" : "0");
        if (on) {
          void notifications.ensurePermission(true).then((ok) => {
            $("s-notif-denied").hidden = ok;
          });
        } else {
          $("s-notif-denied").hidden = true;
        }
        this.syncNotifOpts();
        this.syncSecStates();
      },
      reset: () => {
        ($("s-notif") as HTMLInputElement).checked = notifEnabled();
      },
    });
    for (const k of ["done", "question", "error"] as AlertKind[]) {
      this.stage(`s-notif-${k}`, {
        apply: () =>
          localStorage.setItem(
            `ac-notif-${k}`,
            ($(`s-notif-${k}`) as HTMLInputElement).checked ? "1" : "0",
          ),
        reset: () => {
          ($(`s-notif-${k}`) as HTMLInputElement).checked = notifTypeOn(k);
        },
      });
    }
    this.stage("s-notif-unfocused", {
      apply: () =>
        localStorage.setItem(
          "ac-notif-unfocused",
          ($("s-notif-unfocused") as HTMLInputElement).checked ? "1" : "0",
        ),
      reset: () => {
        ($("s-notif-unfocused") as HTMLInputElement).checked =
          notifOnlyUnfocused();
      },
    });
    ($("s-notif-test") as HTMLButtonElement).onclick = () =>
      notifications.test();
    this.stage("s-snd-vol", {
      event: "oninput",
      apply: () => {
        localStorage.setItem(
          "ac-snd-vol",
          ($("s-snd-vol") as HTMLInputElement).value,
        );
        this.syncSecStates();
      },
      reset: () => {
        ($("s-snd-vol") as HTMLInputElement).value = String(
          Math.round(sndVolume() * 100),
        );
      },
    });
    ($("s-checkupdate") as HTMLButtonElement).onclick = () => this.runUpdateCheck();
    // « Voir les logs » : la visionneuse client.log partagée — la télémétrie
    // update (check → install) y est lisible sans quitter les paramètres.
    bindLogView($("s-logs") as HTMLDetailsElement);
    // Staging (#190) : « Enregistrer » applique tout, « Annuler »
    // restaure — inactifs tant que rien n'est modifié.
    ($("s-save") as HTMLButtonElement).onclick = () => this.save();
    ($("s-cancel") as HTMLButtonElement).onclick = () => this.cancelStaged();
    this.syncDirty();
  }

  /** Résumé d'état à droite du titre de chaque section — la valeur
   *  courante reste lisible quand la section est repliée. */
  private syncSecStates() {
    const opt = (id: string) =>
      ($(id) as HTMLSelectElement).selectedOptions[0]?.textContent?.trim() ??
      "";
    $("s-st-appearance").textContent =
      `${opt("s-theme")} · ${opt("s-accent")} · ${opt("s-accent-int")}`;
    $("s-st-chat").textContent = opt("s-sendmode");
    const sndOn = ($("s-snd") as HTMLInputElement).checked;
    const notifOn = !isWeb && ($("s-notif") as HTMLInputElement).checked;
    $("s-st-notif").textContent = [
      sndOn ? `${t("st_on")} · ${($("s-snd-vol") as HTMLInputElement).value}%` : t("st_off"),
      ...(isWeb ? [] : [`${t("notif_short")} ${notifOn ? t("st_on") : t("st_off")}`]),
    ].join(" · ");
    $("s-st-conn").textContent = ($("s-autoconn") as HTMLInputElement).checked
      ? t("st_on")
      : t("st_off");
    $("s-st-update").textContent = $("s-version").textContent;
    $("s-st-files").textContent = opt("s-encoding");
  }

  /** Master off → les sous-options sons sont grisées/inertes — sauf les
   *  boutons « Tester », qui restent cliquables (preview au volume réglé
   *  et déblocage de l'autoplay par le geste utilisateur). */
  private syncSndOpts() {
    const off = !($("s-snd") as HTMLInputElement).checked;
    $("s-snd-opts").classList.toggle("off", off);
    for (const el of $("s-snd-opts").querySelectorAll<
      HTMLInputElement | HTMLSelectElement
    >("input, select")) {
      el.disabled = off;
    }
  }

  /** Même grisage pour le bloc notifications — sauf le bouton « Tester »,
   *  cliquable même canal coupé (le test vérifie permission + rendu OS). */
  private syncNotifOpts() {
    const off = !($("s-notif") as HTMLInputElement).checked;
    $("s-notif-opts").classList.toggle("off", off);
    for (const el of $("s-notif-opts").querySelectorAll<
      HTMLInputElement | HTMLButtonElement
    >("input, button")) {
      el.disabled = off && el.id !== "s-notif-test";
    }
  }
}
