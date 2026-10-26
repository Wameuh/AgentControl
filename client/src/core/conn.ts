// ---------------------------------------------------------------------------
// Helpers de connexion/démon — fonctions pures (clés, versions, params).
// ---------------------------------------------------------------------------

// Doit refléter conn_key() côté Rust : ssh → "ssh{,-native}:host:sshport".
export function connKeyOf(cfg: Record<string, unknown>, isAndroid = false) {
  if (cfg.kind === "ssh") {
    const nat = cfg.native === true || isAndroid;
    const p = Number(cfg.port) || 22;
    return `${nat ? "ssh-native" : "ssh"}:${cfg.host}:${p}`;
  }
  return cfg.kind === "tcp" ? `tcp:${cfg.addr}` : "local";
}

/// Params ensure_daemon — forward les champs ssh de la cfg (natif ou CLI).
export function daemonParams(cfg: Record<string, unknown>, tag: string) {
  return {
    host: cfg.host,
    args: cfg.args ?? [],
    tag,
    port: cfg.port ?? null,
    user: cfg.user ?? null,
    password: cfg.password ?? null,
    key: cfg.key ?? null,
    native: cfg.native ?? null,
  };
}

export const minorOf = (v: string) => v.split(".").slice(0, 2).join(".");

export function cmpVer(a: string, b: string): number {
  const pa = a.split(".").map(Number);
  const pb = b.split(".").map(Number);
  for (let i = 0; i < 3; i++) {
    const d = (pa[i] || 0) - (pb[i] || 0);
    if (d) return d;
  }
  return 0;
}
