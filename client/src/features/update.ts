// ---------------------------------------------------------------------------
// Mise à jour du client : check au boot (chip "⬆ vX" dans la titlebar),
// install .deb desktop / ouverture APK Android, popup "client périmé"
// quand le démon est plus récent (minor).
// ---------------------------------------------------------------------------

import { invoke } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import { $ } from "../core/dom";
import { isAndroid, shellOpen } from "../core/chrome";
import { cmpVer } from "../core/conn";
import { t } from "../i18n";

// Version distante : fetch direct (pas d'outil local), fallback commande Rust.
async function remoteVersion(): Promise<string> {
  try {
    const r = await fetch(
      "https://api.github.com/repos/Wameuh/AgentControl/releases/latest",
    );
    if (r.ok) {
      const j = (await r.json()) as { tag_name?: string };
      if (j.tag_name) return j.tag_name.replace(/^v/, "");
    }
  } catch {
    /* CSP/réseau → fallback invoke */
  }
  return invoke<string>("check_update");
}

/// URL de l'APK de la release `tag` (asset `.apk`), fallback = page releases.
async function apkReleaseUrl(tag: string): Promise<string> {
  try {
    const r = await fetch(
      `https://api.github.com/repos/Wameuh/AgentControl/releases/tags/v${tag}`,
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

/// Update du client vers `tag` : desktop = .deb auto (`update_client`,
/// fallback page releases) ; Android = APK ouvert dans le navigateur —
/// le gestionnaire de paquets fait la maj in-place (signature identique).
export async function performClientUpdate(tag: string) {
  if (isAndroid()) {
    shellOpen(await apkReleaseUrl(tag));
    return;
  }
  await invoke("update_client", { tag }).catch(() => {
    shellOpen(`https://github.com/Wameuh/AgentControl/releases/tag/v${tag}`);
  });
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
    await performClientUpdate(target);
    btn.disabled = false;
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

// Check update au boot : version distante (gh release ou package.json sur
// main) > locale → chip "⬆ vX" dans la titlebar → clic = update (deb+dpkg)
// ou ouverture de la page releases si aucun asset.
export function checkBootUpdate() {
  void (async () => {
    try {
      const remote = await remoteVersion();
      const local = await getVersion().catch(() => "0.0.0");
      if (cmpVer(remote, local) <= 0) return;
      const b = document.createElement("button");
      b.className = "tb-update";
      b.textContent = `⬆ v${remote}`;
      b.title = `Update v${remote} available — click to install`;
      b.onclick = () => {
        b.textContent = "…";
        performClientUpdate(remote).finally(() => {
          b.textContent = `⬆ v${remote}`;
        });
      };
      $("titlebar").insertBefore(b, document.querySelector(".tb-btns"));
    } catch {
      /* pas de gh/réseau → tant pis */
    }
  })();
}
