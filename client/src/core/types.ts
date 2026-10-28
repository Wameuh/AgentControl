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
