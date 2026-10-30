// ---------------------------------------------------------------------------
// Liste des sessions : regroupées par connexion puis groupes nommés —
// badges PR et état de connexion, menu contextuel, renommage inline,
// drag & drop inter-groupes, poignée de redimensionnement.
// ---------------------------------------------------------------------------

import { $, esc, notify, q, showErr } from "../core/dom";
import { call } from "../core/rpc";
import { app } from "../core/state";
import { isAndroid, meshSetMood } from "../core/chrome";
import type { PrEntry, SessionMeta } from "../core/types";
import { t } from "../i18n";
import { dotCls, dotIcon, setAppIcon } from "./status";
import { icon } from "../ui/icons";
import { bindCtx, hideSessCtx, menuBtn, placeMenu } from "../ui/ctxmenu";
import { nav } from "../ui/nav";
import { PrListView, prBadge, prUnresolved, repoLabel, type PrRow } from "./prs";
import { BacklogView } from "./backlog";
import type { ChatView } from "./chat-view";
import type { ConnectionManager } from "./connections";
import { grpNorm, grpParent, inSubtree } from "./groups";
import type { GroupStore } from "./groups";
import { sounds } from "./sounds";
import type { NewSessionDialog } from "./new-session";
import type { SettingsPanel } from "./settings";
import type { ConnView } from "./conn-view";
import type { ComponentsPanel } from "./components";
import type { MapView } from "./map-view";

const DTYPE = "application/x-ac-session";

/** Charge d'un drag interne à l'arbre : session (id) ou sous-arbre de
 *  groupe (path). `kind` absent = session (payloads antérieurs). */
type DragPayload =
  | { kind?: "session"; id: string; conn: string }
  | { kind: "group"; path: string; conn: string };

/** Nœud de l'arbre des groupes d'une connexion : un chemin "a/b/c"
 *  s'affiche comme 3 nœuds imbriqués ; les sessions sont feuilles du
 *  nœud de leur chemin exact. */
interface GNode {
  path: string;
  name: string;
  kids: Map<string, GNode>;
  rows: SessionMeta[];
}

/** Ordonne un compartiment de sessions en arbre parent→sous-agents :
 *  une session spawnée (spawned_by) dont la créatrice est rendue dans le
 *  même compartiment s'y imbrique juste après elle (depth+1) ; les
 *  orphelines (parent supprimé, autre groupe, filtré) restent racines.
 *  Garde-fou cycles (meta corrompu) : tout nœud non atteint ressort
 *  racine plutôt que disparaître. Retour aplatie dans l'ordre d'affichage. */
function nestSessions(rows: SessionMeta[]): { s: SessionMeta; depth: number }[] {
  const ids = new Set(rows.map((r) => r.id));
  const byParent = new Map<string, SessionMeta[]>();
  const roots: SessionMeta[] = [];
  for (const s of rows) {
    const p = s.spawned_by;
    if (p && ids.has(p)) {
      const a = byParent.get(p);
      if (a) a.push(s); else byParent.set(p, [s]);
    } else {
      roots.push(s);
    }
  }
  const out: { s: SessionMeta; depth: number }[] = [];
  const seen = new Set<string>();
  const walk = (s: SessionMeta, depth: number) => {
    if (seen.has(s.id)) return;
    seen.add(s.id);
    out.push({ s, depth });
    for (const c of byParent.get(s.id) ?? []) walk(c, depth + 1);
  };
  for (const s of roots) walk(s, 0);
  for (const s of rows) walk(s, 0);
  return out;
}

export class Sidebar {
  private conns!: ConnectionManager;
  private groups!: GroupStore;
  private newSess!: NewSessionDialog;
  private chat!: ChatView;
  private settings!: SettingsPanel;
  private connView!: ConnView;
  private comps!: ComponentsPanel;
  private map!: MapView;

  /** PRs ouvertes par session ("conn\0sid") — rempli par refresh ;
   *  vide si le démon ne connaît pas pr_list (vieux démon → pas de badge). */
  private prBySess = new Map<string, PrEntry[]>();

  // --- Vue sidebar : « agents » ↔ « repos » ↔ « backlog » (file partagée)
  private prsView = new PrListView();
  private backlogView = new BacklogView();
  private view: "agents" | "repos" | "backlog" = "agents";
  /** Le diagramme (#map-view, overlay plein #chat) n'est pas une vue de
   *  la liste mais partage le sélecteur : ouvert, son onglet prend
   *  l'état actif ; à sa fermeture this.view reprend la main. */
  private mapOpen = false;
  /** Réponse brute du dernier pr_list (entrées + marqueurs conn_error). */
  private prRows: PrRow[] = [];
  private prFailed = false;
  /** delete_session en vol OU terminée : l'id reste tombstoné après
   *  succès (un refresh dont le snapshot list_sessions date d'avant la
   *  fin de la suppression ne doit pas réinjecter la ligne — un id
   *  supprimé ne réapparaît jamais) ; retiré seulement en cas d'échec,
   *  où la session existe encore. */
  private pendingDel = new Set<string>();

  /** Renommage inline en cours (compteur : plusieurs lignes peuvent
   *  théoriquement être en édition). Tant qu'il est >0, renderList()
   *  diffère son rebuild — innerHTML="" détacherait l'input de titre
   *  (blur → annulation ou sauvegarde prématurée), ce qui tuait
   *  l'édition dès qu'une session qui travaille émettait un
   *  status_changed. Le dernier done() rejoue le rendu différé. */
  private editCount = 0;
  private rebuildPending = false;

  /** Drag HTML5 en vol (toute source : ligne de session, sélection de
   *  texte…). Tant qu'il est >0, renderList() diffère son rebuild —
   *  détacher l'élément source du drag le tue anormalement et, sous
   *  WebKitGTK/Wayland, laisse le curseur "grabbing" collé sans recours
   *  côté web (bug toolkit connu). dragend — ou le watchdog mouseup —
   *  solde le compteur et rejoue le rendu différé. */
  private dragging = 0;

  /** Suppression initiée ici — l'event session_deleted qui suit ne doit
   *  pas re-notifier (la ligne a déjà disparu, l'utilisateur sait). */
  wasPendingDel(id: string) {
    return this.pendingDel.has(id);
  }

  /** Marque des suppressions initiées en masse hors sidebar (ex. delete
   *  d'une connexion → cascade delete_session) : leurs events
   *  session_deleted ne doivent pas spammer de toasts. */
  notePendingDel(ids: string[]) {
    for (const id of ids) this.pendingDel.add(id);
  }

  /** Démarque une suppression en échec (cascade) : la session existe
   *  encore, elle ne doit pas rester filtrée de la liste. */
  clearPendingDel(id: string) {
    this.pendingDel.delete(id);
  }
  private ctx = $("sctx");

  bind(conns: ConnectionManager, groups: GroupStore, newSess: NewSessionDialog, chat: ChatView, settings: SettingsPanel, connView: ConnView, comps: ComponentsPanel, map: MapView) {
    this.conns = conns;
    this.groups = groups;
    this.newSess = newSess;
    this.chat = chat;
    this.settings = settings;
    this.connView = connView;
    this.comps = comps;
    this.map = map;
    this.prsView.bind({
      openSession: (id) => this.openSession(id),
      connState: (k) => this.conns.state(k),
      hl: (txt) => this.hl(txt),
    });
    this.backlogView.bind({
      // Composants = 1 démon lié — même convention que la map-view.
      conn: () => this.conns.knownConns()[0] ?? "",
      componentsCapable: (c) => this.conns.cap(c)?.components === true,
      openSession: (id, conn) => {
        this.closeChatOverlays();
        void this.chat.open(id, false, conn);
      },
      hl: (txt) => this.hl(txt),
    });
  }

  /** Ferme tous les overlays plein #chat — la navigation vers une
   *  session prime : rien ne persiste par-dessus la discussion
   *  demandée (diagramme inclus). close() est un no-op quand
   *  l'overlay est déjà fermé. */
  private closeChatOverlays() {
    this.settings.close();
    this.connView.close();
    this.comps.close();
    this.map.close();
  }

  /** Ouvrir une session depuis la sidebar = quitter les overlays de
   *  #chat (paramètres, vue connexion, composants, diagramme la
   *  recouvrent — la session s'afficherait dessous, invisible). */
  private openSession(id: string): Promise<void> {
    this.closeChatOverlays();
    return this.chat.open(id);
  }

  /** Ligne de session marquée active immédiatement au clic — avant tout
   *  chargement, sinon l'ancienne session semble encore sélectionnée. */
  setActiveRow(id: string) {
    for (const r of document.querySelectorAll<HTMLElement>("#session-list .sess")) {
      r.classList.toggle("active", r.dataset.sid === id);
    }
  }

  async refresh() {
    // pr_list en parallèle : un démon mort/vieux ne doit pas retarder la
    // sidebar (erreur → pas de badge, c'est tout).
    const [raw, prRes] = await Promise.all([
      call<SessionMeta[]>("list_sessions").catch((e) => {
        setAppIcon("grey"); // démon injoignable → aucun agent joignable
        this.chat.updateEmptyState(); // l'accueil ne doit pas attendre la liste
        throw e;
      }),
      call<PrRow[]>("pr_list").then(
        (rows) => ({ rows, failed: false }),
        () => ({ rows: [] as PrRow[], failed: true }),
      ),
    ]);
    this.applyPrRows(prRes.rows, prRes.failed);
    // Marqueur Rust `conn_error` : la sonde list_sessions d'une conn a
    // échoué (timeout 8 s) → la marquer down (une seule fois : déjà down,
    // markDead re-rendererait refresh en boucle).
    const list = raw.filter((s) => {
      if (this.pendingDel.has(s.id)) return false;
      if (!(s as { conn_error?: boolean }).conn_error) return true;
      const k = s.conn ?? "";
      if ((this.conns.state(k) ?? "ok") === "ok") this.conns.markDead(k);
      return false;
    });
    // Conn down : ses sessions n'arrivent plus — réafficher le dernier cache
    // connu (grisé) au lieu de faire disparaître le serveur.
    for (const [k, st] of this.conns.liveEntries()) {
      if (st !== "ok" && !list.some((s) => (s.conn ?? "default") === k)) {
        for (const s of app.lastSessions.filter(
          (x) => (x.conn ?? "default") === k && !this.pendingDel.has(x.id),
        )) {
          list.push({ ...s, offline: true });
        }
      }
    }
    app.lastSessions = list;
    // La vue backlog résout agent_id → titre de session : snapshot frais.
    this.backlogView.noteSessions();
    // Alertes sonores : diff d'états (la liste fusionnée couvre toutes les
    // conns — le canal event ne suit que la session ouverte).
    sounds.noteStates(list);
    this.checkSpawns(list);
    // Un spawn tourne sans canal event (session non ouverte → pas de
    // status_changed) → repoller tant qu'un "starting" reste dans son
    // budget (3 min chacune — un spawn bloqué ne poll pas indéfiniment,
    // mais un second spawn du même épisode ré-arme sa propre deadline).
    const now = Date.now();
    const seen = new Set<string>();
    let spawning = false;
    for (const s of list) {
      if (s.offline || s.state !== "starting") continue;
      const key = `${s.conn ?? "default"}:${s.id}`;
      seen.add(key);
      const t0 = this.startingSeen.get(key) ?? now;
      this.startingSeen.set(key, t0);
      if (now - t0 < 180_000) spawning = true;
    }
    for (const k of [...this.startingSeen.keys()]) {
      if (!seen.has(k)) this.startingSeen.delete(k);
    }
    if (spawning && this.spawnPoll === undefined) {
      this.spawnPoll = window.setTimeout(() => {
        this.spawnPoll = undefined;
        void this.refresh();
      }, 2500);
    }
    // Vue « Repos » : croise les entrées du registre avec les sessions
    // fusionnées (cache offline inclus) + compteur sur l'onglet.
    this.prsView.setData(this.prRows, list, this.prFailed);
    this.renderPrBadge();
    const live = list.filter((s) =>
      !s.offline && s.state !== "dead" && s.state !== "interrupted" && s.state !== "asleep");
    setAppIcon(
      live.some((s) => s.state === "waiting_input")
        ? "red"
        : live.some(
              (s) =>
                s.state === "working" ||
                s.state === "running" ||
                s.state === "starting",
            )
          ? "gold"
          : live.some((s) => s.state === "compacting")
            ? "violet"
            : live.length > 0 ? "base" : "grey",
    );
    // Session courante disparue alors que sa connexion répond encore
    // (sessions du même serveur présentes) → supprimée côté serveur.
    this.chat.markGoneIfMissing(list);
    // Lumières "agents" du fond : une par session active (les sessions
    // cachées d'une conn down ne comptent pas).
    const n = list.filter(
      (s) =>
        !s.offline &&
        (s.state === "working" || s.state === "waiting_input" || s.state === "compacting"),
    ).length;
    meshSetMood({ agents: Math.min(n, 4), intensity: 0.55 });
    this.renderView();
    // La liste arrive → l'accueil recalcule son état (0 session → CTA).
    this.chat.updateEmptyState();
  }

  /** Applique une réponse `pr_list` : registre + marqueurs conn_error →
   *  lignes de la vue Repos et index session → PRs (badges des lignes). */
  private applyPrRows(rows: PrRow[], failed: boolean) {
    this.prRows = rows;
    this.prFailed = failed;
    this.prBySess.clear();
    for (const p of rows) {
      if (!p.session || p.status === "merged" || p.status === "closed") continue;
      // « Non résoluble » = probable fantôme — visible dans l'onglet
      // mais pas en badge de session (rien d'actionnable dessus).
      if (prUnresolved(p)) continue;
      const k = `${p.conn ?? "default"}${p.session}`;
      if (!this.prBySess.has(k)) this.prBySess.set(k, []);
      this.prBySess.get(k)!.push(p);
    }
  }

  /** Compteur de l'onglet Repos — PRs encore ouvertes du registre. */
  private renderPrBadge() {
    const n = this.prsView.openCount();
    const b = $("sv-count");
    b.hidden = n === 0;
    b.textContent = String(n);
  }

  /** Badge Repos vivant hors de l'onglet : `pr_list` lit le registre
   *  local du démon (aucun appel forge — `pr_sync` reste l'affaire du
   *  cycle propre à l'onglet). Couvre les changements sans event client
   *  visible : PR mergée par un autre agent, attribution détectée par
   *  le watcher du démon… Tick sauté sur l'onglet Repos lui-même :
   *  `prSyncCycle` y fait déjà un refresh() complet toutes les 60 s. */
  private prCountTimer: number | undefined;
  /** Intervalle == timeout RPC (60 s) : un pr_list pendu laisserait le
   *  tick suivant se chevaucher — garde anti-réentrance. */
  private prTickBusy = false;

  private async prCountTick() {
    if (this.view === "repos" || this.prTickBusy) return;
    this.prTickBusy = true;
    try {
      // Le badge suit la forge, pas le registre figé : tant que des PRs
      // encore « open/draft » sont affichées sur des lignes session, on
      // sync avant de relire — sinon un merge hors session (web GitHub,
      // gh externe, automation) laissait le badge collé indéfiniment
      // (pr_sync ne tournait que sur l'onglet Repos). Pas d'appel quand
      // rien n'est tracké : zéro trafic forge sans badge. Le démon
      // throttle (45 s) + déduplique — N clients = 1 round/min. Délai
      // de disparition après merge : ≤ ~1 tick (pas d'état « merged »
      // transitoire — le retrait est le comportement choisi/documenté).
      if (this.prBySess.size > 0) await this.prSyncRequest();
      const r = await call<PrRow[]>("pr_list").then(
        (rows) => ({ rows, failed: false }),
        () => ({ rows: [] as PrRow[], failed: true }),
      );
      this.applyPrRows(r.rows, r.failed);
      this.prsView.setData(this.prRows, app.lastSessions, this.prFailed);
      this.renderPrBadge();
    } finally {
      this.prTickBusy = false;
    }
  }

  /** Repoll interne tant qu'une session est "starting" (budget par
   *  session) — spawn sans canal event : la session n'est pas forcément
   *  ouverte. startingSeen = première apparition par conn:id. */
  private spawnPoll: number | undefined;
  private startingSeen = new Map<string, number>();

  /** Issue des spawns lancés par ce client (app.spawnWatch) : échec
   *  (dead/interrupted) → toast ; état résolu ou TTL dépassé → fin de
   *  suivi. Couvre le cas où l'utilisateur navigue ailleurs pendant le
   *  boot : hors session ouverte aucun event ne signale l'échec. */
  private checkSpawns(list: SessionMeta[]) {
    const now = Date.now();
    for (const [id, ts] of app.spawnWatch) {
      const s = list.find((x) => x.id === id);
      if (!s || now - ts > 10 * 60 * 1000) {
        // Sortie de la liste (supprimée) ou jamais résolue — oubli.
        app.spawnWatch.delete(id);
        continue;
      }
      if (s.offline || s.state === "starting") continue;
      // Session ouverte : l'event Error est déjà visible dans le chat —
      // le toast ne sert que quand l'échec arrive hors de la vue.
      if ((s.state === "dead" || s.state === "interrupted") && app.current?.id !== id) {
        notify("warn", t("spawn_failed", { title: s.title }));
      }
      app.spawnWatch.delete(id);
    }
  }

  /** Rend la liste de la vue active : arbre sessions ou PRs par dépôt.
   *  Réaligne aussi le placeholder du filtre partagé (applyStatic a pu
   *  le réécrire au changement de langue pendant la vue repos). */
  private renderView() {
    const fIn = $("sess-filter-input") as HTMLInputElement;
    const v = this.view;
    fIn.placeholder = t(
      v === "repos" ? "pr_filter_ph" : v === "backlog" ? "bl_filter_ph" : "sess_filter_ph",
    );
    fIn.title = t(
      v === "repos" ? "pr_filter_title" : v === "backlog" ? "bl_filter_title" : "sess_filter_title",
    );
    if (v === "repos") {
      this.prsView.setFilter(this.filter);
      this.prsView.render();
    } else if (v === "backlog") {
      this.backlogView.setFilter(this.filter);
    } else {
      this.renderList();
    }
  }

  /** Bascule agents ↔ repos ↔ backlog (persistée). Passer en repos
   *  resynchronise le registre ; passer en backlog démarre son fetch
   *  rapide (le quitter retombe sur la cadence badge ~60 s). */
  private setView(v: "agents" | "repos" | "backlog") {
    if (this.view === v) return;
    this.view = v;
    localStorage.setItem("ac-sideview", v);
    this.applyView();
    if (v === "repos") void this.refresh();
  }

  /** Ouverture/fermeture du diagramme — notifiée par MapView via deps.
   *  applyView est idempotent : simple re-calcul des états visuels. */
  setMapOpen(open: boolean) {
    if (this.mapOpen === open) return;
    this.mapOpen = open;
    this.applyView();
  }

  /** Poll du sync des statuts PR, actif seulement sur l'onglet Repos :
   *  cycle à l'ouverture puis toutes les 60 s (le démon throttle à ~45 s
   *  et déduplique les runs — plusieurs clients/fenêtres peuvent
   *  demander en rafale). */
  private prSyncTimer: number | undefined;
  /** Vieux démon sans `pr_sync` → on arrête de demander (une erreur
   *  réseau n'arme pas le flag : la retente passe au prochain tick). */
  private prSyncDead = false;

  private prsSetActive(on: boolean) {
    if (on) {
      // Réarme à l'activation : un démon mis à jour depuis la dernière
      // visite de l'onglet doit pouvoir sync à nouveau.
      this.prSyncDead = false;
      void this.prSyncCycle();
      if (this.prSyncTimer === undefined) {
        this.prSyncTimer = window.setInterval(
          () => void this.prSyncCycle(),
          60_000,
        );
      }
    } else if (this.prSyncTimer !== undefined) {
      window.clearInterval(this.prSyncTimer);
      this.prSyncTimer = undefined;
    }
  }

  /** Un `pr_sync` borné — partagé par le cycle de l'onglet Repos et le
   *  tick badge : vieux démon sans la méthode → `prSyncDead` coupe la
   *  demande (réarmé à l'activation de l'onglet). */
  private async prSyncRequest() {
    if (this.prSyncDead) return;
    try {
      const r = await call<Record<string, { error?: string }>>("pr_sync");
      // Fusion multi-conns : les échecs remontent en {conn:{error}} —
      // 100% « méthode inconnue » = tous les démons sont trop vieux
      // pour pr_sync → on arrête de demander (une erreur réseau
      // n'arme pas le flag : la retente passe au prochain tick).
      const errs = Object.values(r ?? {}).map((x) => String(x?.error ?? ""));
      if (
        errs.length > 0 &&
        errs.every((e) => /inconnue|unknown|variant/i.test(e))
      ) {
        this.prSyncDead = true;
      }
    } catch (e) {
      if (/inconnue|unknown|variant/i.test(String(e))) this.prSyncDead = true;
    }
  }

  /** pr_sync puis refresh : le premier rendu (refresh() immédiat, à
   *  l'ouverture de l'onglet) montre le registre courant ; celui-ci
   *  reflète les états live dès que la forge a répondu. */
  private async prSyncCycle() {
    await this.prSyncRequest();
    if (this.view === "repos") await this.refresh();
  }

  /** Visibilité des panneaux + état visuel de l'onglet actif. */
  private applyView() {
    const v = this.view;
    const m = this.mapOpen;
    $("sv-agents").classList.toggle("on", !m && v === "agents");
    $("sv-repos").classList.toggle("on", !m && v === "repos");
    $("sv-backlog").classList.toggle("on", !m && v === "backlog");
    $("sv-map").classList.toggle("on", m);
    $("sv-agents").setAttribute("aria-selected", String(!m && v === "agents"));
    $("sv-repos").setAttribute("aria-selected", String(!m && v === "repos"));
    $("sv-backlog").setAttribute("aria-selected", String(!m && v === "backlog"));
    $("sv-map").setAttribute("aria-selected", String(m));
    $("session-list").hidden = v !== "agents";
    $("pr-list").hidden = v !== "repos";
    $("backlog-list").hidden = v !== "backlog";
    $("btn-new-session").hidden = v !== "agents";
    // L'onglet backlog pilote sa cadence de fetch (rapide actif, lente
    // pour le badge inactif) — la bascule se fait ici. Pareil pour
    // le poll pr_sync de l'onglet repos (arrêté hors de la vue).
    this.backlogView.setActive(v === "backlog");
    this.prsSetActive(v === "repos");
    this.renderView();
  }

  /** Event session poussé par le démon (pushes = session ouverte
   *  seulement) → invalidation debouncée de la file et du badge. */
  noteSessionEvent(type: string) {
    this.backlogView.noteSessionEvent(type);
  }

  /** Capabilities (re)connues : la vue backlog peut devenir capable
   *  sans action — poke pour peindre son badge dès que possible (au
   *  lieu du premier tick ~60 s après le boot). */
  noteCapsChanged() {
    this.backlogView.noteCaps();
  }

  /** Filtre de l'arbre (normalisé : trim + bas de casse) — vide = tout. */
  private filter = "";

  /** Surligne la 1re occurrence du filtre dans un libellé (HTML sûr :
   *  chaque segment passe par esc(), seul le <mark> est injecté). */
  private hl(txt: string): string {
    const f = this.filter;
    txt = txt ?? ""; // champ absent du fil (vieux démon) → "" plutôt que TypeError/"undefined"
    if (!f) return esc(txt);
    const i = txt.toLowerCase().indexOf(f);
    if (i < 0) return esc(txt);
    return esc(txt.slice(0, i)) + "<mark>" + esc(txt.slice(i, i + f.length)) + "</mark>" +
      esc(txt.slice(i + f.length));
  }

  /** Rendu de #session-list depuis app.lastSessions — appelé par refresh()
   *  et à chaque frappe du filtre (pas de RPC : la donnée est déjà là).
   *  Filtre actif : seules les branches avec ≥1 session visible survivent,
   *  tout est déplié, et la 1re occurrence du filtre est surlignée. */
  private renderList() {
    // Une édition de titre est en cours : reconstruire la liste
    // détacherait l'input (blur → fin d'édition) — rebuild différé, le
    // dernier done() rejoue renderList(). Couvre les deux appelants :
    // refresh() (status_changed d'une session qui travaille) et les
    // frappes du filtre. Le flag se consomme au prochain rendu réel.
    // Même garde pendant un drag : détacher l'élément source tuerait le
    // drag anormalement (curseur "grabbing" résiduel — voir init()).
    if (this.editCount > 0 || this.dragging > 0) {
      this.rebuildPending = true;
      return;
    }
    this.rebuildPending = false;
    const el = $("session-list");
    el.innerHTML = "";
    const f = this.filter;
    const hit = (txt: string) => (txt ?? "").toLowerCase().includes(f);
    // Rendu : connexion → groupes nommés (le store client garde les groupes
    // VIDES visibles hors filtre) → sessions non groupées.
    const byConn = new Map<string, SessionMeta[]>();
    for (const s of app.lastSessions) {
      if (this.pendingDel.has(s.id)) continue;
      const c = s.conn ?? "default";
      if (!byConn.has(c)) byConn.set(c, []);
      byConn.get(c)!.push(s);
    }
    // Hors filtre : conns suivies même sans session (ex. serveur down) →
    // header + badge. Filtré : une conn sans session visible disparaît.
    const connKeys = [
      ...new Set([...(f ? [] : this.conns.knownConns()), ...byConn.keys()]),
    ].sort();
    let shown = 0;
    for (const conn of connKeys) {
      const sessions = (byConn.get(conn) ?? []).sort((a, b) => a.title.localeCompare(b.title));
      // Une connexion ou un groupe dont le NOM matche affiche toutes ses
      // sessions (sinon l'utilisateur ne verrait que les titres chanceux).
      // Le hit porte sur le chemin complet : « a » matche « a/b » — le
      // sous-arbre du groupe nommé reste visible (sémantique « groupe »
      // étendue aux descendants).
      const connHit = !!f && hit(conn);
      const pass = (s: SessionMeta) =>
        !f || connHit || hit(s.group ? grpNorm(s.group) : "") || hit(s.title) ||
        hit(s.agent) || hit(s.origin ?? "");

      // Arbre des groupes : chemins stockés (groupes VIDES visibles hors
      // filtre, ancêtres explicites) + chemins portés par les sessions ;
      // un nœud préfixe sans session propre est affiché quand même.
      const gRoot = new Map<string, GNode>();
      const ensure = (path: string): GNode => {
        let kids = gRoot, acc = "";
        let node: GNode = { path: "", name: "", kids: gRoot, rows: [] };
        for (const seg of path.split("/")) {
          acc = acc ? `${acc}/${seg}` : seg;
          let n = kids.get(seg);
          if (!n) {
            n = { path: acc, name: seg, kids: new Map(), rows: [] };
            kids.set(seg, n);
          }
          node = n;
          kids = n.kids;
        }
        return node;
      };
      if (!f) {
        for (const p of this.groups.stored(conn)) {
          const n = grpNorm(p);
          if (n) ensure(n);
        }
      }
      for (const s of sessions) {
        const g = s.group ? grpNorm(s.group) : "";
        if (g && pass(s)) ensure(g).rows.push(s);
      }
      // Groupe qui se normalise en "" (méta mal formée ex. "a//b" → ok,
      // "  " → vide) : la session retombe non groupée plutôt que perdue.
      const plain = sessions.filter((x) => !grpNorm(x.group ?? "") && pass(x));
      // Filtré : un nœud sans aucune session visible dans son sous-arbre
      // disparaît (règle inchangée, portée à la profondeur).
      const prune = (n: GNode): boolean => {
        for (const [k, kid] of [...n.kids]) if (!prune(kid)) n.kids.delete(k);
        return n.rows.length + n.kids.size > 0;
      };
      if (f) for (const [k, n] of [...gRoot]) if (!prune(n)) gRoot.delete(k);
      const countRows = (n: GNode): number =>
        n.rows.length + [...n.kids.values()].reduce((a, k) => a + countRows(k), 0);
      const treeRows = [...gRoot.values()].reduce((a, n) => a + countRows(n), 0);
      // Filtré : conn sans aucune session visible → pas de header. Hors
      // filtre : le header s'affiche même sans session (conns suivies).
      if (f && !gRoot.size && !plain.length) continue;
      shown += plain.length + treeRows;

      const key = `ac-grp-${conn}`;
      const connClosed = !f && localStorage.getItem(key) === "1";
      const st = this.conns.state(conn) ?? "ok";
      const ch = document.createElement("div");
      ch.className = "grp-head" + (connClosed ? " closed" : "");
      const badge = this.conns.hasCfg(conn)
        ? `<span class="conn-st ${st === "ok" ? "ok" : st === "reconnecting" ? "warn" : "bad"}"` +
          ` title="${q(esc(this.conns.tip(st, conn)))}">${st === "reconnecting" ? icon("refresh") : icon("dot")}</span> `
        : "";
      ch.innerHTML = `<span class="grp-arrow">${icon(connClosed ? "chev-right" : "chev-down")}</span> ${badge}<span class="grp-name">${this.hl(conn)}</span>` +
        `<button class="grp-add" title="${esc(t("new_group"))}">${icon("folder-plus")}` +
        `<span class="grp-add-lbl">${esc(t("new_group_lbl"))}</span></button>`;
      ch.onclick = () => {
        // Sous filtre l'arbre est forcé déplié : toggler persisterait un
        // état invisible à l'écran (+ RPC refresh inutile).
        if (this.filter) return;
        localStorage.setItem(key, connClosed ? "0" : "1");
        void this.refresh();
      };
      ch.querySelector(".grp-add")!.addEventListener("click", (e) => {
        e.stopPropagation();
        this.groups.openDlg("new", null, conn);
      });
      // Badge ≠ ok → clic = reconnecter maintenant (retry simple).
      ch.querySelector(".conn-st")?.addEventListener("click", (e) => {
        e.stopPropagation();
        if ((this.conns.state(conn) ?? "ok") !== "ok") this.conns.retry(conn);
      });
      // Déposer sur le header connexion = racine : dégroupe une session,
      // remonte un sous-arbre (move(..., "") — comme un drop sans parent).
      this.bindDropTarget(ch, conn, undefined);
      bindCtx(ch, (x, y) => this.conns.openCtx(x, y, conn));
      el.appendChild(ch);

      // Rendu récursif d'un nœud : header pliable (état persisté par
      // CHEMIN COMPLET) → sous-groupes triés → sessions membres (sous-
      // agents imbriqués juste après leur parent via nestSessions).
      // Sémantique arbre : un ancêtre plié cache le nœud ENTIER (header
      // inclus), pas seulement ses sessions — contrairement à l'ancien
      // rendu plat où les headers de groupe survivaient au pliage conn.
      const firstMember = (n: GNode): SessionMeta | undefined => {
        if (n.rows[0]) return n.rows[0];
        for (const k of n.kids.values()) {
          const m = firstMember(k);
          if (m) return m;
        }
        return undefined;
      };
      const renderNode = (n: GNode, gDepth: number, foldedAbove: boolean) => {
        const gkey = `ac-grp-${conn}::${n.path}`;
        const folded = !f && localStorage.getItem(gkey) === "1";
        const g = document.createElement("div");
        g.className = "grp-sub" + (folded ? " closed" : "");
        if (foldedAbove) g.style.display = "none";
        if (gDepth > 0) g.style.paddingLeft = `${18 + 14 * gDepth}px`;
        g.title = t("drop_to_group"); // tooltip hors nom (le nom porte le chemin)
        g.innerHTML = `<span class="grp-arrow">${icon(folded ? "chev-right" : "chev-down")}</span> ` +
          `<span class="grp-name" title="${q(esc(n.path))}">${this.hl(n.name)}</span>` +
          `<button class="grp-add" title="${esc(t("new_sess_grp"))}">${icon("user-plus")}` +
          `<span class="grp-add-lbl">${esc(t("new_sess_grp_lbl"))}</span></button>`;
        g.onclick = () => {
          if (this.filter) return; // idem : pas de toggle invisible sous filtre
          localStorage.setItem(gkey, localStorage.getItem(gkey) === "1" ? "0" : "1");
          void this.refresh();
        };
        // + : nouvel agent dans le groupe — agent + workspace du 1er
        // membre du sous-arbre préremplis (tri titre → "premier affiché").
        const ref = firstMember(n);
        g.querySelector(".grp-add")!.addEventListener("click", (e) => {
          e.stopPropagation();
          const apiConn = conn === "default" ? undefined : conn;
          this.newSess.setPendingGroup({ conn: apiConn, name: n.path });
          void this.newSess.open(apiConn, ref);
        });
        bindCtx(g, (x, y) => this.openGroupCtx(x, y, conn, n.path, ref));
        this.bindDropTarget(g, conn, n.path);
        this.bindGroupDrag(g, conn, n.path);
        el.appendChild(g);
        const innerFolded = foldedAbove || folded;
        for (const k of [...n.kids.values()].sort((a, b) => a.name.localeCompare(b.name))) {
          renderNode(k, gDepth + 1, innerFolded);
        }
        for (const { s, depth } of nestSessions(n.rows)) {
          el.appendChild(this.sessRow(s, conn, gDepth + 1, innerFolded, depth));
        }
      };
      for (const n of [...gRoot.values()].sort((a, b) => a.name.localeCompare(b.name))) {
        renderNode(n, 0, connClosed);
      }
      for (const { s, depth } of nestSessions(plain)) {
        el.appendChild(this.sessRow(s, conn, 0, connClosed, depth));
      }
    }
    if (f && !shown) {
      const none = document.createElement("div");
      none.className = "sess-none";
      none.textContent = t("sess_filter_none");
      el.appendChild(none);
    }
  }

  /** Payload du drag en cours — renseigné au dragstart (ligne session ou
   *  header de groupe). dataTransfer.getData n'est lisible qu'au drop :
   *  sans ce champ, impossible de refuser les cibles invalides pendant
   *  le survol (drop dans son propre sous-arbre, no-op…). */
  private dragPayload: DragPayload | null = null;

  /** Cible de drop : conn = câble (inter-serveur toujours refusé),
   *  group = chemin cible (undefined = racine → dégroupe une session,
   *  remonte un sous-arbre). Les cibles qui ne changeraient rien ou
   *  casseraient l'arbre n'allument pas le hint et refusent le drop. */
  private bindDropTarget(el: HTMLElement, conn: string, group: string | undefined) {
    const target = group ?? "";
    const ok = (p: DragPayload | null): boolean => {
      if (!p || p.conn !== conn) return false;
      if (p.kind === "group") {
        // Pas sur soi-même, pas dans son propre sous-arbre, pas sur son
        // parent actuel (déjà à cet endroit).
        return target !== p.path && !(target && inSubtree(target, p.path)) &&
          grpParent(p.path) !== target;
      }
      const s = app.lastSessions.find((x) => x.id === p.id);
      return !!s && grpNorm(s.group ?? "") !== target;
    };
    el.addEventListener("dragover", (e) => {
      if (e.dataTransfer?.types.includes(DTYPE) && ok(this.dragPayload)) {
        e.preventDefault();
        el.classList.add("drop-hint");
      }
    });
    el.addEventListener("dragleave", () => el.classList.remove("drop-hint"));
    el.addEventListener("drop", async (e) => {
      e.preventDefault();
      el.classList.remove("drop-hint");
      try {
        const d = JSON.parse(e.dataTransfer!.getData(DTYPE)) as DragPayload;
        if (!ok(d)) return;
        if (d.kind === "group") {
          // Sous-arbre entier déplacé : meta.group de toutes les sessions
          // sous `d.path` + chemins stockés + clés de collapse migrés.
          if (!(await this.groups.move(conn, d.path, target))) {
            showErr(t("group_path_invalid"));
          }
        } else {
          const s = app.lastSessions.find((x) => x.id === d.id);
          if (!s) return;
          await this.groups.applyAssign(s, target || undefined);
        }
        void this.refresh();
      } catch {
        /* donnée étrangère : ignorer */
      }
    });
  }

  /** Header de groupe = source de drag du sous-arbre entier. Desktop
   *  seulement — pas de DnD tactile : sur Android, déplacer un groupe
   *  = éditer son chemin dans le dialogue renommer. */
  private bindGroupDrag(el: HTMLElement, conn: string, path: string) {
    if (isAndroid()) return;
    el.draggable = true;
    el.addEventListener("dragstart", (e) => {
      const p: DragPayload = { kind: "group", path, conn };
      e.dataTransfer!.setData(DTYPE, JSON.stringify(p));
      e.dataTransfer!.effectAllowed = "move";
      this.dragPayload = p;
      // dragend → endDrag() (listener document, init) solde dragPayload.
    });
  }

  /** Menu contextuel session (clic droit / appui long) : rename / fork /
   *  groupe / exit — fixe au point cliqué sur desktop, action-sheet ancrée
   *  en bas sur Android. */
  private openSessionCtx(x: number, y: number, s: SessionMeta, row: HTMLElement) {
    this.ctx.innerHTML = `<div class="fctx-path">${esc(s.title)}</div>`;
    menuBtn(this.ctx, t("ctx_rename"), "", () => {
      hideSessCtx();
      this.renameSessRow(s, row);
    });
    // conn: s.conn — la session peut vivre sur une autre connexion que
    // celle affichée ; sans ça call() routerait vers le mauvais démon.
    const forkBtn = menuBtn(this.ctx, t("ctx_fork"), "", () => {
      hideSessCtx();
      if (app.current?.id === s.id) this.chat.sysline(t("fork_started"));
      void call<{ session: string }>("fork_session", { session: s.id, conn: s.conn })
        .then(async (r) => {
          await this.refresh();
          if (r.session) await this.openSession(r.session);
        })
        .catch((e) => showErr(String(e)));
    });
    // Fork = respawn depuis la session native — impossible sans id natif.
    forkBtn.disabled = !s.native_session_id;
    menuBtn(this.ctx, t("ctx_group"), "", () => {
      hideSessCtx();
      this.groups.openDlg("session", s);
    });
    // Endormir = libère le process agent (la session native reste
    // reprenable — réveil auto au prochain envoi). Sans objet si la
    // session n'a déjà plus de process (dead/interrupted/asleep).
    const sleepBtn = menuBtn(this.ctx, t("ctx_sleep"), "", () => {
      hideSessCtx();
      void call("sleep_session", { session: s.id, conn: s.conn })
        .then(() => this.refresh())
        .catch((e) => showErr(String(e)));
    });
    sleepBtn.disabled = !!s.offline ||
      s.state === "asleep" || s.state === "dead" || s.state === "interrupted";
    // Exit = tue le process agent (kill_session) — la session reste, reprenable.
    // Action destructive → double-clic armé comme delete_path / sess-del.
    menuBtn(this.ctx, t("ctx_exit"), "danger", (b) => {
      if (!b.dataset.armed) {
        b.dataset.armed = "1";
        b.textContent = t("ctx_confirm");
        return;
      }
      hideSessCtx();
      // Kill volontaire d'une session en starting : pas un échec de
      // spawn — retirer le watch pour ne pas toaster « n'a pas démarré ».
      app.spawnWatch.delete(s.id);
      void call("kill_session", { session: s.id, conn: s.conn })
        .then(() => this.refresh())
        .catch((e) => showErr(String(e)));
    });
    placeMenu(this.ctx, "sctx", x, y);
  }

  /** Menu contextuel d'un groupe nommé (clic droit / appui long) : nouvelle
   *  session dans le groupe / renommer / dissoudre — même composant #sctx
   *  que les menus session/fichier/connexion (action-sheet sur Android).
   *  `ref` = 1er membre affiché, préremplit agent + workspace (comme "+"). */
  private openGroupCtx(
    x: number, y: number, conn: string, grp: string, ref?: SessionMeta,
  ) {
    this.ctx.innerHTML = `<div class="fctx-path">${esc(grp)}</div>`;
    menuBtn(this.ctx, t("new_sess_grp"), "", () => {
      hideSessCtx();
      const apiConn = conn === "default" ? undefined : conn;
      this.newSess.setPendingGroup({ conn: apiConn, name: grp });
      void this.newSess.open(apiConn, ref);
    });
    menuBtn(this.ctx, t("new_subgroup"), "", () => {
      hideSessCtx();
      this.groups.openDlg("new", null, conn, "", grp);
    });
    menuBtn(this.ctx, t("rename_group"), "", () => {
      hideSessCtx();
      this.groups.openDlg("rename", null, conn, grp);
    });
    // Dissoudre dégroupe tout le SOUS-ARBRE (sessions du nœud et de ses
    // descendants) — destructif → double-clic armé comme ctx_exit /
    // sess-del (label dédié : on dégroupe, on ne supprime pas).
    const nMembers = this.groups.subtreeSize(conn, grp);
    menuBtn(this.ctx, t("dissolve_group"), "danger", (b) => {
      if (!b.dataset.armed) {
        b.dataset.armed = "1";
        b.textContent = t("ctx_dissolve_armed", { n: nMembers });
        return;
      }
      hideSessCtx();
      void this.groups.dissolve(conn, grp)
        .then(() => this.refresh())
        .catch((e) => showErr(String(e)));
    });
    placeMenu(this.ctx, "sctx", x, y);
  }

  /** Renommage inline du titre dans la ligne session (même pattern que le
   *  double-clic sur chat-title). */
  private renameSessRow(s: SessionMeta, row: HTMLElement) {
    const span = row.querySelector<HTMLElement>(".t");
    if (!span) return;
    const inp = document.createElement("input");
    inp.className = "title-edit";
    inp.value = s.title;
    span.replaceWith(inp);
    row.draggable = false; // sinon un drag depuis l'input part en drag de ligne
    inp.focus();
    inp.select();
    // Tant que l'édition est ouverte, refresh() diffère son rebuild de
    // liste (un status_changed d'une session qui travaille ne doit pas
    // tuer la saisie) ; done() le déclenche à la fin.
    this.editCount++;
    let finished = false;
    const done = async (save: boolean) => {
      if (finished) return;
      finished = true;
      if (!isAndroid()) row.draggable = true;
      const v = inp.value.trim();
      if (inp.isConnected) inp.replaceWith(span);
      const changed = !!(save && v && v !== s.title);
      if (changed) {
        s.title = v;
        span.textContent = v;
        if (app.current?.id === s.id) {
          app.current.title = v;
          this.chat.setChatTitle();
        }
        await call("rename_session", { session: s.id, title: v, conn: s.conn })
          .catch((e) => showErr(String(e)));
      }
      this.editCount--;
      // Refresh seulement si utile : sauvegarde (retri par titre +
      // resync complet) ou rebuilds différés à rejouer — renderList()
      // suffit, les données sont déjà à jour (lastSessions et filtre ont
      // continué de bouger pendant l'édition). Un Escape/blur sans
      // changement ne vaut pas un round-trip.
      if (changed) void this.refresh();
      else if (this.editCount === 0 && this.rebuildPending) this.renderView();
    };
    inp.addEventListener("click", (e) => e.stopPropagation());
    inp.addEventListener("keydown", (e) => {
      if (e.key === "Enter") done(true);
      if (e.key === "Escape") done(false);
    });
    inp.addEventListener("blur", () => done(true));
  }

  private sessRow(s: SessionMeta, conn: string, gDepth: number, closed: boolean, depth = 0): HTMLElement {
    const div = document.createElement("div");
    div.dataset.sid = s.id;
    div.className = "sess" + (gDepth ? " in-grp" : "") + (depth > 0 ? " sub" : "") +
      (s.offline ? " offline" : "") + (app.current?.id === s.id ? " active" : "");
    if (closed) div.style.display = "none";
    // Indentation = profondeur de groupe (14px/niveau) + profondeur de
    // sous-agent (14px/niveau, cap visuel — les arbres de délégation
    // restent peu profonds), tooltip = provenance.
    const indent = gDepth * 14 + Math.min(depth, 4) * 14;
    if (indent > 0) div.style.marginLeft = `${indent}px`;
    if (s.spawned_by) {
      const parent = app.lastSessions.find((x) => x.id === s.spawned_by);
      div.title = t("spawned_by", { parent: parent?.title ?? s.spawned_by });
    } else if (s.origin) {
      div.title = t("origin_tip", { origin: s.origin });
    }
    const prs = this.prBySess.get(`${conn}${s.id}`) ?? [];
    const prHtml = prs.length
      ? `<span class="sess-pr" title="${q(esc(prs.map((p) => `${repoLabel(p)} ${prBadge(p)}${p.title ? ` — ${p.title}` : ""}${p.status ? ` (${p.status})` : ""}`).join("\n")))}">${prs.map((p) => prBadge(p)).join(" ")}</span>`
      : "";
    const originHtml = s.origin
      ? `<span class="sess-origin" title="${q(esc(t("origin_tip", { origin: s.origin })))}">${t("via")} ${this.hl(s.origin.replace(/^component:/, "") || s.origin)}</span>`
      : "";
    div.innerHTML = `<span class="dot st-${dotCls(s.state)}" title="${q(esc(s.state))}">${dotIcon(s.state)}</span><span class="t">${this.hl(s.title)}</span>${originHtml}${prHtml}<span class="a">${this.hl(s.agent)}</span><button class="sess-del" title="Supprimer">${icon("close")}</button>`;
    div.onclick = () => {
      void this.openSession(s.id);
      // Mobile : fermer le drawer après sélection.
      nav.closeDrawer();
    };
    bindCtx(div, (x, y) => this.openSessionCtx(x, y, s, div));
    if (!isAndroid()) {
      div.draggable = true;
      div.addEventListener("dragstart", (e) => {
        const p: DragPayload = { kind: "session", id: s.id, conn };
        e.dataTransfer!.setData(DTYPE, JSON.stringify(p));
        e.dataTransfer!.effectAllowed = "move";
        this.dragPayload = p; // soldé par endDrag() au dragend
      });
    }
    const del = div.querySelector<HTMLElement>(".sess-del")!;
    del.addEventListener("click", (e) => {
      e.stopPropagation();
      if (!del.dataset.armed) {
        del.dataset.armed = "1";
        del.textContent = t("delete_armed");
        setTimeout(() => { del.dataset.armed = ""; del.innerHTML = icon("close"); }, 2500);
        return;
      }
      // Optimiste : la ligne disparaît sans attendre le démon (kill de
      // l'agent + purge des fichiers peuvent durer). L'event
      // session_deleted confirmera ; en cas d'erreur la ligne revient.
      this.pendingDel.add(s.id);
      if (app.current?.id === s.id) {
        // Session affichée : markGone fait le ménage complet (conv
        // cachée, current, gem, boutons, badge "gone", updateHeader).
        // L'event session_deleted trouvera app.current déjà null → il
        // ne repassera pas par là.
        this.chat.markGone();
      } else {
        this.chat.dropConv(s.id);
      }
      div.remove();
      // Suppression volontaire : même garde que kill — pas un échec de
      // spawn à notifier (checkSpawns l'oublierait de toute façon à la
      // sortie de liste, mais autant ne pas toaster entre-temps).
      app.spawnWatch.delete(s.id);
      void this.refresh();
      void call("delete_session", { session: s.id, conn: s.conn })
        .catch((er) => {
          showErr(String(er));
          this.pendingDel.delete(s.id);
          // Échec → la session existe encore : la réinjecter dans le
          // cache liste pour qu'elle réapparaisse au refresh (même
          // offline si la connexion est tombée entre-temps) — sauf si
          // sa connexion a été supprimée entre-temps.
          if (this.conns.hasCfg(s.conn ?? "local") &&
              !app.lastSessions.some((x) => x.id === s.id)) {
            app.lastSessions.push(s);
          }
        })
        .finally(() => void this.refresh());
    });
    return div;
  }

  init() {
    // Sélecteur de vue agents ↔ repos ↔ backlog : état persisté,
    // restauration avant le premier refresh (repos se synchronise
    // dessus, backlog démarre son fetch borné via applyView).
    const sv = localStorage.getItem("ac-sideview");
    this.view = sv === "repos" || sv === "backlog" ? sv : "agents";
    this.applyView();
    // Sélection d'un autre onglet que « Diagramme » = navigation : la
    // vue diagramme se ferme, comme le font les clics session (elle ne
    // persiste pas par-dessus le contenu demandé — no-op si déjà fermée).
    $("sv-agents").onclick = () => { this.map.close(); this.setView("agents"); };
    $("sv-repos").onclick = () => { this.map.close(); this.setView("repos"); };
    $("sv-backlog").onclick = () => { this.map.close(); this.setView("backlog"); };

    // Badge « Repos » tenu à jour hors de l'onglet : pr_list = lecture
    // locale du registre (léger — pas de sync forge), cadence 60 s.
    this.prCountTimer = window.setInterval(() => void this.prCountTick(), 60_000);

    // Filtre de l'arbre : recherche par nom de session / agent / groupe /
    // connexion. Rendu local instantané — pas de debounce, pas de RPC.
    const fIn = $("sess-filter-input") as HTMLInputElement;
    const fClr = $("sess-filter-clear");
    const applyFilter = () => {
      this.filter = fIn.value.trim().toLowerCase();
      fClr.hidden = !this.filter;
      this.renderView();
    };
    fIn.addEventListener("input", applyFilter);
    fIn.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && fIn.value) {
        fIn.value = "";
        applyFilter();
        e.stopPropagation(); // ne pas fermer un menu ctx en même temps
      }
    });
    fClr.addEventListener("click", () => {
      fIn.value = "";
      applyFilter();
      fIn.focus();
    });
    // Ctrl/Cmd+F sans session ouverte → filtre d'arbre (avec session, le
    // handler de search.ts prend la main pour la recherche d'historique).
    // Pas sous un <dialog> ouvert : l'input invisible volerait les frappes.
    window.addEventListener("keydown", (e) => {
      if ((e.ctrlKey || e.metaKey) && (e.key === "f" || e.key === "F") &&
          !app.current && !document.querySelector("dialog[open]")) {
        e.preventDefault();
        fIn.focus();
        fIn.select();
      }
    });

    // Fermeture du menu ctx : clic ailleurs / Échap / scroll de la liste.
    // Un clic DANS le menu ne doit pas le fermer avant l'action du bouton.
    this.ctx.addEventListener("click", (e) => e.stopPropagation());
    window.addEventListener("click", hideSessCtx);
    window.addEventListener("keydown", (e) => {
      if (e.key === "Escape") hideSessCtx();
    });
    $("session-list").addEventListener("scroll", hideSessCtx);

    // ── Cycle de vie du drag & drop ──────────────────────────────
    // Un rebuild de la liste pendant un drag détache l'élément source
    // → le drag meurt de façon anormale et, sous WebKitGTK/Wayland,
    // le curseur "grabbing" (main fermée) reste collé sans recours
    // possible côté web (bug toolkit connu). Trois gardes :
    //  · compteur de drags en vol → renderList() diffère ses rebuilds ;
    //  · drop d'une session hors cible avalé au niveau document =
    //    annulation propre partout (le chemin "drop refusé" déclenche
    //    le même curseur collé) ;
    //  · dragend solde le compteur, purge les .drop-hint qu'un drag
    //    annulé (Échap) laisserait allumés et oublie dragPayload
    //    (sinon les hints resteraient pilotés par un payload périmé) ;
    //    mouseup sert de watchdog si le toolkit tue le drag sans
    //    émettre dragend — endDrag est l'unique point « drag mort ».
    const endDrag = () => {
      this.dragging = 0;
      this.dragPayload = null;
      document
        .querySelectorAll(".drop-hint")
        .forEach((el) => el.classList.remove("drop-hint"));
      if (this.rebuildPending) this.renderList();
    };
    document.addEventListener("dragstart", () => {
      this.dragging++;
    });
    document.addEventListener("dragend", endDrag);
    document.addEventListener("dragover", (e) => {
      if (e.dataTransfer?.types.includes(DTYPE)) e.preventDefault();
    });
    document.addEventListener("drop", (e) => {
      if (e.dataTransfer?.types.includes(DTYPE)) e.preventDefault();
    });
    document.addEventListener("mouseup", () => {
      // Aucun mouseup n'atteint le document pendant un drag vivant :
      // en recevoir un alors que dragging>0 = drag mort sans dragend.
      if (this.dragging) endDrag();
    });

    // Redimensionnement de la sidebar : drag sur la poignée bord droit.
    // Largeur persistée dans localStorage.
    const bar = $("sidebar");
    const grip = $("sidebar-resize");
    // Pas de plafond arbitraire : la sidebar peut prendre toute la
    // fenêtre moins un plancher pour le contenu principal (320 = le
    // min-width des blocs de chat). Sur fenêtre très étroite le cap
    // reste ≥ le min de la sidebar (elle ne compresse pas sous 180).
    const clamp = (x: number) =>
      Math.min(Math.max(180, window.innerWidth - 320), Math.max(180, x));
    const applyW = (w: number) => {
      bar.style.width = w + "px";
      bar.style.minWidth = w + "px";
    };
    const saved = Number(localStorage.getItem("sidebar-w"));
    if (saved && !isAndroid()) applyW(clamp(saved));
    // Fenêtre rétrécie : re-plafonner l'affichage — sinon la poignée
    // sort de l'écran et la sidebar devient irréductible. localStorage
    // n'est pas touché : la largeur voulue survit au ré-agrandissement.
    // Android : la sidebar est un drawer 100vw (grip caché) — un
    // re-plafonnement au resize le casserait (rotation, clavier).
    if (!isAndroid()) {
      window.addEventListener("resize", () => {
        if (bar.offsetWidth) applyW(clamp(bar.offsetWidth));
      });
    }
    grip.addEventListener("mousedown", (e) => {
      e.preventDefault();
      grip.classList.add("drag");
      document.body.style.userSelect = "none";
      const move = (ev: MouseEvent) => {
        // Bouton relâché hors fenêtre → mouseup perdu : auto-relâcher
        // (sinon userSelect reste "none" et le grip suit la souris).
        if (ev.buttons === 0) {
          up(ev);
          return;
        }
        applyW(clamp(ev.clientX));
      };
      const up = (ev: MouseEvent) => {
        document.removeEventListener("mousemove", move);
        document.removeEventListener("mouseup", up);
        grip.classList.remove("drag");
        document.body.style.userSelect = "";
        localStorage.setItem("sidebar-w", String(clamp(ev.clientX)));
      };
      document.addEventListener("mousemove", move);
      document.addEventListener("mouseup", up);
    });
  }
}
