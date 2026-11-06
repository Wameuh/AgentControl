// ---------------------------------------------------------------------------
// Bascule « rendu Markdown / source » du viewer de fichiers (.md/.markdown) :
// le <pre> coloré (#file-view) et le rendu mdRender (#file-md) coexistent —
// bascule instantanée sans re-fetch, le rendu est refait quand le buffer
// source change. Préférence persistée (ac-md-render — rendu par défaut).
// Les chemins cliquables (.flink) se résolvent relativement au dossier du
// fichier ouvert.
// ---------------------------------------------------------------------------

import { $ } from "../core/dom";
import { mdRender } from "../core/format";
import { t, type Key } from "../i18n";
import { icon } from "../ui/icons";

const MD_EXT = new Set(["md", "markdown"]);

interface MdDeps {
  /** Texte source courant (chunks joints). */
  text: () => string;
  /** Chargement terminé — "" signifie alors fichier vide, pas « en cours ». */
  done: () => boolean;
  /** Ouvre un autre fichier dans le viewer (chemin relatif au cwd). */
  openFile: (rel: string) => void;
  /** Proche du bas du conteneur visible → tranche suivante du fichier. */
  more: () => void;
}

export class MdView {
  /** Préférence utilisateur : rendre le Markdown (défaut oui). */
  private on = localStorage.getItem("ac-md-render") !== "0";
  /** Fichier courant = markdown — le bouton n'existe que pour lui. */
  private md = false;
  /** Le textarea d'édition couvre les deux vues. */
  private editing = false;
  /** Rendu à refaire avant affichage (source modifiée depuis). */
  private stale = true;
  /** Dossier du fichier courant — base des .flink relatives. */
  private baseDir = "";
  private timer = 0;

  constructor(private deps: MdDeps) {}

  /** Fichier texte ouvert : le bouton apparaît pour .md/.markdown. */
  attach(rel: string, ext: string) {
    this.md = MD_EXT.has(ext);
    this.stale = true;
    this.baseDir = rel.includes("/") ? rel.slice(0, rel.lastIndexOf("/")) : "";
    // Placeholder pendant la première tranche (le rendu d'un texte vide
    // est vide — « … » matérialise le chargement comme en vue source).
    if (this.rendered) $("file-md").textContent = "…";
    this.sync();
  }

  /** Le fichier n'est pas affichable en md (binaire sniffé, reset, image,
   *  vidéo…) — bouton et rendu disparaissent, la vue source reste. */
  detach() {
    this.md = false;
    this.editing = false;
    this.sync();
  }

  /** Buffer source modifié (tranche reçue, fichier ré-enregistré…) : le
   *  rendu visible se refait en différé — mdRender() est O(n), les tranches
   *  d'un chargement progressif ne déclenchent qu'un seul re-render. */
  changed() {
    this.stale = true;
    if (this.rendered) this.schedule();
  }

  /** Entrée/sortie du mode édition : l'édition porte toujours sur la
   *  source — bouton et rendu masqués, restaurés à la sortie. */
  setEditing(on: boolean) {
    this.editing = on;
    this.sync();
  }

  /** Le div rendu est la vue active (son scroll alimente maybeMore). */
  get rendered() {
    return this.on && this.md && !this.editing;
  }

  /** Préférence « rendu par défaut » basculée depuis le panneau
   *  paramètres — la persistance est faite par l'appelant, ici on
   *  re-synchronise juste la vue courante. */
  setRender(on: boolean) {
    this.on = on;
    this.sync();
  }

  private schedule() {
    window.clearTimeout(this.timer);
    this.timer = window.setTimeout(() => this.render(), 150);
  }

  private render() {
    if (!this.rendered) return;
    const txt = this.deps.text();
    // Garde le « … » tant que la première tranche n'est pas arrivée
    // (fichier non vide en cours de chargement ≠ fichier vide).
    if (!txt && !this.deps.done()) return;
    $("file-md").innerHTML = mdRender(txt);
    this.stale = false;
  }

  /** Applique l'état courant au DOM : visibilité des deux vues + bouton
   *  (icône et titre décrivent l'action du clic — data-i18n-title suit
   *  l'état pour rester traduit au changement de langue). */
  private sync() {
    const show = this.rendered;
    const btn = $("fv-md");
    btn.hidden = !this.md || this.editing;
    btn.classList.toggle("on", this.on);
    btn.innerHTML = icon(this.on ? "code" : "eye");
    btn.dataset.i18nTitle = this.on ? "md_source" : "md_view";
    btn.title = t(btn.dataset.i18nTitle as Key);
    $("file-md").hidden = !show;
    $("file-view").hidden = show || this.editing;
    if (show && this.stale) this.render();
  }

  init() {
    $("fv-md").onclick = () => {
      this.on = !this.on;
      localStorage.setItem("ac-md-render", this.on ? "1" : "0");
      this.sync();
    };
    const view = $("file-md");
    view.addEventListener("scroll", (e) => {
      const el = e.target as HTMLElement;
      if (el.scrollTop + el.clientHeight >= el.scrollHeight * 0.85) this.deps.more();
    });
    // Chemins cliquables du rendu : relatifs au dossier du fichier ouvert.
    view.addEventListener("click", (e) => {
      const el = (e.target as HTMLElement).closest<HTMLElement>(".flink");
      if (!el?.dataset.path) return;
      // Traité ici : la délégation globale des <a> (initExternalLinks)
      // lit defaultPrevented et laisse ce clic tranquille.
      e.preventDefault();
      this.deps.openFile(this.resolve(el.dataset.path));
    });
  }

  /** Résout un chemin relatif au dossier du fichier ouvert. Un « .. »
   *  qui dépasserait la racine du workspace est ignoré — la résolution
   *  finale reste bornée côté démon. */
  private resolve(p: string): string {
    if (p.startsWith("/")) return p;
    const segs = this.baseDir ? this.baseDir.split("/") : [];
    for (const s of p.split("/")) {
      if (s === "..") segs.pop();
      else if (s !== "." && s !== "") segs.push(s);
    }
    return segs.join("/");
  }
}
