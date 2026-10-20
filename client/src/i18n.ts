// i18n minimal : dictionnaire en/fr (défaut en), t() avec interpolation {var},
// data-i18n / data-i18n-ph / data-i18n-title pour les chaînes statiques du HTML.

const STR = {
  en: {
    connect: "Connect", connected_n: "connected ({n} sessions)",
    disconnected_reconnect: "disconnected — reconnect", disconnected: "disconnected",
    new_session: "+ New session", send: "Send", cancel: "Cancel", stop: "Stop",
    resume: "Resume", refresh_title: "Reload session",
    input_ph: "Message… (Enter = send, Shift+Enter = newline, / = commands)",
    model_free_ph: "model…", model_free_title: "Model name (Enter to apply)",
    effort_title: "Effort level", delete: "Delete", delete_confirm: "del?",
    title_dblclick: "Double-click to rename",
    // dialogs
    new_session_h: "New session", agent: "Agent", workdir: "Working directory",
    title_label: "Title", title_ph: "my session", mode_label: "Mode",
    mode_ph: "normal (optional)", model_label: "Model", model_ph: "default",
    attach_label: "Resume native ID", attach_ph: "(optional) existing agent session ID",
    prompt_label: "Initial message", create: "Create",
    conn_h: "Connect to daemon", recent: "Recent connections", choose: "— choose —",
    type_label: "Type", local_sock: "Local socket (daemon on this machine)",
    ssh_label: "SSH (remote server)", tcp_label: "Direct TCP", host_label: "Host",
    host_ph: "user@server.local or 127.0.0.1:7331", port_label: "Remote daemon port",
    ssh_args_label: "SSH args", ssh_args_ph: "-i ~/.ssh/key -p 22", logs: "Logs",
    fusion_h: "Fusion — two models in parallel", main_model: "Main model",
    main_effort: "Main effort", sidekick: "Sidekick", sidekick_effort: "Sidekick effort",
    apply: "Apply", resume_h: "Agent sessions to resume", close: "Close",
    settings_h: "Settings", theme_label: "Theme", theme_dark: "Dark", theme_light: "Light",
    lang_label: "Language", about_h: "About", license_label: "License",
    // generated
    delete_armed: "del ?", loading: "loading…", load_full: "load full output…",
    thought: "thought", resolved: "✔ resolved — ", mode_arrow: "mode → ",
    cmds_avail: "{n} commands available", state_arrow: "state → ",
    perm_title: "permission request", answer_ph: "answer (JSON or text)",
    answer_btn: "Answer", ignore_btn: "Dismiss",
    no_native: "No native sessions (or session/list unsupported by this agent).",
    resume_current: "Resume current session (respawn + session/load)",
    builtin_model: "Change session model", builtin_resume: "Resume session (respawn + session/load)",
    builtin_cancel: "Cancel current run",
    fusion_none: "No Fusion combinations announced by the agent.",
    fusion_bad: "Fusion combination not announced by the agent: ",
    fusion_entry: "✨ Fusion (2 models)…", cmd_placeholder: "⌘ commands…",
    effort_default: "default", subscribe_err: "subscribe: ",
    settings_title: "Settings", about_title: "About AgentControl",
    autoconnect_label: "Auto-connect on launch",
    installing_daemon: "installing daemon on server…",
    updating_daemon: "updating daemon {dv} → {lv}…",
  },
  fr: {
    connect: "Connecter", connected_n: "connecté ({n} sessions)",
    disconnected_reconnect: "déconnecté — reconnecter", disconnected: "déconnecté",
    new_session: "+ Nouvelle session", send: "Envoyer", cancel: "Annuler", stop: "Stop",
    resume: "Reprendre", refresh_title: "Recharger la session",
    input_ph: "Message… (Entrée = envoyer, Maj+Entrée = saut de ligne, / = commandes)",
    model_free_ph: "modèle…", model_free_title: "Nom du modèle (Entrée pour appliquer)",
    effort_title: "Niveau d'effort", delete: "Supprimer", delete_confirm: "suppr ?",
    title_dblclick: "Double-clic pour renommer",
    new_session_h: "Nouvelle session", agent: "Agent", workdir: "Dossier de travail",
    title_label: "Titre", title_ph: "ma session", mode_label: "Mode",
    mode_ph: "normal (optionnel)", model_label: "Modèle", model_ph: "défaut",
    attach_label: "Reprendre ID natif", attach_ph: "(optionnel) ID session agent existante",
    prompt_label: "Message initial", create: "Créer",
    conn_h: "Connexion au démon", recent: "Connexions récentes", choose: "— choisir —",
    type_label: "Type", local_sock: "Socket local (démon sur cette machine)",
    ssh_label: "SSH (serveur distant)", tcp_label: "TCP direct", host_label: "Hôte",
    host_ph: "user@serveur.local ou 127.0.0.1:7331", port_label: "Port démon distant",
    ssh_args_label: "Args SSH", ssh_args_ph: "-i ~/.ssh/key -p 22", logs: "Logs",
    fusion_h: "Fusion — deux modèles en parallèle", main_model: "Modèle principal",
    main_effort: "Effort principal", sidekick: "Sidekick", sidekick_effort: "Effort sidekick",
    apply: "Appliquer", resume_h: "Sessions de l'agent à reprendre", close: "Fermer",
    settings_h: "Paramètres", theme_label: "Thème", theme_dark: "Sombre", theme_light: "Clair",
    lang_label: "Langue", about_h: "À propos", license_label: "Licence",
    delete_armed: "suppr ?", loading: "chargement…", load_full: "charger la sortie complète…",
    thought: "pensée", resolved: "✔ résolu — ", mode_arrow: "mode → ",
    cmds_avail: "{n} commandes disponibles", state_arrow: "état → ",
    perm_title: "demande de permission", answer_ph: "réponse (JSON ou texte)",
    answer_btn: "Répondre", ignore_btn: "Ignorer",
    no_native: "Aucune session native (ou session/list non supporté par l'agent).",
    resume_current: "Reprendre la session courante (respawn + session/load)",
    builtin_model: "Changer le modèle de la session", builtin_resume: "Reprendre la session (respawn + session/load)",
    builtin_cancel: "Annuler la requête en cours",
    fusion_none: "Aucune combinaison Fusion annoncée par l'agent.",
    fusion_bad: "Combinaison Fusion non annoncée par l'agent : ",
    fusion_entry: "✨ Fusion (2 modèles)…", cmd_placeholder: "⌘ commandes…",
    effort_default: "défaut", subscribe_err: "subscribe : ",
    settings_title: "Paramètres", about_title: "À propos d'AgentControl",
    autoconnect_label: "Connexion auto au lancement",
    installing_daemon: "installation du démon sur le serveur…",
    updating_daemon: "mise à jour du démon {dv} → {lv}…",
  },
} as const;

export type Lang = keyof typeof STR;
export type Key = keyof (typeof STR)["en"];

let lang: Lang = "en";

export function initLang() {
  lang = localStorage.getItem("ac-lang") === "fr" ? "fr" : "en";
}

export function getLang() {
  return lang;
}

export function setLang(l: Lang) {
  lang = l;
  localStorage.setItem("ac-lang", l);
  applyStatic();
}

export function t(k: Key, vars?: Record<string, string | number>): string {
  let s: string = STR[lang][k] ?? STR.en[k] ?? k;
  if (vars) for (const [k2, v] of Object.entries(vars)) s = s.replace(`{${k2}}`, String(v));
  return s;
}

/** Traduit les éléments marqués data-i18n / data-i18n-ph / data-i18n-title. */
export function applyStatic() {
  for (const el of document.querySelectorAll<HTMLElement>("[data-i18n]")) {
    el.textContent = t(el.dataset.i18n as Key);
  }
  for (const el of document.querySelectorAll<HTMLInputElement>("[data-i18n-ph]")) {
    el.placeholder = t(el.dataset.i18nPh as Key);
  }
  for (const el of document.querySelectorAll<HTMLElement>("[data-i18n-title]")) {
    el.title = t(el.dataset.i18nTitle as Key);
  }
}
