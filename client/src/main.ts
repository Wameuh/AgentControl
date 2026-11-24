// ---------------------------------------------------------------------------
// Bootstrap : câble les modules entre eux. Le comportement vit dans
//   core/      — helpers DOM, RPC, état transversal, chrome, formatage, conns
//   ui/        — pile d'overlays Android + menus contextuels partagés
//   features/  — connexions, sidebar, vue conversation, composer, fichiers,
//                dialogues, barre de statut, mise à jour
// ---------------------------------------------------------------------------

import "./styles.css";
import "./mesh-bg.ts";
import { invoke, listen, isWeb } from "./core/bridge";
import { $, logUi, notify, copyText } from "./core/dom";
import { app } from "./core/state";
import { initRpc } from "./core/rpc";
import { initModalGuard } from "./core/modal";
import { connKeyOf, connLabel } from "./core/conn";
import {
  applyTheme,
  applyAccent,
  accentPref,
  accentOf,
  applyAccentStrong,
  themePref,
  accentStrongPref,
  applyConnChrome,
  initGems,
  initPlatform,
  isAndroid,
  initWindowChrome,
  initExternalLinks,
  shellOpen,
} from "./core/chrome";
import { nav } from "./ui/nav";
import { hydrateIcons } from "./ui/icons";
import { initStatus, type HostStats } from "./features/status";
import { initMemDialog } from "./features/meminfo";
import { checkBootUpdate, openAbout } from "./features/update";
import { UpdatesPanel } from "./features/updates-panel";
import { OptionsMenu } from "./features/options-menu";
import { initUnsavedDialog } from "./core/unsaved";
import { fitViewTabs, initViewTabsFit } from "./ui/fit-tabs";
import { initModelBar } from "./features/modelbar";
import { initModelCosts } from "./features/modelcosts";
import { initModelCatalog } from "./features/modelcatalog";
import { initFusionDialog, openFusion } from "./features/fusion-dialog";
import { GroupStore } from "./features/groups";
import { NewSessionDialog } from "./features/new-session";
import { AgentCapsDialog } from "./features/agentcaps";
import { ConnectionManager } from "./features/connections";
import { ConnView } from "./features/conn-view";
import { EventRenderer } from "./features/event-renderers";
import { ChatView } from "./features/chat-view";
import { SessionSearch } from "./features/search";
import { Sidebar } from "./features/sidebar";
import { Composer } from "./features/composer";
import { FileExplorer } from "./features/files";
import { QuickOpen } from "./features/quickopen";
import { MapView } from "./features/map-view";
import { TermPanel } from "./features/terminal";
import { SettingsPanel } from "./features/settings";
import { Tts } from "./features/tts";
import { ComponentsPanel, ORCH_COMPONENT } from "./features/components";
import { LogsPanel } from "./features/logs";
import { PluginView } from "./features/plugin-view";
import { stateDiff } from "./features/state-diff";
import { sounds } from "./features/sounds";
import { notifications } from "./features/notifications";
import { initFirstPost } from "./features/first-post";
import { initSessionBar, renderSessionBar } from "./mobile/session-bar";
import { initSessionSheet, openSessionSheet } from "./mobile/session-sheet";
import { initConnSheet } from "./mobile/conn-sheet";
import { initHomeShell, syncHomeConn } from "./mobile/shell";
import { initInbox } from "./mobile/inbox";
import { initGestures } from "./mobile/gestures";
import { initMapGestures } from "./mobile/map-gestures";
import { initPermDock } from "./mobile/perm-dock";
import { initComposerMobile } from "./mobile/composer-mobile";
import { initSettingsMobile } from "./mobile/settings-mobile";
import { tick } from "./mobile/haptics";
import { initToolRuns } from "./mobile/tool-runs";
import { initTermKeys } from "./mobile/term-keys";
import { appMenuItems } from "./mobile/app-menu";
import { Pins } from "./features/pins";
import { Steps, type SessionStep } from "./features/steps";
import { initTelemetry } from "./features/telemetry";
import { initLang, t, applyStatic } from "./i18n";
import { engineReport, debugNoCv } from "./core/engine";
import type { AcEvent, SavedConn } from "./core/types";

// Thème + langue persistés (localStorage), appliqués au boot
initLang();
applyTheme(themePref());
// Accent appliqué au boot, avant le premier rendu utile (pas de flash).
applyAccent(accentPref());
// Intensité d'accent — dimension orthogonale, classe accent-strong.
applyAccentStrong(accentStrongPref(), false);
applyStatic();
// Capacités du moteur de rendu (field-sizing / content-visibility /
// contain) — détectées au boot, loguées et affichées dans « À propos »
// : indispensable pour trancher « le fix est-il actif sur CE webview ? »
// (#294). `ac-nocv` (localStorage) neutralise content-visibility pour
// reproduire un vieux WebKitGTK sur un navigateur moderne.
if (debugNoCv()) document.documentElement.classList.add("nocv");
logUi("info", `engine: ${engineReport()}`);
initGems();
initPlatform();
initWindowChrome();
initExternalLinks();
// Spike ac-stt : prouve la jonction webview → plugin (Kotlin `SttPlugin`
// sur Android, stub Rust sur desktop, stub bridge en web).
invoke("plugin:ac-stt|stt_ping")
  .then((r) => console.debug("[stt] ping:", r))
  .catch((e) => console.warn("[stt] ping failed:", e));
// Core : état moteur/modèle/permission au boot — diagnostic logcat du
// chantier core (le bouton mic arrive avec l'item UX).
invoke("plugin:ac-stt|stt_state")
  .then((r) => console.debug("[stt] state:", r))
  .catch((e) => console.warn("[stt] state failed:", e));
// Log open/close des modals + watchdog anti-gel (dialog invisible, inert
// résiduel) — avant tout showModal pour tracer aussi les dialogs du boot.
initModalGuard();

// Le menu natif du clic droit n'a pas de sens dans l'app — chaque menu
// contextuel custom gère son propre preventDefault/stopPropagation.
// Exception : une sélection de texte active → on laisse passer l'event
// (WebView2/Android exposent un menu natif avec « Copier » ; le chat a
// en plus son propre menu « Copier » sur #events).
window.addEventListener("contextmenu", (e) => {
  if (window.getSelection()?.toString()) return;
  e.preventDefault();
});

// Modèle multi-instances : cette instance se lie à UNE connexion — celle
// demandée par --conn <clé> (spawn_instance d'une autre instance), sinon
// la dernière utilisée (1re entrée de connections.json). Son accent
// éventuel est posé avant le premier rendu utile (pas de flash de la
// couleur par défaut) ; le connect auto lui-même part en fin de boot.
const [cliConn, savedConns, otherInstances] = await Promise.all([
  invoke<string | null>("cli_conn").catch(() => null),
  invoke<SavedConn[]>("saved_connections").catch(() => [] as SavedConn[]),
  // Garde « seconde instance » : une autre instance vivante → l'auto-
  // connexion est suspendue (deux clients passifs sur la même conn se
  // concurrencent — flap de transport, notifications doublées). Un
  // --conn/AGENTCONTROL_CONN explicite reste honoré : c'est le spawn
  // délibéré de la multi-instance. Web/Android : mono-processus.
  isWeb || isAndroid()
    ? Promise.resolve([] as unknown[])
    : invoke<unknown[]>("other_instances").catch(() => [] as unknown[]),
]);
const bootConn =
  (cliConn ? savedConns.find((c) => connKeyOf(c, isAndroid()) === cliConn) : undefined) ??
  savedConns[0];
// Auto-connexion suspendue : instance déjà vivante et aucune directive
// explicite — la connexion manuelle reste possible.
const autoSuppressed = cliConn == null && otherInstances.length > 0;
// Ne teinter que si un bind va réellement suivre — avec autoconnect off,
// auto supprimé ou sans --conn, aucune conn ne sera liée : pas de couleur
// mensongère.
const willConnect =
  cliConn != null || isWeb || (!autoSuppressed && localStorage.getItem("ac-autoconnect") !== "0");
const bootAccent = accentOf(bootConn?.color);
if (willConnect && bootAccent) applyAccent(bootAccent, false);

// --- Features stateful -------------------------------------------------------

const conns = new ConnectionManager();
const groups = new GroupStore();
const renderer = new EventRenderer();
const chat = new ChatView();
const sidebar = new Sidebar();
const composer = new Composer();
const files = new FileExplorer();
const quick = new QuickOpen();
const map = new MapView();
const term = new TermPanel();
const connView = new ConnView();
const newSess = new NewSessionDialog();
const agentCaps = new AgentCapsDialog();
const settings = new SettingsPanel();
const tts = new Tts();
const comps = new ComponentsPanel();
const logsPanel = new LogsPanel();
const pluginViews = new PluginView();
const search = new SessionSearch();
const pins = new Pins();
const steps = new Steps();

// call() a besoin de la conn courante + des états live — injectés pour éviter
// l'import inverse core → features.
initRpc({
  currentConn: () => app.current?.conn,
  connState: (k) => conns.state(k),
  connGone: (k) => conns.isGone(k),
});

chat.bind(renderer, {
  setActiveRow: (id) => sidebar.setActiveRow(id),
  refresh: () => sidebar.refresh(),
  refreshQueue: () => composer.refreshQueue(),
  syncAltBtn: () => composer.syncAltBtn(),
  onSessionOpened: () => {
    // Pile de navigation mobile : l'écran Session glisse sur l'accueil —
    // point unique qui couvre tous les chemins d'ouverture (liste,
    // notif, carte du diagramme, puce de session des PRs, « À traiter »).
    nav.enterSession();
    files.onSessionOpened();
    term.onSessionSwitched();
    pins.sessionOpened();
    steps.sessionOpened();
  },
  onSessionChanged: () => {
    search.onSessionChanged();
    composer.sessionChanged();
    pins.sessionChanged();
    steps.sessionChanged();
    // Une voix qui commente un journal qu'on a quitté = bug UX → stop.
    tts.sessionChanged();
  },
  onAgentText: (ev) => tts.onAgentText(ev),
  onSessionGone: (id) => composer.dropSession(id),
  connCount: () => conns.knownConns().length,
  connsDownOrNone: () => !conns.anyUp(),
  connHome: () => connView.maybeAutoShow(),
  firstPostCapable: (c) => conns.cap(c)?.first_post === true,
  termCapable: (c) => conns.cap(c)?.terminal === true,
  pinCapable: (c) => conns.cap(c)?.pinned === true,
  stepsCapable: (c) => conns.cap(c)?.steps === true,
  capsCapable: (c) => conns.cap(c)?.agent_caps === true,
  pinsMark: (el) => pins.markNode(el),
  afterHeader: () => renderSessionBar(),
});
search.bind({ jumpToSeq: (seq) => chat.jumpToSeq(seq) });
pins.bind({
  capable: (c) => conns.cap(c)?.pinned === true,
  jumpToSeq: (seq) => chat.jumpToSeq(seq),
});
// Steps n'a pas besoin de deps injectées — la capability gate vit dans
// chat.updateHeader (btn-steps) et le sheet ⋮.
renderer.bind({
  metaChanged: () => chat.updateHeader(),
  ctxChanged: () => chat.updateCtx(),
  statusChanged: () => void sidebar.refresh(),
  openSession: (id) => void chat.open(id),
  agentText: (el, ev) => tts.attachMsg(el, ev),
});
// Canaux d'alerte branchés sur le diff partagé (state-diff) — sons et
// notifications OS restent indépendants mais voient les mêmes transitions.
// Question entrante au premier plan → vibration courte (Y4, Android
// seul — en arrière-plan c'est la notif OS qui porte le signal).
stateDiff.onTransition((kind) => sounds.alert(kind));
stateDiff.onTransition((kind) => {
  if (kind === "question" && document.hasFocus()) tick("alert");
});
stateDiff.onTransition((kind, s) => notifications.alert(kind, s));
// Annulation des notifs OS quand leur condition se résout (#184) :
// sortie de waiting_input → la « question » n'a plus lieu d'être ;
// session disparue de la liste → plus aucune notif ne la concerne.
stateDiff.onRawTransition((prev, s) => {
  if (prev === "waiting_input" && s.state !== "waiting_input") {
    notifications.questionResolved(s);
  }
});
stateDiff.onGone((key) => notifications.sessionGone(key));
notifications.bind({
  connCount: () => conns.knownConns().length,
  connState: (k) => conns.state(k),
  // Clic sur la notif → même trajet qu'une ouverture depuis la sidebar :
  // les overlays de #chat doivent d'abord sortir (motif newSess.openSession).
  openSession: (id, conn) => {
    settings.close();
    connView.close();
    map.close();
    comps.close();
    logsPanel.close();
    pluginViews.close();
    void chat.open(id, false, conn);
  },
});
sidebar.bind(conns, groups, newSess, chat, settings, connView, comps, map, logsPanel, pluginViews);
composer.bind(chat);
composer.refresh = () => sidebar.refresh();
// Hint « modèle absent » du bouton mic → ouvre les settings directement
// sur la section de téléchargement STT (details dépliée + scroll).
composer.mic.openSttSettings = () => {
  settings.open();
  const sec = $("s-sec-stt") as HTMLDetailsElement;
  sec.open = true;
  sec.scrollIntoView({ block: "start" });
};
// « modèle absent » d'un clic haut-parleur → settings sur la section
// TTS (même motif que le hint mic → section STT).
tts.openSettings = () => {
  settings.open();
  const sec = $("s-sec-tts") as HTMLDetailsElement;
  sec.open = true;
  sec.scrollIntoView({ block: "start" });
};
settings.tts = tts;
composer.bindAttachCapable((c) => conns.cap(c ?? "default")?.files === true);
composer.bindQueueSteerCapable((c) => conns.cap(c ?? "default")?.queue_steer === true);
composer.bindQueueClearCapable((c) => conns.cap(c ?? "default")?.queue_clear === true);
conns.bind({
  refresh: () => sidebar.refresh(),
  reopenIfConn: (c) => chat.reopenIfConn(c),
  closeIfConn: (c) => chat.closeIfConn(c),
  dropConvsForConn: (c) => chat.dropConvsForConn(c),
  purgeSaved: (c) => connView.purgeSaved(c),
  clearGroups: (c) => groups.clearConn(c),
  notePendingDel: (ids) => sidebar.notePendingDel(ids),
  clearPendingDel: (id) => sidebar.clearPendingDel(id),
  capsChanged: () => {
    chat.updateHeader();
    comps.syncBtn();
    sidebar.noteCapsChanged();
  },
  connChrome: (c, color) => {
    applyConnChrome(c, color);
    // L'accent de l'instance vient de changer (couleur de conn ou retour
    // au neutre) : --panel-solid en dépend → refresh des terminaux ouverts.
    term.refreshTheme();
  },
  termDead: (k) => term.markConnDead(k),
  liveChanged: () => {
    connView.onLiveChanged();
    // Puce connexion de l'accueil mobile : point coloré = état live.
    syncHomeConn();
  },
});
connView.bind(conns);
connView.refresh = () => sidebar.refresh();
connView.connecting = (hosts) => chat.setConnecting(hosts);
// Exclusion mutuelle des overlays plein #chat — chaque open() ferme
// les autres (sinon : empilement opaque + inert, écran "figé").
connView.onOpening = () => { settings.close(); map.close(); comps.close(); logsPanel.close(); pluginViews.close(); };
settings.onOpening = () => { connView.close(); map.close(); comps.close(); logsPanel.close(); pluginViews.close(); };
// Select « Appairer » des settings : pré-sélection sur la conn liée.
settings.activeConnKey = () => conns.knownConns()[0] ?? null;
// Fenêtre Mises à jour (app + démon lié) — pastille, menu Options,
// Paramètres → Général et À propos y mènent.
const updates = new UpdatesPanel(() => conns.knownConns()[0] ?? null);
settings.openUpdates = (anchor) => updates.open(anchor);
// Rail des Paramètres : groupe « Serveur » = conn liée + ses vues.
settings.server = () => {
  const ck = conns.knownConns()[0];
  if (!ck) return null;
  const st = conns.isGone(ck) ? "down" : conns.state(ck);
  return { label: ck === "web" ? location.host : connLabel(ck), up: st === "ok" };
};
settings.serverLinks = () =>
  comps.capable()
    ? [
        { id: "s-lnk-comp", label: t("comp_h"), icon: "tool", fn: () => comps.open() },
        { id: "s-lnk-orch", label: t("orch_h"), icon: "users", fn: () => comps.openParamsOf(ORCH_COMPONENT) },
      ]
    : [];
const optionsMenu = new OptionsMenu({
  compsCapable: () => comps.capable(),
  connKey: () => conns.knownConns()[0] ?? null,
  openSettings: () => settings.open(),
  openComponents: () => comps.open(),
  openOrchestrator: () => comps.openParamsOf(ORCH_COMPONENT),
  openUpdates: (anchor) => updates.open(anchor),
  leaveSettings: () => settings.requestClose(),
});
comps.bind(conns);
comps.onOpening = () => { settings.close(); connView.close(); map.close(); logsPanel.close(); pluginViews.close(); };
logsPanel.bind(conns);
logsPanel.onOpening = () => { settings.close(); connView.close(); map.close(); comps.close(); pluginViews.close(); };
// Vue plugin (option B) : bouton « vue » du panneau composants →
// overlay iframe sandboxée + bridge postMessage borné. Même exclusion
// mutuelle que les autres overlays #chat.
pluginViews.bind(conns);
pluginViews.onOpening = () => { settings.close(); connView.close(); map.close(); comps.close(); logsPanel.close(); };
pluginViews.openSession = (id, conn) => {
  settings.close();
  connView.close();
  map.close();
  comps.close();
  logsPanel.close();
  void chat.open(id, false, conn);
};
pluginViews.openComponents = () => comps.open();
comps.openView = (name, conn) => void pluginViews.open(name, conn);
newSess.bind(groups);
newSess.refresh = () => void sidebar.refresh();
// Le bouton « New agent » vit dans la sidebar (hors #chat — accessible
// sous les overlays) : fermer les overlays de #chat avant d'ouvrir la
// session créée, sinon elle charge derrière (même motif que
// sidebar.openSession ; close() est un no-op quand l'overlay est fermé).
newSess.openSession = (id, conn) => {
  settings.close();
  connView.close();
  map.close();
  comps.close();
  logsPanel.close();
  pluginViews.close();
  void chat.open(id, false, conn);
};
newSess.knownConns = () => conns.knownConns();
agentCaps.knownConns = () => conns.knownConns();
groups.refresh = () => void sidebar.refresh();
groups.deleteSession = (s) => sidebar.deleteSessionNow(s);
files.bind({ sysline: (txt, ic) => chat.sysline(txt, ic) });
quick.bind({
  openFile: (p) => files.openFilePanel(p),
  openDir: (rel) => files.revealInTree(rel),
});
settings.langChanged = () => {
  // applyStatic a réécrit le placeholder avec input_ph — hints à jour.
  composer.refreshHints();
  void sidebar.refresh();
  fitViewTabs();
  chat.updateHeader();
};
settings.sendModeChanged = () => composer.refreshHints();
settings.mdRenderChanged = (on) => files.setMdRender(on);
settings.themeChanged = () => {
  term.refreshTheme();
  // Changement d'accent global : la couleur propre à la conn liée garde
  // la priorité (ré-appliquée ici), l'accent global s'applique sinon.
  conns.reapplyChrome();
  // Thème dark/light → resynchronise la vue plugin ouverte (bridge
  // `theme` — le plugin ne peut pas lire le DOM parent).
  pluginViews.onThemeChanged();
};
term.bind({
  termCapable: (c) => conns.cap(c)?.terminal === true,
  // "default" n'est résoluble en clé réelle qu'en mono-connexion.
  defaultConn: () => {
    const k = conns.knownConns();
    return k.length === 1 ? k[0] : undefined;
  },
});
map.bind({
  // 1 instance = 1 connexion liée — le diagramme montre son écosystème.
  conn: () => conns.knownConns()[0] ?? "",
  componentsCapable: (c) => conns.cap(c)?.components === true,
  openSession: (id, conn) => void chat.open(id, false, conn),
  onOpening: () => { settings.close(); connView.close(); comps.close(); logsPanel.close(); pluginViews.close(); },
  onToggle: (open) => sidebar.setMapOpen(open),
});

// Conn perdue (transport mort) : les PTY du démon meurent avec → bannière
// dans les terminaux concernés (reopen possible via ↻ au retour).
// Payload {conn, why, gen} ; la forme string reste tolérée (web legacy).
// Gen périmée = mort du client remplacé — ne pas tuer les PTY de la
// conn fraîche (même filtre que ConnectionManager).
void listen<{ conn: string; why?: string; gen?: number } | string>("conn-dead", (e) => {
  const p = e.payload;
  if (typeof p === "string") return term.markConnDead(p);
  if (!conns.isFresh(p.conn, p.gen)) return;
  term.markConnDead(p.conn);
});

// --- Events sessions (push du démon via le bridge Tauri) ---------------------

await listen<{ session: string; event: AcEvent }>("session-event", (msg) => {
  const { session, event } = msg.payload;
  // File AgentBacklog : la session ouverte qui change d'état/pop/
  // disparaît peut avoir muté la file — invalidation debouncée (badge
  // + panneau ; les autres mutations remontent par le poll lent).
  sidebar.noteSessionEvent(event.type);
  // Suppression côté serveur : l'event terminal arrive avant la coupure.
  if (event.type === "session_deleted") {
    chat.dropConv(session);
    term.dropSession(session);
    if (app.current && session === app.current.id) chat.markGone();
    // Brouillon du composer + pièces garées : la session les emporte.
    composer.dropSession(session);
    // Toast seulement pour une suppression externe — la nôtre a déjà
    // retiré la ligne optimistement, re-notifier serait du bruit.
    if (!sidebar.wasPendingDel(session)) notify("info", t("session_deleted_toast"));
    void sidebar.refresh();
    return;
  }
  // Journal vidé côté serveur (/clear, /new, rebind) : caches + vue
  // d'un autre cycle — tout jeter, le nouveau cycle suit derrière.
  if (event.type === "session_cleared") {
    chat.dropConv(session);
    if (app.current && session === app.current.id) void chat.convCleared();
    return;
  }
  // Fin d'un replay session/load « refill » (fork/rebind) : le démon
  // a journalisé l'historique SANS le broadcaster — le DOM
  // n'a vu que les syslines de progression. On recharge la queue du
  // journal (open forcé : cache jeté, dernière page, resubscribe) au
  // lieu d'avoir rendu le transcript entier au fil de l'eau (#300).
  // L'event est journalisé : il sera rendu depuis la page rechargée.
  if (event.type === "history_replayed" && app.current && session === app.current.id) {
    void chat.open(session, true, app.current.conn ?? "");
    return;
  }
  // Étapes de tâche publiées par l'agent : l'event porte la liste
  // ENTIÈRE (remplacement complet, pas de delta) → application directe
  // si c'est la session affichée.
  if (event.type === "steps" && app.current && session === app.current.id) {
    steps.onStepsEvent(event.steps as SessionStep[]);
  }
  if (app.current && session === app.current.id) {
    chat.appendEvent(event, true);
  }
  // Les events système « message en file » / « steer refusé — remis
  // en file » / retenue compact changent la file sans status_changed :
  // le bandeau de file resterait périmé jusqu'au prochain changement
  // d'état — refresh immédiat, restreint aux textes liés à la file
  // (un queue_list par event sinon, gratuit mais inutile).
  if (event.type === "system" && app.current?.id === session
      && /queue|steer|compact|file/i.test((event.text as string) ?? "")) {
    void composer.refreshQueue();
  }
  if (event.type === "status_changed") void sidebar.refresh();
});

// --- Settings / About / menus ⋮ ----------------------------------------------

// About : point central des versions (client/démon/release) + updates —
// openAbout ouvre la modale d'abord puis remplit en async ; le moteur
// reste synchrone (report local, pas de réseau).
$("tb-about").onclick = () => {
  $("about-engine").textContent = engineReport();
  openAbout(() => conns.knownConns()[0] ?? null);
};
// À propos n'installe plus rien : « Mises à jour… » ouvre la fenêtre dédiée.
$("about-updates").onclick = () => {
  ($("dlg-about") as HTMLDialogElement).close();
  updates.open(document.querySelector<HTMLElement>(".tb-update") ?? $("tb-options"));
};
// Menus ⋮ (Android) : l'app-level ouvre Settings/About ; en session le
// ⋮ du header ouvre la feuille Session (S2 — réglages + actions).
$("tb-more").onclick = () =>
  nav.sheet(appMenuItems(() => comps.capable(), () => updates.open()));
$("chat-more").onclick = () => openSessionSheet();
$("about-gh").addEventListener("click", (e) => {
  e.preventDefault();
  shellOpen("https://github.com/Wameuh/AgentControl");
});

// --- Init --------------------------------------------------------------------

hydrateIcons();
groups.init();
newSess.init();
agentCaps.init();
$("btn-agent-caps").onclick = () =>
  agentCaps.open(app.current?.conn ?? conns.knownConns()[0]);
conns.init();
connView.init();
chat.init();
// Copier/sélection clavier sur contenu NON éditable (#370) : WebKitGTK
// (webview Tauri Linux) n'exécute ni le copier natif ni le select-all
// sur une sélection de texte ordinaire — même trou que le menu
// « Copier » du clic droit, comblé ici. Ctrl+C / Cmd+C / Ctrl+Insert →
// copyText (clipboard API + fallback execCommand) ; Ctrl+A dans le fil →
// tout #events. Cible éditable (input/textarea/select/contenteditable)
// → on laisse le comportement natif, rien n'est intercepté.
window.addEventListener("keydown", (e) => {
  if (e.defaultPrevented || e.isComposing || !(e.ctrlKey || e.metaKey) || e.altKey) return;
  const t = e.target as HTMLElement | null;
  if (
    t instanceof HTMLInputElement ||
    t instanceof HTMLTextAreaElement ||
    t instanceof HTMLSelectElement ||
    !!t?.isContentEditable
  )
    return;
  const k = e.key.toLowerCase();
  if (k === "c" || k === "insert") {
    const txt = window.getSelection()?.toString() ?? "";
    if (!txt) return;
    e.preventDefault();
    void copyText(txt);
    return;
  }
  if (k === "a" && chat.selectAll()) e.preventDefault();
});
search.init();
sidebar.init();
composer.init();
files.init();
quick.init();
term.init();
map.init();
settings.init();
initUnsavedDialog();
initViewTabsFit();
updates.init();
optionsMenu.init();
tts.init();
comps.init();
logsPanel.init();
pluginViews.init();
notifications.init();
initFirstPost((c) => conns.cap(c)?.first_post === true);
initModelCosts({ capable: (c) => conns.cap(c)?.model_costs === true });
initModelCatalog({ capable: (c) => conns.cap(c)?.model_catalog === true });
pins.init();
steps.init();
// Barre + feuille Session mobiles (S1/S2) : câblage des conditions —
// mêmes gates capability que l'ancien menu ⋮ du header.
initSessionSheet({
  openSearch: () => search.open(),
  openPath: () => quick.open("path"),
  termSupported: () => term.supported(),
  firstPostCapable: (c) => conns.cap(c)?.first_post === true,
  pinCapable: (c) => conns.cap(c)?.pinned === true,
  stepsCapable: (c) => conns.cap(c)?.steps === true,
  onRenamed: () => { chat.setChatTitle(); void sidebar.refresh(); },
  refreshSidebar: () => void sidebar.refresh(),
});
initSessionBar(openSessionSheet);
// Feuille connexion de la puce d'accueil + coquille mobile (N1/N2) :
// titre de vue, loupe → filtre, re-rendu de la barre session à chaque
// bascule de pile. No-ops hors Android.
initConnSheet({
  openPairSettings: () => settings.openSection("s-sec-conn"),
});
initHomeShell(conns);
// Accueil « par état » + glisser des lignes (N3/N4) — no-ops hors Android.
initInbox(sidebar);
initGestures(sidebar);
initMapGestures();
// Dock de permission (S3) + composer pilule (C1) — no-ops hors Android.
initPermDock((seq) => chat.jumpToSeq(seq));
initComposerMobile(composer);
// Paramètres en liste → sous-écrans (Y4) — no-op hors Android.
initSettingsMobile();
// Suites d'outils compactées (S4) + rangée de touches du terminal (S5).
initToolRuns();
initTermKeys(term);
initModelBar(openFusion, () => chat.updateHeader());
initFusionDialog();
initStatus(() => {
  // Bouton « capacités agents » : visible dès qu'une conn connaît la
  // capability agent_caps (vieux démon → masqué, comme les autres
  // boutons à gate).
  $("btn-agent-caps").hidden = !conns
    .knownConns()
    .some((c) => conns.cap(c)?.agent_caps === true);
  // Bloc host du démon de la conn affichée (session ouverte), sinon de
  // la conn liée — absent sur vieux démon → segment masqué.
  const k = app.current?.conn ?? conns.knownConns()[0] ?? "default";
  // Conn down/en essai : les caps gardent la dernière valeur connue —
  // masquer plutôt qu'afficher des chiffres périmés sous icône grise.
  if ((conns.state(k) ?? "ok") !== "ok") return undefined;
  return conns.cap(k)?.host as HostStats | undefined;
});
initMemDialog();
// Télémétrie perf locale (#297) : collecteur singleton — ligne agrégée
// dans client.log toutes les 5 min + window.__telemetry() pour le debug.
// Démarré tôt pour couvrir tout le boot ; la préférence OFF le désarme.
initTelemetry();
checkBootUpdate((chip) => updates.open(chip));
connView.autoConnect(savedConns, cliConn ?? undefined, autoSuppressed);
