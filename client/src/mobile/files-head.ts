// ---------------------------------------------------------------------------
// Barre de l'explorateur sur Android : le chemin mono-ligne (#file-path,
// masqué) est remplacé par un titre lisible — nom du dossier racine +
// son chemin complet — et une rangée de raccourcis vers les fichiers
// récents de la session (les mêmes que la palette quick-open).
// ---------------------------------------------------------------------------

import { $, esc } from "../core/dom";
import { icon } from "../ui/icons";
import { recentFiles, recentsId } from "../core/recent-files";

/** Dossier racine courant — posé par setFileTitle, sert à afficher les
 *  récents en chemins relatifs quand ils vivent sous la racine. */
let rootDir = "";

/** Titre de l'explorateur = dernier segment du dossier racine ; le
 *  chemin complet passe en sous-titre (tronqué à gauche par le CSS). */
export function setFileTitle(dir: string) {
  rootDir = dir;
  $("file-title-name").textContent = dir.split("/").filter(Boolean).pop() ?? dir;
  $("file-title-path").textContent = dir;
  $("file-title").hidden = false;
}

/** Rangée « Récents » : une puce par fichier récemment ouvert (6 max),
 *  tap = ouverture dans le viewer. Masquée quand la pile est vide.
 *  Libellé = chemin relatif à la racine quand il y vit (plus parlant
 *  qu'un basename : client.ts vs tests/client.ts), chemin complet en
 *  title. */
export function renderRecents(
  sid: string,
  conn: string | undefined,
  open: (p: string) => void,
) {
  const box = $("ft-recent");
  const files = recentFiles(recentsId(sid, conn)).slice(0, 6);
  if (!files.length) { box.hidden = true; return; }
  box.innerHTML = "";
  for (const p of files) {
    const label = rootDir && p.startsWith(rootDir + "/")
      ? p.slice(rootDir.length + 1)
      : p;
    const b = document.createElement("button");
    b.className = "ft-rec";
    b.innerHTML = `${icon("file")}<span>${esc(label)}</span>`;
    b.title = p;
    b.onclick = () => open(p);
    box.appendChild(b);
  }
  box.hidden = false;
}
