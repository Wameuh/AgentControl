// ---------------------------------------------------------------------------
// Mise à jour du client : check au boot (chip "⬆ vX" dans la titlebar),
// install .deb desktop / ouverture APK Android, popup "client périmé"
// quand le démon est plus récent (minor).
// ---------------------------------------------------------------------------

import { invoke, getVersion, isWeb } from "../core/bridge";
import { $, esc, logUi, notify } from "../core/dom";
import { codedErrorMsg } from "../core/errcode";
import { icon } from "../ui/icons";
import { isAndroid, shellOpen } from "../core/chrome";
import { cmpVer, connKeyOf, connLabel, daemonParams } from "../core/conn";
import { call } from "../core/rpc";
import { t } from "../i18n";
import { confirmPair } from "./pairing";
import type { SavedConn, SessionMeta } from "../core/types";

/** Canal de maj opt-in : inclure les pré-releases rc (settings —
 *  tester la release candidate avant son merge). */
export const rcChannel = () =>
  localStorage.getItem("ac-rcchannel") === "1";

// Version distante : fetch direct (pas d'outil local), fallback commande Rust.
// releases/latest exclut les prereleases → canal rc = liste + max semver.
// Fetch bornés : un réseau en blackhole ferait popper dlg-outdated (modal,
// page inerte) des dizaines de secondes après l'action — incompréhensible.
const FETCH_TIMEOUT = 15_000;

export async function remoteVersion(): Promise<string> {
  if (rcChannel()) {
    try {
      const r = await fetch(
        "https://api.github.com/repos/Wameuh/AgentControl/releases?per_page=20",
        { signal: AbortSignal.timeout(FETCH_TIMEOUT) },
      );
      if (r.ok) {
        const rels = (await r.json()) as {
          tag_name?: string;
          draft?: boolean;
        }[];
        // La liste est triée par date, pas par semver → max explicite.
        // Stable ET rc confondues : la finale > rc la remplace sitôt publiée.
        let best = "";
        for (const rel of rels) {
          const tag = rel.tag_name?.replace(/^v/, "") ?? "";
          if (tag && !rel.draft && cmpVer(tag, best) > 0) best = tag;
        }
        if (best) return best;
      }
    } catch {
      /* réseau → repli canal stable */
    }
  }
  try {
    const r = await fetch(
      "https://api.github.com/repos/Wameuh/AgentControl/releases/latest",
      { signal: AbortSignal.timeout(FETCH_TIMEOUT) },
    );
    if (r.ok) {
      const j = (await r.json()) as { tag_name?: string };
      if (j.tag_name) return j.tag_name.replace(/^v/, "");
    }
  } catch {
    /* CSP/réseau → fallback invoke */
  }
  return invoke<string>("check_update", { includeRc: rcChannel() });
}

/// `app_signing` (plugin ac-device → PackageManager) rend le verdict :
/// "impossible" = debuggable ou signataire ≠ certificat release.
async function androidReinstallRisk(): Promise<boolean> {
  const info = await invoke<{ update_in_place?: string }>("app_signing")
    .catch(() => null);
  return info?.update_in_place === "impossible";
}

/// Update du client vers `tag` : desktop = installeur auto
/// (`update_client`) ; Android = `update_client` aussi — le backend
/// télécharge l'APK dans le cache de l'app, vérifie l'intégrité de la
/// release (SHA256SUMS signé minisign + sha256, I1) puis le plugin
/// ac-device lance l'installeur système (FileProvider + intent ; la
/// permission « apps inconnues » peut être demandée par Android —
/// statut `permission_required`). « Install unknown apps » devient
/// une permission de l'APP (plus du navigateur) : Android la demande
/// une fois, un refus = échec côté installeur invisible pour nous —
/// d'où le toast « rouvrez l'app » (le process peut être tué par le
/// remplacement, Android ne relance jamais l'app). L'APK release est
/// signé par l'upload keystore (#200) → update in-place possible
/// UNIQUEMENT si l'app installée porte la même signature ; un
/// appareil sur build debug-signé (rc pré-#200 ou build --debug) ne
/// peut pas upgrader → uninstall → perte des accès (Keystore non
/// exportables, #160) → re-pairing QR obligatoire.
export async function performClientUpdate(tag: string) {
  if (isAndroid()) {
    // Garde-fou uninstall : si l'app installée ne peut pas upgrader
    // in-place (signature ≠ release), le chemin impose une
    // désinstallation qui efface TOUTES les données (allowBackup=false,
    // #160 — les secrets Keystore ne sont pas exportables) : l'utilisateur
    // doit le savoir AVANT de télécharger, pas après l'échec d'install.
    if (await androidReinstallRisk()) {
      const ok = await confirmPair(
        t("upd_android_h"),
        t("upd_android_warn"),
        t("upd_android_go"),
      );
      if (!ok) return;
    }
    notify("info", t("update_installing", { v: tag }));
    let status = "";
    try {
      status = await invoke<string>("update_client", { tag });
    } catch (e) {
      const reason = codedErrorMsg(e, "update");
      notify("error", t("update_failed_reason", { reason }));
      // Repli « page releases » : l'APK s'y télécharge à la main —
      // l'erreur backend cite déjà la cause exacte.
      shellOpen(`https://github.com/Wameuh/AgentControl/releases/tag/v${tag}`);
      return;
    }
    if (status === "permission_required") {
      // Android a ouvert les réglages « sources inconnues » de l'app :
      // l'utilisateur accorde puis relance la mise à jour (l'APK est
      // retéléchargé et revérifié — le cache est purgé à chaque fois).
      notify("warn", t("update_android_permission"));
      return;
    }
    // L'installeur système est ouvert : le process meurt avec le
    // remplacement de l'APK et Android ne permet pas à une app
    // remplacée de se relancer seule — on guide explicitement.
    notify("info", t("update_android_reopen"));
    return;
  }
  // notify() verse déjà au client.log — pas de logUi en plus (doublon).
  notify("info", t("update_installing", { v: tag }));
  try {
    await invoke("update_client", { tag });
  } catch (e) {
    const reason = String(e);
    notify("error", t("update_failed_reason", {
      reason: codedErrorMsg(e, "update"),
    }));
    // Repli « page releases » quand le backend le désigne (hors bundle
    // .app, build portable…) — l'erreur affiche déjà la cause exacte.
    if (/releases? page|update-failed:(manual|writable)/.test(reason)) {
      shellOpen(`https://github.com/Wameuh/AgentControl/releases/tag/v${tag}`);
    }
    return;
  }
  // Succès = le backend relance l'app (exit quasi immédiat) — le toast
  // n'est qu'une brève confirmation avant la coupure.
  notify("success", t("update_relaunch"));
}

/// Update du démon de la connexion active vers `target` : auto-update
/// par le démon lui-même (`update_daemon` via le socket déjà connecté —
/// marche quel que soit le transport) quand il annonce `self_update`,
/// sinon script `ensure_daemon` en ssh — même repli que le flux de
/// connexion. Le restart tue les agents en cours → confirmation
/// explicite avec le compte de sessions vivantes.
export async function performDaemonUpdate(o: {
  conn: string;
  target: string;
  current: string;
  cfg: SavedConn | null;
}) {
  const host = connLabel(o.conn);
  // list_sessions agrège toutes les conns (params.conn ignoré côté Rust)
  // → filtre client sur s.conn. Même définition de « vivant » que
  // conn-view (ni dead ni interrupted ni asleep). Échec → compteur inconnu.
  const sessions = await call<SessionMeta[]>("list_sessions", { conn: o.conn })
    .catch(() => null);
  // Sessions sans champ conn (réponse brute mono-serveur, ex. web) ne
  // peuvent appartenir qu'à la conn "web" — les clés desktop sont
  // toujours préfixées ssh/tcp/local.
  const n = sessions?.filter((s) =>
    (s.conn ?? "web") === o.conn && s.state !== "dead" &&
    s.state !== "interrupted" &&
    s.state !== "asleep" && s.state !== "wait-asleep").length;
  const ok = await confirmPair(
    t("dupdate_h"),
    n == null
      ? t("dupdate_msg_unk", { dv: o.current, lv: o.target, host })
      : t("dupdate_msg", { dv: o.current, lv: o.target, host, n }),
    t("update_now"),
  );
  if (!ok) return;
  // Corrélation client.log : une mise à jour démon (surtout ensure_daemon
  // ssh) peut coïncider avec un flap transport — la ligne porte conn +
  // versions pour la retrouver sans fouiller les toasts.
  logUi("info", `daemon update conn=${o.conn} ${o.current}->${o.target} confirmed`);
  // Capacité fraîche (le statut du check peut dater) : self_update
  // annonce que le démon connaît ReqKind::UpdateDaemon — un plus vieux
  // répondrait `parse: unknown variant` en id=0, droppé par le routage
  // → 60 s muet. Sans le flag et hors ssh : pas d'exec possible.
  const st = await call<Record<string, unknown>>("daemon_status", {
    conn: o.conn,
  }).catch(() => null);
  if (st?.self_update === true) {
    notify("info", t("updating_daemon", { dv: o.current, lv: o.target }));
    try {
      const r = await call<{
        updated?: string;
        restarting?: boolean;
        noop?: boolean;
      }>("update_daemon", { conn: o.conn, version: o.target });
      if (r?.noop) notify("info", t("update_uptodate", { v: o.target }));
      else if (r?.restarting) {
        notify("success", t("dupdate_self_ok", { lv: o.target }));
      } else {
        notify("warn", t("dupdate_self_manual", { lv: o.target }));
      }
    } catch (e) {
      const msg = String(e);
      // Requête inconnue du démon (build hybride/dev) → repli script.
      if (/unknown|inconnue|unrecognized|unsupported|not.?supported/i.test(msg) &&
          o.cfg?.kind === "ssh") {
        await daemonViaScript(o, host);
      } else {
        notify("error", codedErrorMsg(e, "update"));
      }
    }
    return;
  }
  if (o.cfg?.kind === "ssh") {
    await daemonViaScript(o, host);
    return;
  }
  notify("warn", t("dupdate_too_old", { dv: o.current, lv: o.target }));
}

/// Repli ssh (`ensure_daemon` → install/update distant idempotent) —
/// résultat "ok:*"|"newer:*"|"busy:*"|"installed:*" traduit en toast.
async function daemonViaScript(
  o: { conn: string; target: string; current: string; cfg: SavedConn | null },
  host: string,
) {
  if (!o.cfg) return;
  try {
    const res = await invoke<string>(
      "ensure_daemon", daemonParams(o.cfg, o.target));
    if (res.startsWith("ok:")) {
      notify("info", t("update_uptodate", { v: res.slice(3) }));
    } else if (res.startsWith("newer:")) {
      notify("warn", t("dupdate_stale", { host, want: o.target, got: res.slice(6) }));
    } else if (res.startsWith("busy:")) {
      notify("warn", t("dupdate_busy", { host, addr: res.slice(5) }));
    } else if (res.startsWith("managed:")) {
      // Traces d'une gestion ailleurs — jamais d'install forcée ici :
      // le flux de connexion proposera « installer quand même ».
      notify("warn", t("dmanaged_msg", { host, detail: res.slice(8) }));
    } else if (res.startsWith("installed:")) {
      const got = res.slice(10);
      if (got === o.target) {
        notify("success", t("dupdate_done", { lv: o.target, host }));
      } else {
        notify("warn", t("dupdate_stale", {
          host, want: o.target, got: got || "?",
        }));
      }
    } else {
      notify("error", res || t("update_failed"));
    }
  } catch (e) {
    // Host key encore inconnue du profil (première connexion native) :
    // même TOFU que le flux de connexion — confirmation explicite,
    // épinglage persisté, puis le script est rejoué avec le pin.
    const u = /hostkey-unknown:(\S+)/.exec(String(e));
    if (u && o.cfg) {
      const where = `${o.cfg.user ? o.cfg.user + "@" : ""}${o.cfg.host}:${o.cfg.port ?? 22}`;
      const ok = await confirmPair(
        t("hk_new_h"),
        t("hk_new_msg", { where, fp: u[1] }),
        t("hk_new_go"),
      );
      if (ok) {
        o.cfg.host_keys = [u[1]];
        await invoke("save_connection", { cfg: o.cfg }).catch(() => {});
        await daemonViaScript(o, host);
        return;
      }
    }
    notify("error", codedErrorMsg(e, "update"));
  }
}

/// Config sauvegardée correspondant à la clé conn — nécessaire au repli
/// ssh d'ensure_daemon (credentials, host_keys, port…). Partagée entre
/// le panneau settings et le dialogue « À propos ».
export async function daemonCfgFor(ck: string): Promise<SavedConn | null> {
  const list = await invoke<SavedConn[]>("saved_connections")
    .catch(() => [] as SavedConn[]);
  return list.find((c) => connKeyOf(c, isAndroid()) === ck) ?? null;
}

// ---------------------------------------------------------------------------
// Dialogue « À propos » : versions du client, du démon de la connexion
// active et de la dernière release — informatif. Les mises à jour vivent
// dans la fenêtre dédiée (./updates-panel.ts, bouton « Mises à jour… »).
// showModal() part AVANT tout fetch (ouverture <100 ms) ; une ligne en
// échec est masquée, jamais d'état d'erreur.
// ---------------------------------------------------------------------------

/// Ouvre le dialogue « À propos » : reset des lignes dynamiques, modal
/// immédiate, puis remplissage async (version locale, release distante,
/// daemon_status de la conn active). `connKey` est résolu à l'ouverture.
export function openAbout(connKey: () => string | null) {
  const dlg = $("dlg-about") as HTMLDialogElement;
  if (dlg.open) return;
  $("about-client-row").hidden = isWeb;
  $("about-release-row").hidden = true;
  $("about-daemon-row").hidden = true;
  dlg.showModal();
  void (async () => {
    const ck = connKey();
    const [lv, remote, dst] = await Promise.all([
      // Web : la « version client » est celle du démon — ligne masquée.
      isWeb ? Promise.resolve("") : getVersion().catch(() => ""),
      remoteVersion().catch(() => ""),
      ck
        ? call<Record<string, unknown>>("daemon_status", { conn: ck })
            .catch(() => null)
        : Promise.resolve(null),
    ]);
    if (!dlg.open) return;
    if (!isWeb && lv) $("about-version").textContent = `v${lv}`;
    if (remote) {
      $("about-remote-ver").textContent = `v${remote}`;
      $("about-release-row").hidden = false;
    }
    if (ck) {
      const dv = typeof dst?.version === "string" ? dst.version : "";
      $("about-daemon-row").hidden = false;
      $("about-daemon-ver").textContent = dv ? `v${dv}` : "—";
      // "web" = le démon qui sert la SPA — pas de label d'hôte à montrer.
      $("about-daemon-host").textContent =
        ck !== "web" ? ` — ${connLabel(ck)}` : "";
    }
  })();
}

/** Client plus vieux que le démon (minor) : le démon n'est PAS touché —
 *  popup proposant l'update du client (desktop : auto, Android : APK). */
export async function showOutdated(lv: string, dv: string) {
  const dlg = $("dlg-outdated") as HTMLDialogElement;
  if (dlg.open) return;
  const remote = await remoteVersion().catch(() => "");
  const target = remote && cmpVer(remote, dv) >= 0 ? remote : dv;
  $("outdated-msg").textContent = t("outdated_msg", { lv, dv, target });
  const btn = $("outdated-update") as HTMLButtonElement;
  btn.onclick = async () => {
    btn.disabled = true;
    const prev = btn.textContent;
    btn.textContent = t("update_installing", { v: target });
    await performClientUpdate(target);
    btn.disabled = false;
    btn.textContent = prev;
    dlg.close();
  };
  dlg.showModal();
}

/** Check manuel (panneau paramètres) : versions comparables par l'appelant. */
export async function checkUpdate(): Promise<{ local: string; remote: string }> {
  const [remote, local] = await Promise.all([
    remoteVersion(),
    getVersion().catch(() => "0.0.0"),
  ]);
  return { local, remote };
}

/// Résultat laissé par l'instance précédente : l'install tue le process
/// (relaunch) avant tout affichage — le backend dépose alors un statut
/// "OK <ver>" / "FAIL <cause>" consommé ici une fois → toast + log.
async function reportLastUpdateStatus() {
  const st = await invoke<string | null>("update_last_status").catch(() => null);
  if (!st) return;
  if (st.startsWith("FAIL")) {
    notify("error", t("update_failed_reason", {
      reason: codedErrorMsg(st.slice(4), "update"),
    }));
  } else if (st.startsWith("WARN")) {
    // "WARN <ver> <cause>" : install réussie mais relance impossible —
    // le prochain boot doit pouvoir le dire (spec item #195).
    const rest = st.slice(4).trim();
    const sp = rest.indexOf(" ");
    notify("warn", t("update_partial", {
      v: sp < 0 ? rest : rest.slice(0, sp),
      reason: sp < 0 ? "" : codedErrorMsg(rest.slice(sp + 1), "update"),
    }));
  } else if (st.startsWith("OK")) {
    notify("success", t("update_done", { v: st.slice(2).trim() }));
  }
}

// Check update au boot : version distante (gh release ou package.json sur
// main) > locale → chip "⬆ vX" dans la titlebar → clic = fenêtre Mises à
// jour (versions app + démon, bouton d'install — `openPanel`).
export function checkBootUpdate(openPanel: (chip: HTMLElement) => void) {
  // Pas de client à mettre à jour en mode web : la SPA est livrée par le
  // démon — sa « version » suit celle du binaire qui la sert.
  if (isWeb) return;
  void (async () => {
    await reportLastUpdateStatus();
    try {
      const remote = await remoteVersion();
      const local = await getVersion().catch(() => "0.0.0");
      if (cmpVer(remote, local) <= 0) return;
      const b = document.createElement("button");
      b.className = "tb-update";
      b.innerHTML = `${icon("up")} v${esc(remote)}`;
      b.title = t("update_badge_title", { v: remote });
      b.setAttribute("aria-haspopup", "dialog");
      b.onclick = () => openPanel(b);
      $("titlebar").insertBefore(b, document.querySelector(".tb-btns"));
    } catch {
      /* pas de gh/réseau → tant pis */
    }
  })();
}
