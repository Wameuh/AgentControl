// ---------------------------------------------------------------------------
// Mise à jour du client : check au boot (chip "⬆ vX" dans la titlebar),
// install .deb desktop / ouverture APK Android, popup "client périmé"
// quand le démon est plus récent (minor).
// ---------------------------------------------------------------------------

import { invoke, getVersion, isWeb } from "../core/bridge";
import { $, esc } from "../core/dom";
import { icon } from "../ui/icons";
import { isAndroid, shellOpen } from "../core/chrome";
import { cmpVer } from "../core/conn";
import { t } from "../i18n";

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
  // Pas de client à mettre à jour en mode web : la SPA est livrée par le
  // démon — sa « version » suit celle du binaire qui la sert.
  if (isWeb) return;
  void (async () => {
    try {
      const remote = await remoteVersion();
      const local = await getVersion().catch(() => "0.0.0");
      if (cmpVer(remote, local) <= 0) return;
      const b = document.createElement("button");
      b.className = "tb-update";
      b.innerHTML = `${icon("up")} v${esc(remote)}`;
      b.title = `Update v${remote} available — click to install`;
      b.onclick = () => {
        b.textContent = "…";
        performClientUpdate(remote).finally(() => {
          b.innerHTML = `${icon("up")} v${esc(remote)}`;
        });
      };
      $("titlebar").insertBefore(b, document.querySelector(".tb-btns"));
    } catch {
      /* pas de gh/réseau → tant pis */
    }
  })();
}
