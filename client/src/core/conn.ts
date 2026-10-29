// ---------------------------------------------------------------------------
// Helpers de connexion/démon — fonctions pures (clés, versions, params).
// ---------------------------------------------------------------------------

// Doit refléter conn_key() côté Rust : ssh → "ssh{,-native}:host:sshport".
// "web" = connexion unique du mode navigateur (le démon qui sert la SPA).
export function connKeyOf(cfg: Record<string, unknown>, isAndroid = false) {
  if (cfg.kind === "web") return "web";
  if (cfg.kind === "ssh") {
    const nat = cfg.native === true || isAndroid;
    const p = Number(cfg.port) || 22;
    return `${nat ? "ssh-native" : "ssh"}:${cfg.host}:${p}`;
  }
  if (cfg.kind === "tcp") return `tcp:${cfg.addr}`;
  if (cfg.kind === "local") return "local";
  // kind inconnu/absent (fichier legacy corrompu) : identité « kind:host »
  // — miroir de conn_key_json côté Rust, pas de fusion avec "local"
  // (dédup/suppression croisées entre entrées corrompues).
  return `${cfg.kind ?? ""}:${cfg.host ?? ""}`;
}

/// Params ensure_daemon — forward les champs ssh de la cfg (natif ou CLI).
export function daemonParams(cfg: Record<string, unknown>, tag: string) {
  return {
    host: cfg.host,
    args: cfg.args ?? [],
    tag,
    port: cfg.port ?? null,
    remotePort: cfg.remote_port ?? null,
    user: cfg.user ?? null,
    password: cfg.password ?? null,
    key: cfg.key ?? null,
    native: cfg.native ?? null,
  };
}

/** "ssh:user@hôte:22" → "user@hôte" pour l'affichage compact
 *  (le port ssh par défaut est masqué, les autres gardés). */
export const connLabel = (k: string) =>
  k.replace(/^(ssh-native|ssh|tcp):/, "").replace(/:22$/, "");

export const minorOf = (v: string) => v.split(".").slice(0, 2).join(".");

/// "1.2.3-rc.4" → parties numériques + identifiants de pré-release.
/// Le build metadata `+…` est ignoré (semver).
function vParts(v: string): { n: number[]; pre?: string[] } {
  const noBuild = v.split("+")[0];
  const i = noBuild.indexOf("-");
  const core = i < 0 ? noBuild : noBuild.slice(0, i);
  return {
    n: core.split(".").map((x) => parseInt(x, 10) || 0),
    pre: i < 0 ? undefined : noBuild.slice(i + 1).split("."),
  };
}

/// Compare semver : pré-release < stable de même X.Y.Z, et dans les
/// identifiants numérique < alphanum (semver §11). "0.6" toléré.
export function cmpVer(a: string, b: string): number {
  const pa = vParts(a);
  const pb = vParts(b);
  for (let i = 0; i < 3; i++) {
    const d = (pa.n[i] || 0) - (pb.n[i] || 0);
    if (d) return d;
  }
  if (!pa.pre && !pb.pre) return 0;
  if (!pa.pre) return 1;
  if (!pb.pre) return -1;
  for (let i = 0; i < Math.max(pa.pre.length, pb.pre.length); i++) {
    const x = pa.pre[i];
    const y = pb.pre[i];
    if (x === undefined) return -1;
    if (y === undefined) return 1;
    const xn = /^\d+$/.test(x);
    const yn = /^\d+$/.test(y);
    if (xn && yn) {
      const d = Number(x) - Number(y);
      if (d) return d;
    } else if (xn !== yn) {
      return xn ? -1 : 1;
    } else if (x !== y) {
      return x < y ? -1 : 1;
    }
  }
  return 0;
}
