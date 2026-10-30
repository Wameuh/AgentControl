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
  /** Version sondée d'un `tool` (1re ligne de sa commande, ≤200 c). */
  version?: string;
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

/** Spec [[components]] de components.toml — lue et écrite telle quelle. */
export interface ComponentSpec {
  name: string;
  kind: ComponentKind;
  command: string[];
  env: Record<string, string>;
  cwd: string | null;
  autostart: boolean;
  restart: "always" | "on-failure" | "never";
  restart_sec: number;
  healthcheck: ComponentHealthcheck;
  description: string | null;
}

export interface ComponentStatus {
  state: ComponentState;
  health?: string;
  pid?: number;
  uptime_s?: number;
  restarts?: number;
  last_exit?: number;
  /** Version sondée d'un `tool` (1re ligne de sa commande, ≤200 c). */
  version?: string;
}

/** Item ListComponents / ComponentInfo : spec + état runtime. `source`
 *  = "internal" (déclaré en code, non éditable) | "components.toml". */
export interface ComponentItem {
  name: string;
  kind: ComponentKind;
  autostart: boolean;
  description?: string | null;
  source?: string;
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
  installed: boolean;
  version?: string | null;
  latest_version?: string | null;
  latest_notes?: string | null;
}
