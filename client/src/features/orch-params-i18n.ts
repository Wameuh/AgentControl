// ---------------------------------------------------------------------------
// Libellés et descriptions courtes des paramètres de l'orchestrateur
// (composant `backlog-orchestrator`), par langue. Le schéma du démon
// reste la source de vérité (clés, types, défauts) et son texte anglais
// détaillé sert d'infobulle « En savoir plus » ; une clé absente d'ici
// retombe sur le libellé du démon. `adv` = réglage avancé, masqué par
// défaut dans la vue (chemins, prompts, modèles par rôle, JSON).
// ---------------------------------------------------------------------------

import type { Lang } from "../i18n";

type Txt = Record<Lang, [label: string, desc: string]>;

export interface OrchParamText {
  t: Txt;
  adv?: true;
  /** Libellés des valeurs d'une enum fermée, par langue. */
  choices?: Record<string, Record<Lang, string>>;
}

const P: Record<string, OrchParamText> = {
  // ---- Équipe ----
  max_workers: { t: {
    en: ["Max workers", "Worker sessions running at the same time. 0 stops new spawns."],
    fr: ["Workers maximum", "Sessions worker en même temps. 0 empêche tout nouveau lancement."],
    de: ["Maximale Worker", "Gleichzeitige Worker-Sitzungen. 0 verhindert neue Starts."],
    es: ["Workers máximos", "Sesiones worker simultáneas. 0 impide nuevos lanzamientos."],
  } },
  max_working: { t: {
    en: ["Active workers at once", "A worker waiting on CI or a review frees its active slot. 0 = same limit as the team."],
    fr: ["Workers actifs en même temps", "Un worker qui attend la CI ou une revue libère sa place active. 0 = même limite que l'équipe."],
    de: ["Gleichzeitig aktive Worker", "Ein Worker, der auf CI oder Review wartet, gibt seinen aktiven Platz frei. 0 = wie das Team."],
    es: ["Workers activos a la vez", "Un worker que espera la CI o una revisión libera su plaza activa. 0 = mismo límite que el equipo."],
  } },
  worker_pool: { t: {
    en: ["Worker team", "One row per agent profile, with its own count, model and effort. Empty = Max workers with the worker profile."],
    fr: ["Équipe de workers", "Une ligne par profil d'agent, avec son nombre, son modèle et son effort. Vide = Workers maximum avec le profil worker."],
    de: ["Worker-Team", "Eine Zeile pro Agentenprofil mit eigener Anzahl, Modell und Aufwand. Leer = Maximale Worker mit dem Worker-Profil."],
    es: ["Equipo de workers", "Una fila por perfil de agente, con su número, modelo y esfuerzo. Vacío = Workers máximos con el perfil worker."],
  } },
  worker_agent: { t: {
    en: ["Worker profile", "Agent profile for workers. Empty = the global agent profile."],
    fr: ["Profil des workers", "Profil d'agent des workers. Vide = profil d'agent global."],
    de: ["Worker-Profil", "Agentenprofil der Worker. Leer = globales Agentenprofil."],
    es: ["Perfil de los workers", "Perfil de agente de los workers. Vacío = perfil de agente global."],
  } },
  title_prefix: { adv: true, t: {
    en: ["Session prefix", "Title prefix of worker sessions. Running workers keep the old name and are no longer tracked."],
    fr: ["Préfixe des sessions", "Préfixe du titre des sessions worker. Les workers en cours gardent l'ancien nom et ne sont plus suivis."],
    de: ["Sitzungspräfix", "Titelpräfix der Worker-Sitzungen. Laufende Worker behalten den alten Namen und werden nicht mehr verfolgt."],
    es: ["Prefijo de sesiones", "Prefijo del título de las sesiones worker. Los workers en curso conservan el nombre antiguo y dejan de seguirse."],
  } },
  prompt: { adv: true, t: {
    en: ["Worker prompt", "Custom prompt for workers. Empty = built-in prompt."],
    fr: ["Prompt des workers", "Prompt personnalisé des workers. Vide = prompt intégré."],
    de: ["Worker-Prompt", "Eigener Prompt für Worker. Leer = eingebauter Prompt."],
    es: ["Prompt de los workers", "Prompt personalizado de los workers. Vacío = prompt integrado."],
  } },
  // ---- Planification ----
  plan_review: { t: {
    en: ["Review plans before implementation", "Each item is planned, reviewed by a separate agent, then implemented. Off = a worker implements directly."],
    fr: ["Revue du plan avant implémentation", "Chaque élément est planifié, relu par un autre agent, puis implémenté. Désactivé = un worker implémente directement."],
    de: ["Plan vor Umsetzung prüfen", "Jedes Element wird geplant, von einem anderen Agenten geprüft und dann umgesetzt. Aus = ein Worker setzt direkt um."],
    es: ["Revisar el plan antes de implementar", "Cada elemento se planifica, lo revisa otro agente y luego se implementa. Desactivado = un worker implementa directamente."],
  } },
  planner_agent: { t: {
    en: ["Planner profile", "Agent profile that plans, then implements. Empty = the global agent profile."],
    fr: ["Profil du planificateur", "Profil d'agent qui planifie puis implémente. Vide = profil d'agent global."],
    de: ["Planer-Profil", "Agentenprofil, das plant und dann umsetzt. Leer = globales Agentenprofil."],
    es: ["Perfil del planificador", "Perfil de agente que planifica y luego implementa. Vacío = perfil de agente global."],
  } },
  reviewer_agent: { t: {
    en: ["Plan reviewer profile", "Agent profile that reviews plans. Empty = the global agent profile."],
    fr: ["Profil du relecteur de plans", "Profil d'agent qui relit les plans. Vide = profil d'agent global."],
    de: ["Profil des Planprüfers", "Agentenprofil, das Pläne prüft. Leer = globales Agentenprofil."],
    es: ["Perfil del revisor de planes", "Perfil de agente que revisa los planes. Vacío = perfil de agente global."],
  } },
  max_review_rounds: { t: {
    en: ["Max review rounds", "Beyond this, the item waits for a human review."],
    fr: ["Tours de revue maximum", "Au-delà, l'élément attend une revue humaine."],
    de: ["Maximale Prüfrunden", "Danach wartet das Element auf eine menschliche Prüfung."],
    es: ["Rondas de revisión máximas", "Después, el elemento espera una revisión humana."],
  } },
  plan_rereview: { t: {
    en: ["Review again after changes", "Off = a single review round; a rejected plan waits for a human."],
    fr: ["Relire à nouveau après correction", "Désactivé = un seul tour de revue ; un plan refusé attend un humain."],
    de: ["Nach Änderungen erneut prüfen", "Aus = nur eine Prüfrunde; ein abgelehnter Plan wartet auf einen Menschen."],
    es: ["Revisar de nuevo tras cambios", "Desactivado = una sola ronda; un plan rechazado espera a una persona."],
  } },
  reviewer_reuse: { t: {
    en: ["Keep the same plan reviewer", "Off = a new reviewer session for each round."],
    fr: ["Garder le même relecteur", "Désactivé = une nouvelle session de relecture à chaque tour."],
    de: ["Gleichen Prüfer behalten", "Aus = für jede Runde eine neue Prüfsitzung."],
    es: ["Mantener el mismo revisor", "Desactivado = una nueva sesión de revisión en cada ronda."],
  } },
  planner_mode: { adv: true, t: {
    en: ["Planner mode", "Mode requested for planner sessions. Empty = profile default."],
    fr: ["Mode du planificateur", "Mode demandé pour les sessions de planification. Vide = défaut du profil."],
    de: ["Planer-Modus", "Modus für Planungssitzungen. Leer = Profilstandard."],
    es: ["Modo del planificador", "Modo solicitado para las sesiones de planificación. Vacío = predeterminado del perfil."],
  } },
  reviewer_mode: { adv: true, t: {
    en: ["Plan reviewer mode", "Mode requested for plan reviewer sessions. Empty = profile default."],
    fr: ["Mode du relecteur de plans", "Mode demandé pour les sessions de relecture. Vide = défaut du profil."],
    de: ["Modus des Planprüfers", "Modus für Prüfsitzungen. Leer = Profilstandard."],
    es: ["Modo del revisor de planes", "Modo solicitado para las sesiones de revisión. Vacío = predeterminado del perfil."],
  } },
  implement_mode: { adv: true, t: {
    en: ["Implementation mode", "Mode used once a plan is approved. Empty = profile default."],
    fr: ["Mode d'implémentation", "Mode utilisé une fois le plan approuvé. Vide = défaut du profil."],
    de: ["Umsetzungsmodus", "Modus nach Freigabe des Plans. Leer = Profilstandard."],
    es: ["Modo de implementación", "Modo usado cuando se aprueba el plan. Vacío = predeterminado del perfil."],
  } },
  planner_model: { adv: true, t: {
    en: ["Planner model", "Model for planners and implementers. Empty = global model."],
    fr: ["Modèle du planificateur", "Modèle des planificateurs et implémenteurs. Vide = modèle global."],
    de: ["Planer-Modell", "Modell für Planer und Umsetzer. Leer = globales Modell."],
    es: ["Modelo del planificador", "Modelo de planificadores e implementadores. Vacío = modelo global."],
  } },
  planner_effort: { adv: true, t: {
    en: ["Planner effort", "Thinking level for planners. Empty = global level."],
    fr: ["Effort du planificateur", "Niveau de réflexion des planificateurs. Vide = niveau global."],
    de: ["Planer-Aufwand", "Denkstufe der Planer. Leer = globale Stufe."],
    es: ["Esfuerzo del planificador", "Nivel de razonamiento de los planificadores. Vacío = nivel global."],
  } },
  reviewer_model: { adv: true, t: {
    en: ["Plan reviewer model", "Model for plan reviewers. Empty = global model."],
    fr: ["Modèle du relecteur de plans", "Modèle des relecteurs de plans. Vide = modèle global."],
    de: ["Modell des Planprüfers", "Modell der Planprüfer. Leer = globales Modell."],
    es: ["Modelo del revisor de planes", "Modelo de los revisores de planes. Vacío = modelo global."],
  } },
  reviewer_effort: { adv: true, t: {
    en: ["Plan reviewer effort", "Thinking level for plan reviewers. Empty = global level."],
    fr: ["Effort du relecteur de plans", "Niveau de réflexion des relecteurs. Vide = niveau global."],
    de: ["Aufwand des Planprüfers", "Denkstufe der Planprüfer. Leer = globale Stufe."],
    es: ["Esfuerzo del revisor de planes", "Nivel de razonamiento de los revisores. Vacío = nivel global."],
  } },
  // ---- Revues PR ----
  pr_review: { t: {
    en: ["Daemon PR reviews", "A review session runs on the exact commit and posts its verdict on the PR."],
    fr: ["Revue des PR par le démon", "Une session de revue tourne sur le commit exact et poste son verdict sur la PR."],
    de: ["PR-Reviews durch den Daemon", "Eine Review-Sitzung läuft auf dem genauen Commit und postet ihr Urteil in den PR."],
    es: ["Revisión de PR por el daemon", "Una sesión de revisión se ejecuta sobre el commit exacto y publica su veredicto en la PR."],
  } },
  review_trigger_mode: { t: {
    en: ["When to review", "What starts a review."],
    fr: ["Déclenchement", "Ce qui lance une revue."],
    de: ["Auslöser", "Was ein Review startet."],
    es: ["Activación", "Qué inicia una revisión."],
  }, choices: {
    comment: { en: "Comment", fr: "Commentaire", de: "Kommentar", es: "Comentario" },
    auto_open: { en: "When opened", fr: "À l'ouverture", de: "Beim Öffnen", es: "Al abrirse" },
    auto_push: { en: "Every push", fr: "À chaque push", de: "Bei jedem Push", es: "En cada push" },
    manual: { en: "Manual", fr: "Manuel", de: "Manuell", es: "Manual" },
    off: { en: "Off", fr: "Désactivé", de: "Aus", es: "Desactivado" },
  } },
  pr_review_trigger: { t: {
    en: ["Trigger comment", "Posted alone by a repository member to start a review."],
    fr: ["Commentaire déclencheur", "Posté seul par un membre du dépôt pour lancer une revue."],
    de: ["Auslöser-Kommentar", "Von einem Repo-Mitglied allein gepostet, um ein Review zu starten."],
    es: ["Comentario de activación", "Publicado solo por un miembro del repositorio para iniciar una revisión."],
  } },
  pr_rereview: { t: {
    en: ["Review again after a verdict", "Off = one review per PR."],
    fr: ["Relire à nouveau après un verdict", "Désactivé = une seule revue par PR."],
    de: ["Nach einem Urteil erneut prüfen", "Aus = ein Review pro PR."],
    es: ["Revisar de nuevo tras un veredicto", "Desactivado = una revisión por PR."],
  } },
  auto_review_untrusted: { t: {
    en: ["Auto-review external PRs", "In automatic modes, also review PRs from forks by outside authors."],
    fr: ["Revue auto des PR externes", "En modes automatiques, relire aussi les PR de forks d'auteurs extérieurs."],
    de: ["Externe PRs automatisch prüfen", "In automatischen Modi auch PRs aus Forks externer Autoren prüfen."],
    es: ["Revisión automática de PR externas", "En modos automáticos, revisar también PR de forks de autores externos."],
  } },
  pr_reviewer_agent: { t: {
    en: ["PR reviewer profile", "Agent profile for PR reviews. Empty = the global agent profile."],
    fr: ["Profil du relecteur de PR", "Profil d'agent des revues de PR. Vide = profil d'agent global."],
    de: ["PR-Prüfer-Profil", "Agentenprofil für PR-Reviews. Leer = globales Agentenprofil."],
    es: ["Perfil del revisor de PR", "Perfil de agente de las revisiones de PR. Vacío = perfil de agente global."],
  } },
  pr_review_max_parallel: { t: {
    en: ["Reviews in parallel", "Separate from workers, so reviews are never starved."],
    fr: ["Revues en parallèle", "Indépendant des workers : les revues ne sont jamais bloquées."],
    de: ["Parallele Reviews", "Unabhängig von den Workern, damit Reviews nie blockiert werden."],
    es: ["Revisiones en paralelo", "Independiente de los workers: las revisiones nunca se bloquean."],
  } },
  pr_review_timeout: { t: {
    en: ["Review time limit (s)", "Beyond this, the review is stopped and reported as failed."],
    fr: ["Durée maximale d'une revue (s)", "Au-delà, la revue est arrêtée et signalée en échec."],
    de: ["Zeitlimit eines Reviews (s)", "Danach wird das Review gestoppt und als fehlgeschlagen gemeldet."],
    es: ["Tiempo máximo de revisión (s)", "Después, la revisión se detiene y se marca como fallida."],
  } },
  pr_reviewer_mode: { adv: true, t: {
    en: ["PR reviewer mode", "Mode requested for PR review sessions. Empty = profile default."],
    fr: ["Mode du relecteur de PR", "Mode demandé pour les sessions de revue de PR. Vide = défaut du profil."],
    de: ["PR-Prüfer-Modus", "Modus für PR-Review-Sitzungen. Leer = Profilstandard."],
    es: ["Modo del revisor de PR", "Modo solicitado para las sesiones de revisión de PR. Vacío = predeterminado del perfil."],
  } },
  pr_reviewer_model: { adv: true, t: {
    en: ["PR reviewer model", "Model for PR reviews. Empty = global model."],
    fr: ["Modèle du relecteur de PR", "Modèle des revues de PR. Vide = modèle global."],
    de: ["PR-Prüfer-Modell", "Modell für PR-Reviews. Leer = globales Modell."],
    es: ["Modelo del revisor de PR", "Modelo de las revisiones de PR. Vacío = modelo global."],
  } },
  pr_reviewer_effort: { adv: true, t: {
    en: ["PR reviewer effort", "Thinking level for PR reviews. Empty = global level."],
    fr: ["Effort du relecteur de PR", "Niveau de réflexion des revues de PR. Vide = niveau global."],
    de: ["PR-Prüfer-Aufwand", "Denkstufe für PR-Reviews. Leer = globale Stufe."],
    es: ["Esfuerzo del revisor de PR", "Nivel de razonamiento de las revisiones de PR. Vacío = nivel global."],
  } },
  review_prompt: { adv: true, t: {
    en: ["PR review prompt", "Custom reviewer template. Empty = built-in template."],
    fr: ["Prompt de revue de PR", "Modèle personnalisé du relecteur. Vide = modèle intégré."],
    de: ["PR-Review-Prompt", "Eigene Prüfer-Vorlage. Leer = eingebaute Vorlage."],
    es: ["Prompt de revisión de PR", "Plantilla personalizada del revisor. Vacío = plantilla integrada."],
  } },
  verdict_approved: { adv: true, t: {
    en: ["Approval heading", "Posted as a heading on the PR. Your merge rules must accept it."],
    fr: ["Titre d'approbation", "Posté en titre sur la PR. Vos règles de merge doivent l'accepter."],
    de: ["Freigabe-Überschrift", "Als Überschrift im PR gepostet. Ihre Merge-Regeln müssen sie akzeptieren."],
    es: ["Título de aprobación", "Se publica como título en la PR. Sus reglas de merge deben aceptarlo."],
  } },
  verdict_changes: { adv: true, t: {
    en: ["Changes-requested heading", "Posted as a heading on the PR when changes are needed."],
    fr: ["Titre de demande de modifications", "Posté en titre sur la PR quand des changements sont demandés."],
    de: ["Überschrift für Änderungswünsche", "Als Überschrift im PR gepostet, wenn Änderungen nötig sind."],
    es: ["Título de cambios solicitados", "Se publica como título en la PR cuando se piden cambios."],
  } },
  review_overrides: { adv: true, t: {
    en: ["Per-repository exceptions", "JSON: trigger, prompt and labels specific to a repository."],
    fr: ["Exceptions par dépôt", "JSON : déclencheur, prompt et libellés propres à un dépôt."],
    de: ["Ausnahmen pro Repository", "JSON: Auslöser, Prompt und Bezeichnungen für ein Repository."],
    es: ["Excepciones por repositorio", "JSON: activación, prompt y etiquetas propios de un repositorio."],
  } },
  // ---- Général ----
  enabled: { t: {
    en: ["Run the orchestrator", "Saved setting, also applied now."],
    fr: ["Faire tourner l'orchestrateur", "Valeur enregistrée, appliquée aussi tout de suite."],
    de: ["Orchestrator ausführen", "Gespeicherte Einstellung, wird auch sofort angewendet."],
    es: ["Ejecutar el orquestador", "Valor guardado, que también se aplica ahora."],
  } },
  watch_prs: { t: {
    en: ["Follow workers' PRs", "Keeps workers with an open PR and recycles them after merge."],
    fr: ["Suivre les PR des workers", "Garde les workers qui ont une PR ouverte et les recycle après le merge."],
    de: ["PRs der Worker verfolgen", "Behält Worker mit offenem PR und recycelt sie nach dem Merge."],
    es: ["Seguir las PR de los workers", "Mantiene los workers con una PR abierta y los recicla tras el merge."],
  } },
  auto_patchnotes: { t: {
    en: ["Automatic release notes", "Adds a release-notes item and flips the release PR when the queue empties."],
    fr: ["Notes de version automatiques", "Ajoute un élément de notes de version et bascule la PR de release quand la file se vide."],
    de: ["Automatische Release Notes", "Fügt ein Release-Notes-Element hinzu und schaltet den Release-PR um, wenn die Warteschlange leer ist."],
    es: ["Notas de versión automáticas", "Añade un elemento de notas de versión y cambia la PR de release cuando la cola se vacía."],
  } },
  spawn_managers: { t: {
    en: ["Offer Managers", "One Manager per routed workspace, in the Backlog panel."],
    fr: ["Proposer des Managers", "Un Manager par espace de travail routé, dans le panneau Backlog."],
    de: ["Manager anbieten", "Ein Manager pro gerouteten Arbeitsbereich, im Backlog-Bereich."],
    es: ["Ofrecer Managers", "Un Manager por espacio de trabajo enrutado, en el panel Backlog."],
  } },
  integration_branch: { t: {
    en: ["Integration branch", "Base of feature PRs."],
    fr: ["Branche d'intégration", "Base des PR de fonctionnalité."],
    de: ["Integrationsbranch", "Basis der Feature-PRs."],
    es: ["Rama de integración", "Base de las PR de funcionalidad."],
  } },
  release_branch: { t: {
    en: ["Release branch", "Base of the release PR. Empty = no release PR."],
    fr: ["Branche de release", "Base de la PR de release. Vide = pas de PR de release."],
    de: ["Release-Branch", "Basis des Release-PRs. Leer = kein Release-PR."],
    es: ["Rama de release", "Base de la PR de release. Vacío = sin PR de release."],
  } },
  group: { t: {
    en: ["Sidebar group", "Group of the sessions it starts. Empty = no group."],
    fr: ["Groupe dans la barre latérale", "Groupe des sessions lancées. Vide = sans groupe."],
    de: ["Gruppe in der Seitenleiste", "Gruppe der gestarteten Sitzungen. Leer = keine Gruppe."],
    es: ["Grupo en la barra lateral", "Grupo de las sesiones lanzadas. Vacío = sin grupo."],
  } },
  agent: { t: {
    en: ["Agent profile", "Default profile for every role left empty."],
    fr: ["Profil d'agent", "Profil par défaut de chaque rôle laissé vide."],
    de: ["Agentenprofil", "Standardprofil für jede leere Rolle."],
    es: ["Perfil de agente", "Perfil por defecto de cada rol vacío."],
  } },
  metrics: { t: {
    en: ["Activity history", "Records queue samples for the Backlog activity chart."],
    fr: ["Historique d'activité", "Enregistre l'état de la file pour le graphique d'activité du Backlog."],
    de: ["Aktivitätsverlauf", "Speichert Warteschlangen-Stichproben für das Backlog-Aktivitätsdiagramm."],
    es: ["Historial de actividad", "Registra muestras de la cola para el gráfico de actividad del Backlog."],
  } },
  worktree_dir: { adv: true, t: {
    en: ["Worktree folder", "Root of the agents' dedicated worktrees."],
    fr: ["Dossier des worktrees", "Racine des worktrees dédiés aux agents."],
    de: ["Worktree-Ordner", "Wurzel der Worktrees der Agenten."],
    es: ["Carpeta de worktrees", "Raíz de los worktrees dedicados a los agentes."],
  } },
  merge_rule: { adv: true, t: {
    en: ["Agent merge rule", "Text given to agents about merging. Empty = the repository's rules file decides."],
    fr: ["Règle de merge des agents", "Texte donné aux agents sur le merge. Vide = le fichier de règles du dépôt décide."],
    de: ["Merge-Regel der Agenten", "Text für die Agenten zum Mergen. Leer = die Regeldatei des Repos entscheidet."],
    es: ["Regla de merge de los agentes", "Texto dado a los agentes sobre el merge. Vacío = decide el archivo de reglas del repositorio."],
  } },
  rules_files: { adv: true, t: {
    en: ["Repository rules files", "First existing file is given to agents, comma-separated."],
    fr: ["Fichiers de règles du dépôt", "Le premier fichier existant est donné aux agents, séparés par des virgules."],
    de: ["Regeldateien des Repos", "Die erste vorhandene Datei wird den Agenten gegeben, kommagetrennt."],
    es: ["Archivos de reglas del repositorio", "Se da a los agentes el primer archivo existente, separados por comas."],
  } },
  poll_secs: { adv: true, t: {
    en: ["Cycle period (s)", "How often the orchestrator checks the queue. Minimum 10 s."],
    fr: ["Période du cycle (s)", "Fréquence de vérification de la file. Minimum 10 s."],
    de: ["Zykluszeit (s)", "Wie oft die Warteschlange geprüft wird. Mindestens 10 s."],
    es: ["Periodo del ciclo (s)", "Frecuencia con la que se revisa la cola. Mínimo 10 s."],
  } },
  nudge_cooldown_secs: { adv: true, t: {
    en: ["Delay between relaunches (s)", "Minimum time between two relaunches of the same worker."],
    fr: ["Délai entre deux relances (s)", "Temps minimum entre deux relances du même worker."],
    de: ["Abstand zwischen Neustarts (s)", "Mindestzeit zwischen zwei Neustarts desselben Workers."],
    es: ["Espera entre relanzamientos (s)", "Tiempo mínimo entre dos relanzamientos del mismo worker."],
  } },
  workspace: { adv: true, t: {
    en: ["Default workspace", "Folder for untagged items. Empty = the daemon's git root."],
    fr: ["Espace de travail par défaut", "Dossier des éléments sans tag. Vide = racine git du démon."],
    de: ["Standard-Arbeitsbereich", "Ordner für Elemente ohne Tag. Leer = Git-Wurzel des Daemons."],
    es: ["Espacio de trabajo por defecto", "Carpeta de los elementos sin etiqueta. Vacío = raíz git del daemon."],
  } },
  state_dir: { adv: true, t: {
    en: ["State folder", "Shared by every orchestrator of the same queue. Empty = ~/.agentbacklog."],
    fr: ["Dossier d'état", "Partagé par tous les orchestrateurs de la même file. Vide = ~/.agentbacklog."],
    de: ["Statusordner", "Von allen Orchestratoren derselben Warteschlange geteilt. Leer = ~/.agentbacklog."],
    es: ["Carpeta de estado", "Compartida por todos los orquestadores de la misma cola. Vacío = ~/.agentbacklog."],
  } },
  mode: { adv: true, t: {
    en: ["Default mode", "Mode of the sessions it starts. Empty = profile default."],
    fr: ["Mode par défaut", "Mode des sessions lancées. Vide = défaut du profil."],
    de: ["Standardmodus", "Modus der gestarteten Sitzungen. Leer = Profilstandard."],
    es: ["Modo por defecto", "Modo de las sesiones lanzadas. Vacío = predeterminado del perfil."],
  } },
  model: { adv: true, t: {
    en: ["Default model", "Model of the sessions it starts. Empty = profile default."],
    fr: ["Modèle par défaut", "Modèle des sessions lancées. Vide = défaut du profil."],
    de: ["Standardmodell", "Modell der gestarteten Sitzungen. Leer = Profilstandard."],
    es: ["Modelo por defecto", "Modelo de las sesiones lanzadas. Vacío = predeterminado del perfil."],
  } },
  thought_level: { adv: true, t: {
    en: ["Default thinking level", "Applied after each start. Empty = unchanged."],
    fr: ["Niveau de réflexion par défaut", "Appliqué après chaque lancement. Vide = inchangé."],
    de: ["Standard-Denkstufe", "Nach jedem Start angewendet. Leer = unverändert."],
    es: ["Nivel de razonamiento por defecto", "Se aplica tras cada lanzamiento. Vacío = sin cambios."],
  } },
  manager_prompt: { adv: true, t: {
    en: ["Manager prompt", "Custom Manager template. Empty = built-in template."],
    fr: ["Prompt des Managers", "Modèle personnalisé des Managers. Vide = modèle intégré."],
    de: ["Manager-Prompt", "Eigene Manager-Vorlage. Leer = eingebaute Vorlage."],
    es: ["Prompt de los Managers", "Plantilla personalizada de los Managers. Vacío = plantilla integrada."],
  } },
  repo_workspaces: { adv: true, t: {
    en: ["Routed workspaces", "« repo=/folder » entries, comma-separated, to route items by repository."],
    fr: ["Espaces de travail routés", "Entrées « dépôt=/dossier » séparées par des virgules, pour router les éléments par dépôt."],
    de: ["Geroutete Arbeitsbereiche", "Einträge « repo=/ordner », kommagetrennt, um Elemente nach Repository zu routen."],
    es: ["Espacios de trabajo enrutados", "Entradas « repo=/carpeta » separadas por comas, para enrutar elementos por repositorio."],
  } },
};

/** Texte d'un paramètre de l'orchestrateur, ou undefined (clé inconnue). */
export function orchParamText(key: string): OrchParamText | undefined {
  return P[key];
}
