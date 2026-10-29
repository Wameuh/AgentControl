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
}

/** État runtime d'un composant (champ `status` des items ListComponents). */
export interface CompStatus {
  /** starting | running | degraded | stopped | failed (proto snake_case). */
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
