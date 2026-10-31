// ---------------------------------------------------------------------------
// Types miroir du protocole (+ quelques types UI transverses)
// ---------------------------------------------------------------------------

export type CmdInfo = string | { name: string; description?: string; hint?: string };

export interface SessionMeta {
  conn?: string;
  /** Connexion down → session affichée depuis le cache (grisée). */
  offline?: boolean;
  group?: string;
  id: string;
  agent: string;
  title: string;
  cwd: string;
  created_at: string;
  native_session_id?: string;
  mode?: string;
  model?: string;
  available_modes: string[];
  available_commands: CmdInfo[];
  available_models: string[];
  config_options?: ConfigOption[];
  state: string;
  context_used?: number;
  context_size?: number;
  /** Session créatrice (spawn agent→agent) — affichage imbriqué sidebar. */
  spawned_by?: string;
  /** Provenance externe (session créée par un service via new_session
   *  `origin`) — badge « via » sidebar + marquage du prompt initial. */
  origin?: string;
}

/** Registre PR ↔ session du démon (prs.json) — badge sidebar. */
export interface PrEntry {
  conn?: string;
  repo: string;
  pr: number;
  session: string;
  branch?: string;
  title?: string;
  status?: string;
  note?: string;
  via?: string;
  /** Type de forge : github (défaut) | gitlab | gerrit. */
  forge?: string;
  /** Hôte web canonique (`gitlab.example.com`, `gerrit.corp:8080`). */
  host?: string;
  /** URL canonique de la référence (lien badge). */
  url?: string;
  /** `I<40 hex>` Gerrit — adressage quand `pr` vaut 0. */
  change_id?: string;
  /** Syncs consécutifs sans lecture live — au-delà de
   *  `PR_UNRESOLVED_AFTER` (3 côté proto) l'entrée est « non
   *  résoluble » : repo/hôte injoignable ou référence inexistante. */
  sync_fails?: number;
  /** Horodatages ISO du registre — tri de la section « terminées ». */
  created_at?: string;
  updated_at?: string;
}

/** État runtime d'un composant (champ `status` des items ListComponents). */
export interface CompStatus {
  /** starting | running | degraded | stopped | failed — ou les états
   *  d'observation d'un `tool` : installed | absent (snake_case proto). */
  state: string;
  /** Verdict de la dernière sonde : "ok" | "fail" — absent sans sonde. */
  health?: string;
  pid?: number;
  /** Secondes depuis le (re)spawn courant — 0 sans process vivant. */
  uptime_s?: number;
  /** Respawns supervisés depuis le boot du démon. */
  restarts?: number;
  /** Code du dernier exit (128+signal si tué par signal). */
  last_exit?: number;
  /** Version sondée de l'outil managé (`version_argv`, ou `command`
   *  d'un tool) — 1re ligne ≤200 c. */
  version?: string;
  /** Seuil `min_version` déclaré (spec ou entrée catalogue homonyme). */
  version_min?: string;
  /** Version sondée sous `version_min` — badge « mise à jour requise ». */
  version_outdated?: boolean;
}

/** Item ListComponents : spec aplatie + état (`item_json` côté démon). */
export interface CompItem {
  name: string;
  /** internal | process | external | tool. */
  kind: string;
  autostart?: boolean;
  description?: string;
  /** Provenance de la spec : "internal" | "components.toml". */
  source?: string;
  /** Spec complète (command, env, healthcheck…), sérialisée telle quelle. */
  spec?: Record<string, unknown>;
  status?: CompStatus;
  /** Instantané de l'orchestrateur natif — présent sur l'item
   *  `backlog-orchestrator` (status_json côté démon). `workspaces` =
   *  routing repo → cwd pour la surface « Manager » (vide si éteint). */
  orchestrator?: {
    enabled?: boolean;
    workspaces?: { repo?: string; cwd?: string }[];
    [k: string]: unknown;
  };
}

export interface ConfigOpt { value: string; name: string; description?: string }
export interface ConfigOption {
  id: string; name: string; category?: string;
  currentValue?: string; options: ConfigOpt[];
}

export interface AcEvent {
  seq: number;
  ts: string;
  type: string;
  [k: string]: unknown;
}

export interface DirEntry { name: string; dir: boolean; size: number }

export type AgentOpt = { id: string; name: string; available?: boolean };

// Type alias (pas interface) → signature d'index implicite, assignable à
// Record<string, unknown> pour connKeyOf().
export type SavedConn = {
  kind: string;
  host?: string;
  addr?: string;
  path?: string;
  remote_port?: number;
  args?: string[];
  port?: number;
  user?: string;
  password?: string;
  key?: string;
  native?: boolean;
  /** Empreintes `SHA256:…` de host key épinglées — persistées par
   *  l'appairage B′ ; toute connexion native du profil les impose. */
  host_keys?: string[];
  /** Accent propre à la connexion (multi-instances : une couleur par
   *  démon pour identifier l'instance d'un coup d'œil). Nom d'accent
   *  ("sage" | "gold" | …) — absent = accent global des réglages. */
  color?: string;
};

export type SendMode = "normal" | "steer" | "force";

export type ConnLive = "ok" | "reconnecting" | "down";

// ---------------------------------------------------------------------------
// Composants managés (miroir proto::components — docs/components-integration.md)
// ---------------------------------------------------------------------------

export type ComponentKind = "internal" | "process" | "external" | "tool";
export type ComponentState =
  | "starting" | "running" | "degraded" | "stopped" | "failed"
  // États passifs des `tool` (proto ≥0.8) — + tolérance aux états futurs.
  | "installed" | "absent" | (string & {});

/** healthcheck sérialisé `{kind, …}` (tag interne serde). */
export type ComponentHealthcheck =
  | { kind: "none" }
  | { kind: "process" }
  | { kind: "tcp"; addr: string }
  | { kind: "http"; url: string }
  | { kind: "command"; argv: string[] };

/** Type d'un paramètre déclaré — pilote le widget du formulaire et la
 *  validation côté démon. */
export type ParamKind =
  | "string" | "path" | "int" | "duration" | "bool" | "enum" | "list" | "json";

/** Paramètre de config déclaré (catalogue → spec.params_schema) : la
 *  seule surface éditable d'un composant managé — la spec interne
 *  (command/restart/healthcheck) reste propriété du catalogue. */
export interface ParamSpec {
  key: string;
  label: string;
  description?: string | null;
  required: boolean;
  kind: ParamKind;
  /** Forme chaîne — castée selon kind. */
  default?: string | null;
  secret?: boolean;
  /** Regex (string/path). */
  validation?: string | null;
  /** Valeurs autorisées (enum). */
  choices?: string[];
}

/** Spec [[components]] de components.toml — lue et écrite telle quelle. */
export interface ComponentSpec {
  name: string;
  kind: ComponentKind;
  command: string[];
  /** Sonde de version de l'outil managé (process/external ; remplace
   *  `command` pour un tool) — déclarée par le catalogue ou à la main. */
  version_argv?: string[] | null;
  /** Version minimale exigée de l'outil managé → badge « obsolète ». */
  min_version?: string | null;
  env: Record<string, string>;
  cwd: string | null;
  autostart: boolean;
  restart: "always" | "on-failure" | "never";
  restart_sec: number;
  healthcheck: ComponentHealthcheck;
  description: string | null;
  /** Entrée catalogue d'origine — non nul = composant managé par
   *  catalogue : l'édition utilisateur se borne aux `params`. */
  catalog?: string | null;
  /** Paramètres éditables déclarés (schéma copié à l'install). */
  params_schema?: ParamSpec[];
  /** Valeurs effectives des params — `secret` masqués (« •••••• »). */
  params?: Record<string, string>;
  /** Cible de rendu de la config managée (fichier possédé démon). */
  render?: { path: string; template: string } | null;
  /** Requêtes de données déclarées (`component_query` → argv). */
  queries?: Record<string, string[]>;
}

export interface ComponentStatus {
  state: ComponentState;
  /** "ok" | "fail" | "outdated" — « outdated » = outil sous
   *  `version_min` (actionnable : mettre à jour l'outil). */
  health?: string;
  pid?: number;
  uptime_s?: number;
  restarts?: number;
  last_exit?: number;
  /** Version sondée de l'outil managé (≤200 c). */
  version?: string;
  /** Seuil `min_version` déclaré (spec ou entrée catalogue homonyme). */
  version_min?: string;
  /** Version sondée sous `version_min` — badge « mise à jour requise ». */
  version_outdated?: boolean;
  /** Params required sans valeur — « configuration requise » :
   *  spawn bloqué tant que la liste n'est pas vide. */
  config_required?: string[];
}

/** Item ListComponents / ComponentInfo : spec + état runtime. `source`
 *  = "internal" (déclaré en code, non éditable) | "components.toml".
 *  `catalog` = le nom est couvert par une entrée du catalogue →
 *  édition bornée aux params déclarés, spec interne scellée. */
export interface ComponentItem {
  name: string;
  kind: ComponentKind;
  autostart: boolean;
  description?: string | null;
  source?: string;
  catalog?: boolean;
  spec: ComponentSpec;
  status: ComponentStatus;
}

/** Entrée du catalogue embarqué (ComponentCatalog) : métadonnées +
 *  detection installed/version + sondes upstream. */
export interface CatalogEntry {
  name: string;
  description: string;
  repo?: string | null;
  homepage?: string | null;
  suggested_spec: ComponentSpec;
  /** Paramètres éditables déclarés — collectés au dialogue d'install. */
  params?: ParamSpec[];
  installed: boolean;
  version?: string | null;
  latest_version?: string | null;
  latest_notes?: string | null;
}
