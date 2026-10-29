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

/** Forge active déclarée par le démon (`forge_list`). */
export interface ForgeInfo {
  type: string;
  /** Entrées `{host, token_env?}` — host[:port][/prefix]. */
  hosts: { host: string; token_env?: string }[];
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
};

export type SendMode = "normal" | "steer" | "force";

export type ConnLive = "ok" | "reconnecting" | "down";
