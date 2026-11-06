// ---------------------------------------------------------------------------
// Mise à jour du client : check au boot (chip "⬆ vX" dans la titlebar),
// install .deb desktop / ouverture APK Android, popup "client périmé"
// quand le démon est plus récent (minor).
// ---------------------------------------------------------------------------

import { invoke, getVersion, isWeb } from "../core/bridge";
import { $, esc, notify } from "../core/dom";
import { icon } from "../ui/icons";
import { isAndroid, shellOpen } from "../core/chrome";
import { cmpVer, connLabel, daemonParams } from "../core/conn";
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

async function remoteVersion(): Promise<string> {
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

/// URL de l'APK de la release `tag` (asset `.apk`), fallback = page releases.
async function apkReleaseUrl(tag: string): Promise<string> {
  try {
    const r = await fetch(
      `https://api.github.com/repos/Wameuh/AgentControl/releases/tags/v${tag}`,
      { signal: AbortSignal.timeout(FETCH_TIMEOUT) },
    );
    if (r.ok) {
      const j = (await r.json()) as {
        assets?: { name?: string; browser_download_url?: string }[];
      };
      const a = j.assets?.find((x) => x.name?.endsWith(".apk"));
      if (a?.browser_download_url) return a.browser_download_url;
    }
  } catch {
    /* réseau */
  }
  return "https://github.com/Wameuh/AgentControl/releases/latest";
}

/// Update du client vers `tag` : desktop = installeur auto
/// (`update_client`) ; Android = APK ouvert dans le navigateur — le
/// gestionnaire de paquets fait la maj in-place (signature identique).
/// Jamais silencieux : étapes versées à client.log, toast info au
/// départ puis succès / échec AVEC la raison (les erreurs backend
/// citent le repli manuel — commande ou page releases).
export async function performClientUpdate(tag: string) {
  if (isAndroid()) {
    shellOpen(await apkReleaseUrl(tag));
    // L'APK s'installe dans le gestionnaire de paquets : le process meurt
    // avec l'app et Android ne permet pas à une app remplacée de se
    // relancer seule — on guide explicitement au lieu de laisser
    // l'utilisateur dans le navigateur sans mode d'emploi.
    notify("info", t("update_android_reopen"));
    return;
  }
  // notify() verse déjà au client.log — pas de logUi en plus (doublon).
  notify("info", t("update_installing", { v: tag }));
  try {
    await invoke("update_client", { tag });
  } catch (e) {
    const reason = String(e);
    notify("error", t("update_failed_reason", { reason }));
    // Repli « page releases » quand le backend le désigne (hors bundle
    // .app, build portable…) — l'erreur affiche déjà la cause exacte.
    if (reason.includes("page releases")) {
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
  const n = sessions?.filter((s) =>
    s.conn === o.conn && s.state !== "dead" && s.state !== "interrupted" &&
    s.state !== "asleep").length;
  const ok = await confirmPair(
    t("dupdate_h"),
    n == null
      ? t("dupdate_msg_unk", { dv: o.current, lv: o.target, host })
      : t("dupdate_msg", { dv: o.current, lv: o.target, host, n }),
    t("update_now"),
  );
  if (!ok) return;
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
        notify("error", msg);
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
    notify("error", String(e));
  }
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
    notify("error", t("update_failed_reason", { reason: st.slice(4).trim() }));
  } else if (st.startsWith("WARN")) {
    // "WARN <ver> <cause>" : install réussie mais relance impossible —
    // le prochain boot doit pouvoir le dire (spec item #195).
    const rest = st.slice(4).trim();
    const sp = rest.indexOf(" ");
    notify("warn", t("update_partial", {
      v: sp < 0 ? rest : rest.slice(0, sp),
      reason: sp < 0 ? "" : rest.slice(sp + 1),
    }));
  } else if (st.startsWith("OK")) {
    notify("success", t("update_done", { v: st.slice(2).trim() }));
  }
}

// Check update au boot : version distante (gh release ou package.json sur
// main) > locale → chip "⬆ vX" dans la titlebar → clic = update (deb+dpkg)
// ou ouverture de la page releases si aucun asset.
export function checkBootUpdate() {
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
      b.onclick = () => {
        b.disabled = true;
        b.textContent = t("update_installing", { v: remote });
        performClientUpdate(remote).finally(() => {
          b.disabled = false;
          b.innerHTML = `${icon("up")} v${esc(remote)}`;
        });
      };
      $("titlebar").insertBefore(b, document.querySelector(".tb-btns"));
    } catch {
      /* pas de gh/réseau → tant pis */
    }
  })();
}
