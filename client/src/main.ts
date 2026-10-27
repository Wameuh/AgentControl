// ---------------------------------------------------------------------------
// Bootstrap : câble les modules entre eux. Le comportement vit dans
//   core/      — helpers DOM, RPC, état transversal, chrome, formatage, conns
//   ui/        — pile d'overlays Android + menus contextuels partagés
//   features/  — connexions, sidebar, vue conversation, composer, fichiers,
//                dialogues, barre de statut, mise à jour
// ---------------------------------------------------------------------------

import "./styles.css";
import "./mesh-bg.ts";
import { listen } from "@tauri-apps/api/event";
import { getVersion } from "@tauri-apps/api/app";
import { $ } from "./core/dom";
import { app } from "./core/state";
import { initRpc } from "./core/rpc";
import {
  applyTheme,
  initGems,
  initPlatform,
  initWindowChrome,
  shellOpen,
} from "./core/chrome";
import { nav } from "./ui/nav";
import { initStatus } from "./features/status";
import { checkBootUpdate } from "./features/update";
import { initModelBar } from "./features/modelbar";
import { initFusionDialog, openFusion } from "./features/fusion-dialog";
import { GroupStore } from "./features/groups";
import { NewSessionDialog } from "./features/new-session";
import { ConnectionManager } from "./features/connections";
import { ConnectDialog } from "./features/connect-dialog";
import { EventRenderer } from "./features/event-renderers";
import { ChatView } from "./features/chat-view";
import { Sidebar } from "./features/sidebar";
import { Composer } from "./features/composer";
import { FileExplorer } from "./features/files";
import { SettingsPanel } from "./features/settings";
import { initLang, t, applyStatic } from "./i18n";
import type { AcEvent } from "./core/types";

// Thème + langue persistés (localStorage), appliqués au boot
initLang();
applyTheme(localStorage.getItem("ac-theme") === "light" ? "light" : "dark");
applyStatic();
initGems();
initPlatform();
initWindowChrome();

// Le menu natif du clic droit n'a pas de sens dans l'app — chaque menu
// contextuel custom gère son propre preventDefault/stopPropagation.
window.addEventListener("contextmenu", (e) => e.preventDefault());

// --- Features stateful -------------------------------------------------------

const conns = new ConnectionManager();
const groups = new GroupStore();
const renderer = new EventRenderer();
const chat = new ChatView();
const sidebar = new Sidebar();
const composer = new Composer();
const files = new FileExplorer();
const connectDlg = new ConnectDialog();
const newSess = new NewSessionDialog();
const settings = new SettingsPanel();

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
  onSessionOpened: () => files.onSessionOpened(),
});
renderer.bind({
  metaChanged: () => chat.updateHeader(),
  ctxChanged: () => chat.updateCtx(),
  statusChanged: () => void sidebar.refresh(),
  openSession: (id) => void chat.open(id),
});
sidebar.bind(conns, groups, newSess, chat);
composer.bind(chat);
composer.refresh = () => sidebar.refresh();
conns.bind({
  refresh: () => sidebar.refresh(),
  reopenIfConn: (c) => chat.reopenIfConn(c),
  closeIfConn: (c) => chat.closeIfConn(c),
  dropConvsForConn: (c) => chat.dropConvsForConn(c),
  purgeSaved: (c) => connectDlg.purgeSaved(c),
  clearGroups: (c) => groups.clearConn(c),
});
connectDlg.bind(conns);
connectDlg.refresh = () => sidebar.refresh();
newSess.bind(groups);
newSess.refresh = () => void sidebar.refresh();
newSess.knownConns = () => conns.knownConns();
groups.refresh = () => void sidebar.refresh();
files.bind({ sysline: (txt) => chat.sysline(txt) });
settings.langChanged = () => {
  // applyStatic a réécrit le placeholder avec input_ph — hints à jour.
  composer.refreshHints();
  void sidebar.refresh();
  chat.updateHeader();
};
settings.sendModeChanged = () => composer.refreshHints();

// --- Events sessions (push du démon via le bridge Tauri) ---------------------

await listen<{ session: string; event: AcEvent }>("session-event", (msg) => {
  const { session, event } = msg.payload;
  // Suppression côté serveur : l'event terminal arrive avant la coupure.
  if (event.type === "session_deleted") {
    chat.dropConv(session);
    if (app.current && session === app.current.id) chat.markGone();
    void sidebar.refresh();
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
    { label: `⚙  ${t("settings_title")}`, fn: () => $("tb-settings").click() },
    { label: `ⓘ  ${t("about_title")}`, fn: () => $("tb-about").click() },
  ]);
$("chat-more").onclick = () => {
  const items: { label: string; danger?: boolean; fn: () => void }[] = [];
  if (app.current) items.push({ label: `📁  ${t("files_title")}`, fn: () => $("btn-files").click() });
  if (app.current && !app.alive)
    items.push({ label: `↻  ${t("resume")}`, fn: () => $("btn-resume").click() });
  items.push({ label: `⟳  ${t("refresh_title")}`, fn: () => $("btn-refresh").click() });
  nav.sheet(items);
};
$("about-gh").addEventListener("click", (e) => {
  e.preventDefault();
  shellOpen("https://github.com/Wameuh/AgentControl");
});

// --- Init --------------------------------------------------------------------

groups.init();
newSess.init();
conns.init();
connectDlg.init();
chat.init();
sidebar.init();
composer.init();
files.init();
settings.init();
initModelBar(openFusion);
initFusionDialog();
initStatus();
checkBootUpdate();
connectDlg.autoConnect();
