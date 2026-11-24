// ---------------------------------------------------------------------------
// Panneau des paramètres : rendu dans la zone de discussion (overlay plein
// #chat, pas un dialogue modal) — préférences persistées en localStorage.
// Navigation (rail, recherche, contrôles visuels) : ./settings-nav.ts ;
// mises à jour : fenêtre dédiée (./updates-panel.ts), ouverte d'ici.
// ---------------------------------------------------------------------------

import { invoke, getVersion, isWeb } from "../core/bridge";
import { $, esc, notify } from "../core/dom";
import { bindLogView } from "../core/logview";
import { applyTheme, themePref, applyAccent, stageAccent, accentPref, applyAccentStrong, accentStrongPref, applyBgAnim, bgAnimPrefOn, isAndroid, type Accent } from "../core/chrome";
import { defaultEncoding } from "../core/encoding";
import { imgMode, imgTrustedHosts, refreshImgPolicy } from "../core/format";
import { connKeyOf, connLabel } from "../core/conn";
import { askUnsaved } from "../core/unsaved";
import { setLang, getLang, t, LANGS, type Lang } from "../i18n";
import { nav } from "../ui/nav";
import type { SavedConn } from "../core/types";
import { renderAccesses, shareQr } from "./pairing";
import { rcChannel } from "./update";
import { SettingsNav, settingName, type ServerLink } from "./settings-nav";
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
import { setTelemetryPref, telemetryPref } from "./telemetry";

/** Préférence d'envoi : "queue" (file FIFO, défaut) | "steer" (injection
 *  mid-turn quand l'agent tourne ; idle → envoi normal côté démon). */
export type SendPref = "queue" | "steer";

/** Une option stagée (#190) — `apply` persiste au « Enregistrer »,
 *  `reset` relit la valeur enregistrée, `preview` applique sans
 *  persister au change (cosmétiques), `onChange` = effet immédiat
 *  non persisté (écoute d'un preset). */
export interface StagedOpts {
  apply: () => void;
  reset: () => void;
  preview?: () => void;
  onChange?: () => void;
  event?: "onchange" | "oninput";
}
/** Enregistreur d'option stagée — passé aux sous-panneaux (STT/TTS). */
export type Stager = (id: string, o: StagedOpts) => void;
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
  /** Connexion liée (en-tête du groupe « Serveur » du rail) — injecté. */
  server: () => { label: string; up: boolean } | null = () => null;
  /** Vues serveur du rail (Composants, Orchestrateur…) — injecté. */
  serverLinks: () => ServerLink[] = () => [];
  /** « Vérifier les mises à jour » : fenêtre Mises à jour (injecté). */
  openUpdates: (anchor?: HTMLElement) => void = () => {};

  private snav = new SettingsNav({
    server: () => this.server(),
    serverLinks: () => this.serverLinks(),
    dirtyIds: () => this.dirtyIds,
    leave: () => this.requestClose(),
  });

  /** Options stagées par id de contrôle (#190) : `apply` relit la
   *  valeur du contrôle au clic « Enregistrer » (persistance incluse),
   *  `reset` recharge le contrôle depuis la valeur persistée
   *  (« Annuler »), `preview` applique au change SANS persister
   *  (aperçu live thème/accent — reverté par reset+preview à
   *  l'annulation), `onChange` = effet immédiat sans persistance
   *  (preview sonore d'un preset). */
  private staged = new Map<string, StagedOpts>();
  private dirtyIds = new Set<string>();

  /** Enregistreur exposé aux sous-panneaux (STT/TTS) — leurs contrôles
   *  entrent dans la même boucle Enregistrer/Annuler. */
  readonly stager: Stager = (id, o) => this.stage(id, o);

  private stage(id: string, o: StagedOpts) {
    const el = $(id) as HTMLInputElement | HTMLSelectElement;
    this.staged.set(id, o);
    el[o.event ?? "onchange"] = () => this.markDirty(id);
  }

  private markDirty(id: string) {
    const s = this.staged.get(id);
    if (!s) return;
    this.dirtyIds.add(id);
    s.preview?.();
    s.onChange?.();
    this.syncDirty();
  }

  /** Barre d'enregistrement : visible seulement avec des modifications
   *  en attente — leur nombre et leurs noms, plus la pastille du rail. */
  private syncDirty() {
    const d = this.dirtyIds.size > 0;
    ($("s-save") as HTMLButtonElement).disabled = !d;
    ($("s-cancel") as HTMLButtonElement).disabled = !d;
    $("s-dirty").hidden = !d;
    $("s-savebar").hidden = !d;
    if (d) {
      const names = [...new Set([...this.dirtyIds].map(settingName))];
      const n = names.length;
      $("s-dirty-msg").innerHTML =
        `<b>${esc(t(n === 1 ? "s_unsaved_1" : "s_unsaved_n", { n }))}</b> · ${esc(names.join(", "))}`;
    }
    this.snav.render();
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
      s.preview?.();
    }
    this.dirtyIds.clear();
    this.syncDirty();
    this.syncSndOpts();
    this.syncNotifOpts();
    this.syncSecStates();
    this.snav.syncVisual();
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
    ($("s-theme") as HTMLSelectElement).value = themePref();
    ($("s-accent") as HTMLSelectElement).value = accentPref();
    ($("s-accent-int") as HTMLSelectElement).value =
      accentStrongPref() ? "strong" : "soft";
    ($("s-bganim") as HTMLInputElement).checked = bgAnimPrefOn();
    ($("s-lang") as HTMLSelectElement).value = getLang();
    ($("s-sendmode") as HTMLSelectElement).value = sendPref();
    ($("s-autoconn") as HTMLInputElement).checked =
      localStorage.getItem("ac-autoconnect") !== "0";
    if (!isWeb) void this.refreshPairConns();
    ($("s-images") as HTMLSelectElement).value = imgMode();
    ($("s-img-hosts") as HTMLInputElement).value =
      imgTrustedHosts().join(", ");
    ($("s-rcchan") as HTMLInputElement).checked = rcChannel();
    ($("s-encoding") as HTMLSelectElement).value = defaultEncoding();
    ($("s-mdrender") as HTMLInputElement).checked =
      localStorage.getItem("ac-md-render") !== "0";
    ($("s-telemetry") as HTMLInputElement).checked = telemetryPref();
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
    // Notifications OS : canal Tauri seulement — colonne masquée en web.
    $("s-alerts").classList.toggle("no-os", isWeb);
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
    // Web : pas de reconnexion optionnelle (la conn unique est vitale) ni
    // de canal client (la SPA suit le binaire démon) → lignes masquées ;
    // « Vérifier » reste (fenêtre Mises à jour = update du démon).
    $("s-sec-conn").hidden = isWeb;
    $("s-autoconn-row").hidden = isWeb;
    $("s-rcchan-row").hidden = isWeb;
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
    this.snav.syncVisual();
    this.snav.render();
    const view = $("settings-view");
    view.hidden = false;
    this.setChatInert(true);
    // Le focus peut rester dans #input (ouverture par clic programmatique
    // depuis la sheet ⋮) → frappes invisibles envoyées à l'agent. Le panneau
    // prend le focus : tabindex=-1, sans scroll.
    view.focus({ preventScroll: true });
    nav.push("settings");
  }

  /** Fermeture demandée par l'utilisateur (✕, Échap, icône, Ctrl+,,
   *  back Android, lien du rail) : avec des modifications en attente,
   *  demande Enregistrer / Ignorer / Rester. `false` = on reste. */
  async requestClose(): Promise<boolean> {
    if (!this.isOpen()) return true;
    if (this.dirtyIds.size) {
      const c = await askUnsaved(t("unsaved_settings_msg"));
      if (c === "stay") return false;
      if (c === "save") this.save();
    }
    this.close();
    return true;
  }

  /** Ouvre le panneau sur une section (rail desktop, sous-écran Android). */
  openSection(id: string) {
    this.open();
    this.snav.select(id);
  }

  close() {
    // Chemin programmatique (une autre vue s'ouvre) : modifications
    // stagées annulées (aperçus live revertés) — jamais d'application
    // partielle. Les fermetures utilisateur passent par requestClose.
    this.cancelStaged();
    this.snav.resetSearch();
    // L'inert est retiré même si le panneau avait déjà été masqué par un
    // autre chemin (early-return historique : un résiduel gelait #chat
    // sans cause visible — le watchdog core/modal filet de sécurité).
    const was = this.isOpen();
    $("settings-view").hidden = true;
    // Ne lever l'inert que si les autres overlays #chat sont aussi
    // fermés — sinon on ré-expose #input derrière conn-view/map-view/
    // components-view (frappes invisibles).
    if ($("conn-view").hidden && $("map-view").hidden && $("components-view").hidden && $("logs-view").hidden && $("plugin-view").hidden) {
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
    if (this.isOpen()) void this.requestClose();
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
    this.syncSecStates();
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

  init() {
    this.stt.init(this.stager);
    this.ttsModels.init();
    this.tts?.bindSettings(this.stager);
    $("tb-settings").onclick = () => this.toggle();
    $("settings-close").onclick = () => void this.requestClose();
    // Échap ferme le panneau (les <dialog> l'avaient nativement, pas un div)
    // — sauf si un dialogue est ouvert par-dessus (About…) : Échap = lui.
    // Ctrl/Cmd+, : raccourci desktop usuel des préférences.
    window.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && this.isOpen() && !document.querySelector("dialog[open]"))
        void this.requestClose();
      else if ((e.ctrlKey || e.metaKey) && !e.altKey && e.key === "," && !isAndroid()) {
        e.preventDefault();
        this.toggle();
      }
    });
    // Back Android avec des modifications en attente : même question que
    // le ✕ — la fermeture n'a lieu qu'après la réponse.
    nav.addBackGuard((id) => {
      if (id !== "settings" || !this.dirtyIds.size) return true;
      void this.requestClose();
      return false;
    });
    this.snav.init();

    // Options stagées (#190) : `apply` (persistant) n'a lieu qu'au clic
    // « Enregistrer » — les cosmétiques gardent un aperçu live : le
    // `preview` applique SANS persister, et « Annuler »/fermeture fait
    // reset (relecture de la valeur enregistrée) puis preview pour
    // restaurer l'état visuel persisté.
    const chromePreview = (persist: boolean) => {
      applyTheme(
        ($("s-theme") as HTMLSelectElement).value as "dark" | "light",
        persist,
      );
      this.themeChanged();
      this.syncSecStates();
    };
    this.stage("s-theme", {
      apply: () => chromePreview(true),
      reset: () => {
        ($("s-theme") as HTMLSelectElement).value = themePref();
      },
      preview: () => chromePreview(false),
    });
    // L'aperçu d'accent passe par l'override stagé relu par
    // applyConnChrome (themeChanged → reapplyChrome) — un applyAccent
    // direct serait défait par la ré-application de la pref persistée.
    const accentPreview = () => {
      const v = ($("s-accent") as HTMLSelectElement).value as Accent;
      // Null quand le choix rejoint la pref (annulation incluse) — pas
      // d'override résiduel après revert.
      stageAccent(v === accentPref() ? null : v);
      this.themeChanged();
      this.syncSecStates();
    };
    this.stage("s-accent", {
      apply: () => {
        applyAccent(($("s-accent") as HTMLSelectElement).value as Accent);
        stageAccent(null);
        this.themeChanged();
        this.syncSecStates();
      },
      reset: () => {
        ($("s-accent") as HTMLSelectElement).value = accentPref();
      },
      preview: accentPreview,
    });
    const accentIntPreview = (persist: boolean) => {
      applyAccentStrong(
        ($("s-accent-int") as HTMLSelectElement).value === "strong",
        persist,
      );
      this.themeChanged();
      this.syncSecStates();
    };
    this.stage("s-accent-int", {
      apply: () => accentIntPreview(true),
      reset: () => {
        ($("s-accent-int") as HTMLSelectElement).value =
          accentStrongPref() ? "strong" : "soft";
      },
      preview: () => accentIntPreview(false),
    });
    this.stage("s-bganim", {
      apply: () =>
        applyBgAnim(($("s-bganim") as HTMLInputElement).checked),
      reset: () => {
        ($("s-bganim") as HTMLInputElement).checked = bgAnimPrefOn();
      },
      preview: () =>
        applyBgAnim(($("s-bganim") as HTMLInputElement).checked, false),
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
        this.snav.render();
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
      apply: () => {
        localStorage.setItem(
          "ac-inline-images",
          ($("s-images") as HTMLSelectElement).value,
        );
        this.syncSecStates();
        // Le contenu déjà rendu n'est pas re-généré : la politique est
        // ré-appliquée en place (never retire, always/whitelist ouvre).
        refreshImgPolicy();
      },
      reset: () => {
        ($("s-images") as HTMLSelectElement).value = imgMode();
      },
    });
    // Whitelist normalisée au save : csv → minuscules, sans doublons —
    // le stockage est la forme déjà propre, la saisie reste libre.
    this.stage("s-img-hosts", {
      apply: () => {
        const hosts = ($("s-img-hosts") as HTMLInputElement).value
          .split(",")
          .map((h) => h.trim().toLowerCase())
          .filter(Boolean);
        localStorage.setItem("ac-img-hosts", [...new Set(hosts)].join(", "));
        // Un hôte nouvellement fiable ouvre ses placeholders sans clic.
        refreshImgPolicy();
      },
      reset: () => {
        ($("s-img-hosts") as HTMLInputElement).value =
          imgTrustedHosts().join(", ");
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
    this.stage("s-telemetry", {
      apply: () =>
        setTelemetryPref(($("s-telemetry") as HTMLInputElement).checked),
      reset: () => {
        ($("s-telemetry") as HTMLInputElement).checked = telemetryPref();
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
      // Grisage des sous-options sans attendre Enregistrer — retour
      // visuel immédiat du master (annulable comme tout le staging).
      preview: () => this.syncSndOpts(),
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
      // choix affiché (le preset/volume stagés sont passés au test ;
      // le setItem attend le save).
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
        onChange: () =>
          sounds.test(
            k,
            ($(`s-snd-preset-${k}`) as HTMLSelectElement).value,
            Number(($("s-snd-vol") as HTMLInputElement).value) / 100,
          ),
      });
      // « Tester » joue même tout désactivé — le geste débloque l'autoplay
      // de la webview (Android). L'aperçu reflète les choix affichés
      // (preset/volume stagés), pas seulement la config enregistrée.
      ($(`s-snd-test-${k}`) as HTMLButtonElement).onclick = () =>
        sounds.test(
          k,
          ($(`s-snd-preset-${k}`) as HTMLSelectElement).value,
          Number(($("s-snd-vol") as HTMLInputElement).value) / 100,
        );
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
      preview: () => this.syncNotifOpts(),
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
    ($("s-checkupdate") as HTMLButtonElement).onclick = (e) =>
      this.openUpdates(e.currentTarget as HTMLElement);
    // Journal client (section Diagnostic) : la visionneuse client.log
    // partagée — la télémétrie update (check → install) y est lisible.
    bindLogView($("s-logs") as HTMLDetailsElement);
    // Valeur du volume affichée à côté du curseur (le staging garde
    // `oninput` — un listener séparé ne l'écrase pas).
    $("s-snd-vol").addEventListener("input", () => this.syncVolLabel());
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
    $("s-st-general").textContent = [opt("s-lang"), $("s-version").textContent]
      .filter(Boolean)
      .join(" · ");
    $("s-st-appearance").textContent =
      `${opt("s-theme")} · ${opt("s-accent")} · ${opt("s-accent-int")}`;
    $("s-st-chat").textContent = `${opt("s-sendmode")} · ${opt("s-images")}`;
    const sndOn = ($("s-snd") as HTMLInputElement).checked;
    const notifOn = !isWeb && ($("s-notif") as HTMLInputElement).checked;
    $("s-st-notif").textContent = [
      sndOn ? `${t("st_on")} · ${($("s-snd-vol") as HTMLInputElement).value}%` : t("st_off"),
      ...(isWeb ? [] : [`${t("notif_short")} ${notifOn ? t("st_on") : t("st_off")}`]),
    ].join(" · ");
    $("s-st-conn").textContent = this.pairConns.length
      ? String(this.pairConns.length)
      : "";
    $("s-st-files").textContent = opt("s-encoding");
    $("s-st-diag").textContent = ($("s-telemetry") as HTMLInputElement)
      .checked
      ? t("st_on")
      : t("st_off");
  }

  /** Master « Son » off → cases et presets de la colonne grisés/inertes
   *  — sauf les boutons « Tester », qui restent cliquables (preview au
   *  volume réglé et déblocage de l'autoplay par le geste utilisateur). */
  private syncSndOpts() {
    const off = !($("s-snd") as HTMLInputElement).checked;
    for (const cell of document.querySelectorAll<HTMLElement>("#s-alerts .mx-snd")) {
      cell.classList.toggle("off", off);
      for (const el of cell.querySelectorAll<HTMLInputElement | HTMLSelectElement>(
        "input, select",
      )) {
        el.disabled = off;
      }
    }
    this.syncVolLabel();
  }

  private syncVolLabel() {
    $("s-snd-volv").textContent = `${($("s-snd-vol") as HTMLInputElement).value} %`;
  }

  /** Même grisage pour la colonne notifications système — le bouton
   *  « Envoyer un test » (en-tête) reste cliquable même canal coupé. */
  private syncNotifOpts() {
    const off = !($("s-notif") as HTMLInputElement).checked;
    for (const cell of document.querySelectorAll<HTMLElement>("#s-alerts .mx-os")) {
      cell.classList.toggle("off", off);
      for (const el of cell.querySelectorAll<HTMLInputElement>("input")) {
        el.disabled = off;
      }
    }
  }
}
