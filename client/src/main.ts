// ---------------------------------------------------------------------------
// Bootstrap : câble les modules entre eux. Le comportement vit dans
//   core/      — helpers DOM, RPC, état transversal, chrome, formatage, conns
//   ui/        — pile d'overlays Android + menus contextuels partagés
//   features/  — connexions, sidebar, vue conversation, composer, fichiers,
//                dialogues, barre de statut, mise à jour
// ---------------------------------------------------------------------------

import "./styles.css";
import "./mesh-bg.ts";
import { invoke, listen, getVersion, isWeb } from "./core/bridge";
import { $, notify } from "./core/dom";
import { app } from "./core/state";
import { initRpc } from "./core/rpc";
import { initModalGuard } from "./core/modal";
import { connKeyOf } from "./core/conn";
import {
  applyTheme,
  applyAccent,
  accentPref,
  accentOf,
  applyConnChrome,
  initGems,
  initPlatform,
  isAndroid,
  initWindowChrome,
  shellOpen,
} from "./core/chrome";
import { nav, type SheetItem } from "./ui/nav";
import { hydrateIcons } from "./ui/icons";
import { initStatus } from "./features/status";
import { checkBootUpdate } from "./features/update";
import { initModelBar } from "./features/modelbar";
import { initFusionDialog, openFusion } from "./features/fusion-dialog";
import { GroupStore } from "./features/groups";
import { NewSessionDialog } from "./features/new-session";
import { ConnectionManager } from "./features/connections";
import { ConnView } from "./features/conn-view";
import { EventRenderer } from "./features/event-renderers";
import { ChatView } from "./features/chat-view";
import { SessionSearch } from "./features/search";
import { Sidebar } from "./features/sidebar";
import { Composer } from "./features/composer";
import { FileExplorer } from "./features/files";
import { MapView } from "./features/map-view";
import { TermPanel } from "./features/terminal";
import { SettingsPanel } from "./features/settings";
import { initFirstPost } from "./features/first-post";
import { initLang, t, applyStatic } from "./i18n";
import type { AcEvent, SavedConn } from "./core/types";

// Thème + langue persistés (localStorage), appliqués au boot
initLang();
applyTheme(localStorage.getItem("ac-theme") === "light" ? "light" : "dark");
// Accent appliqué au boot, avant le premier rendu utile (pas de flash).
applyAccent(accentPref());
applyStatic();
initGems();
initPlatform();
initWindowChrome();
// Log open/close des modals + watchdog anti-gel (dialog invisible, inert
// résiduel) — avant tout showModal pour tracer aussi les dialogs du boot.
initModalGuard();

// Le menu natif du clic droit n'a pas de sens dans l'app — chaque menu
// contextuel custom gère son propre preventDefault/stopPropagation.
window.addEventListener("contextmenu", (e) => e.preventDefault());

// Modèle multi-instances : cette instance se lie à UNE connexion — celle
// demandée par --conn <clé> (spawn_instance d'une autre instance), sinon
// la dernière utilisée (1re entrée de connections.json). Son accent
// éventuel est posé avant le premier rendu utile (pas de flash de la
// couleur par défaut) ; le connect auto lui-même part en fin de boot.
const [cliConn, savedConns] = await Promise.all([
  invoke<string | null>("cli_conn").catch(() => null),
  invoke<SavedConn[]>("saved_connections").catch(() => [] as SavedConn[]),
]);
const bootConn =
  (cliConn ? savedConns.find((c) => connKeyOf(c, isAndroid()) === cliConn) : undefined) ??
  savedConns[0];
// Ne teinter que si un bind va réellement suivre — avec autoconnect off et
// sans --conn, aucune conn ne sera liée : pas de couleur mensongère.
const willConnect =
  cliConn != null || isWeb || localStorage.getItem("ac-autoconnect") !== "0";
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
const map = new MapView();
const term = new TermPanel();
const connView = new ConnView();
const newSess = new NewSessionDialog();
const settings = new SettingsPanel();
const search = new SessionSearch();

// call() a besoin de la conn courante + des états live — injectés pour éviter
// l'import inverse core → features.
initRpc({
  currentConn: () => app.current?.conn,
  connState: (k) => conns.state(k),
});

chat.bind(renderer, {
  setActiveRow: (id) => sidebar.setActiveRow(id),
  refresh: () => sidebar.refresh(),
  refreshQueue: () => composer.refreshQueue(),
  syncAltBtn: () => composer.syncAltBtn(),
  onSessionOpened: () => {
    files.onSessionOpened();
    term.onSessionSwitched();
  },
  onSessionChanged: () => {
    search.onSessionChanged();
    composer.sessionChanged();
  },
  connCount: () => conns.knownConns().length,
  connsDownOrNone: () => !conns.anyUp(),
  connHome: () => connView.maybeAutoShow(),
  firstPostCapable: (c) => conns.cap(c)?.first_post === true,
  termCapable: (c) => conns.cap(c)?.terminal === true,
});
search.bind({ jumpToSeq: (seq) => chat.jumpToSeq(seq) });
renderer.bind({
  metaChanged: () => chat.updateHeader(),
  ctxChanged: () => chat.updateCtx(),
  statusChanged: () => void sidebar.refresh(),
  openSession: (id) => void chat.open(id),
});
sidebar.bind(conns, groups, newSess, chat, settings, connView);
composer.bind(chat);
composer.refresh = () => sidebar.refresh();
composer.bindAttachCapable((c) => conns.cap(c ?? "default")?.files === true);
conns.bind({
  refresh: () => sidebar.refresh(),
  reopenIfConn: (c) => chat.reopenIfConn(c),
  closeIfConn: (c) => chat.closeIfConn(c),
  dropConvsForConn: (c) => chat.dropConvsForConn(c),
  purgeSaved: (c) => connView.purgeSaved(c),
  clearGroups: (c) => groups.clearConn(c),
  notePendingDel: (ids) => sidebar.notePendingDel(ids),
  clearPendingDel: (id) => sidebar.clearPendingDel(id),
  capsChanged: () => chat.updateHeader(),
  connChrome: (c, color) => applyConnChrome(c, color),
  termDead: (k) => term.markConnDead(k),
  liveChanged: () => connView.onLiveChanged(),
});
connView.bind(conns);
connView.refresh = () => sidebar.refresh();
connView.connecting = (hosts) => chat.setConnecting(hosts);
// Exclusion mutuelle des 3 overlays plein #chat — chaque open() ferme
// les deux autres (sinon : empilement opaque + inert, écran "figé").
connView.onOpening = () => { settings.close(); map.close(); };
settings.onOpening = () => { connView.close(); map.close(); };
newSess.bind(groups);
newSess.refresh = () => void sidebar.refresh();
// Le bouton « New agent » vit dans la sidebar (hors #chat — accessible
// sous les overlays) : fermer settings/conn-view avant d'ouvrir la
// session créée, sinon elle charge derrière l'overlay (même motif que
// sidebar.openSession ; close() est un no-op quand l'overlay est fermé).
newSess.openSession = (id, conn) => {
  settings.close();
  connView.close();
  void chat.open(id, false, conn);
};
newSess.knownConns = () => conns.knownConns();
groups.refresh = () => void sidebar.refresh();
files.bind({ sysline: (txt, ic) => chat.sysline(txt, ic) });
settings.langChanged = () => {
  // applyStatic a réécrit le placeholder avec input_ph — hints à jour.
  composer.refreshHints();
  void sidebar.refresh();
  chat.updateHeader();
};
settings.sendModeChanged = () => composer.refreshHints();
settings.themeChanged = () => {
  term.refreshTheme();
  // Changement d'accent global : la couleur propre à la conn liée garde
  // la priorité (ré-appliquée ici), l'accent global s'applique sinon.
  conns.reapplyChrome();
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
  onOpening: () => { settings.close(); connView.close(); },
});

// Conn perdue (EOF transport) : les PTY du démon meurent avec → bannière
// dans les terminaux concernés (reopen possible via ↻ au retour).
void listen<string>("conn-dead", (e) => term.markConnDead(e.payload));

// --- Events sessions (push du démon via le bridge Tauri) ---------------------

await listen<{ session: string; event: AcEvent }>("session-event", (msg) => {
  const { session, event } = msg.payload;
  // Suppression côté serveur : l'event terminal arrive avant la coupure.
  if (event.type === "session_deleted") {
    chat.dropConv(session);
    term.dropSession(session);
    if (app.current && session === app.current.id) chat.markGone();
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
  if (app.current && session === app.current.id) {
    chat.appendEvent(event, true);
  }
  if (event.type === "status_changed") void sidebar.refresh();
});

// --- Settings / About / menus ⋮ ----------------------------------------------

// About : version réelle + lien GitHub via le plugin shell
$("tb-about").onclick = async () => {
  try {
    $("about-version").textContent = "v" + (await getVersion());
  } catch {
    /* dev */
  }
  ($("dlg-about") as HTMLDialogElement).showModal();
};
// Menus ⋮ (Android) : l'app-level ouvre Settings/About, celui du chat les
// actions de session (cachées du header sur mobile).
$("tb-more").onclick = () =>
  nav.sheet([
    { icon: "map", label: t("map_title"), fn: () => $("tb-map").click() },
    { icon: "settings", label: t("settings_title"), fn: () => $("tb-settings").click() },
    { icon: "info", label: t("about_title"), fn: () => $("tb-about").click() },
  ]);
$("chat-more").onclick = () => {
  const items: SheetItem[] = [];
  if (app.current) items.push({ icon: "search", label: t("search_title"), fn: () => search.open() });
  if (app.current && conns.cap(app.current.conn ?? "default")?.first_post === true)
    items.push({ icon: "top", label: t("first_post"), fn: () => $("btn-first").click() });
  if (app.current && term.supported())
    items.push({ icon: "terminal", label: t("term_title"), fn: () => $("btn-term").click() });
  if (app.current) items.push({ icon: "folder", label: t("files_title"), fn: () => $("btn-files").click() });
  if (app.current && !app.alive && app.current.state !== "starting")
    items.push({ icon: "play", label: t("resume"), fn: () => $("btn-resume").click() });
  if (app.current && app.alive)
    items.push({ icon: "sleep", label: t("ctx_sleep"), fn: () => $("btn-sleep").click() });
  items.push({ icon: "refresh", label: t("refresh_title"), fn: () => $("btn-refresh").click() });
  nav.sheet(items);
};
$("about-gh").addEventListener("click", (e) => {
  e.preventDefault();
  shellOpen("https://github.com/Wameuh/AgentControl");
});

// --- Init --------------------------------------------------------------------

hydrateIcons();
groups.init();
newSess.init();
conns.init();
connView.init();
chat.init();
search.init();
sidebar.init();
composer.init();
files.init();
term.init();
map.init();
settings.init();
initFirstPost((c) => conns.cap(c)?.first_post === true);
initModelBar(openFusion);
initFusionDialog();
initStatus();
checkBootUpdate();
connView.autoConnect(savedConns, cliConn ?? undefined);
