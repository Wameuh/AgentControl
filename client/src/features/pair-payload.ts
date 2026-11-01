// ---------------------------------------------------------------------------
// Parsing strict des payloads d'appairage — module SANS aucune dépendance
// (importable en Node nu pour les tests unitaires, ex. tests/pair-parse).
//
// Formats acceptés :
//   v2.1 (B′) : {"v":2,"host","port","user","rport","k":"<PEM>","hk":[…],"exp"}
//               → kind "v2" — `k` + `hk` obligatoires ensemble
//   endpoint dégradé : {"v":2,"host","port","user","rport"} (sans k/hk)
//               → kind "endpoint" — clé de l'appareil posée à la main
//   v1 (repli A) : agentcontrol://pair?v=1&…&secret=<b64url>
//               → kind "v1" — credential complet du profil
// ---------------------------------------------------------------------------

export interface PairV1 {
  host: string;
  port: number;
  rport: number;
  user: string;
  auth: "key" | "pw";
  /** auth=key → PEM de la clé privée ; auth=pw → mot de passe. */
  secret: string;
  pw?: string;
  color?: string;
}

export interface PairV2 {
  host: string;
  port: number;
  user: string;
  rport: number;
  /** PEM ed25519 de la clé jetable K — jamais persistée. */
  k?: string;
  /** Empreintes "SHA256:…" des host keys du serveur — pinning russh. */
  hk?: string[];
  /** Expiration serveur (epoch s) — indicatif UI seulement. */
  exp?: number;
}

export type PairScan =
  | { kind: "v1"; p: PairV1 }
  | { kind: "v2"; p: PairV2 }
  | { kind: "endpoint"; p: PairV2 };

const HOST_RE = /^[^\s/@]{1,253}$/;
const FP_RE = /^SHA256:[A-Za-z0-9+/]{40,48}=*$/;

function portOf(v: unknown): number | null {
  return typeof v === "number" && Number.isInteger(v) && v >= 1 && v <= 65535
    ? v
    : null;
}

function hostOf(v: unknown): string | null {
  return typeof v === "string" && HOST_RE.test(v) ? v : null;
}

function userOf(v: unknown): string | null {
  return typeof v === "string" && v.length <= 64 && !/[\s@]/.test(v) ? v : null;
}

/** Parse strict du payload v2.1 JSON (avec `k` → "v2", sans → "endpoint"). */
export function parsePairV2(text: string): PairScan | null {
  let j: Record<string, unknown>;
  try {
    j = JSON.parse(text);
  } catch {
    return null;
  }
  if (!j || typeof j !== "object" || j.v !== 2) return null;
  const host = hostOf(j.host);
  const port = portOf(j.port);
  const rport = portOf(j.rport);
  const user = userOf(j.user);
  if (host == null || port == null || rport == null || user == null) return null;
  const p: PairV2 = { host, port, user, rport };
  if (typeof j.exp === "number" && j.exp > 0) p.exp = j.exp;
  if (j.k == null) {
    // Endpoint seul (mode dégradé H) — `hk` seul sans `k` est suspect :
    // on l'exige absent aussi (un QR tronqué ne décide pas du pinning).
    if (j.hk != null) return null;
    return { kind: "endpoint", p };
  }
  if (typeof j.k !== "string" || !/-----BEGIN [A-Z ]*PRIVATE KEY-----/.test(j.k)) {
    return null;
  }
  if (!Array.isArray(j.hk) || j.hk.length === 0 || j.hk.length > 16) return null;
  for (const f of j.hk) {
    if (typeof f !== "string" || !FP_RE.test(f)) return null;
  }
  p.k = j.k;
  p.hk = j.hk as string[];
  return { kind: "v2", p };
}

/** base64url (sans padding) → texte UTF-8 ; null si alphabet invalide. */
function b64urlDecode(s: string): string | null {
  if (!/^[A-Za-z0-9_-]+$/.test(s)) return null;
  try {
    const bin = atob(s.replace(/-/g, "+").replace(/_/g, "/"));
    const bytes = Uint8Array.from(bin, (c) => c.charCodeAt(0));
    return new TextDecoder().decode(bytes);
  } catch {
    return null;
  }
}

/** Param numérique borné — absent → `def`, présent mais invalide → null
 *  (rejet strict : un QR malformé n'est pas réparé à l'aveugle). */
function numParam(p: URLSearchParams, k: string, def: number): number | null {
  const v = p.get(k);
  if (v == null) return def;
  const n = Number(v);
  return Number.isInteger(n) && n >= 1 && n <= 65535 ? n : null;
}

const V1_KNOWN = new Set([
  "v", "host", "port", "rport", "user", "auth", "secret", "pw", "color",
]);

/** Parse strict de l'URI v1 `agentcontrol://pair?…` (repli A — whitelist :
 *  tout paramètre hors V1_KNOWN est ignoré). */
export function parsePairV1(text: string): PairScan | null {
  let u: URL;
  try {
    u = new URL(text.trim());
  } catch {
    return null;
  }
  if (u.protocol !== "agentcontrol:") return null;
  // `agentcontrol://pair?…` → host "pair" ; `agentcontrol:pair?…` →
  // pathname. Le premier est le format émis, le second toléré.
  const action = u.host || u.pathname.replace(/^\//, "");
  if (action !== "pair") return null;
  const p = u.searchParams;
  for (const k of [...p.keys()]) {
    if (!V1_KNOWN.has(k)) p.delete(k);
  }
  if (p.get("v") !== "1") return null;
  const host = hostOf(p.get("host") ?? "");
  const port = numParam(p, "port", 22);
  const rport = numParam(p, "rport", 7331);
  const user = userOf(p.get("user") ?? "");
  if (host == null || port == null || rport == null || user == null) return null;
  const auth = p.get("auth");
  if (auth !== "key" && auth !== "pw") return null;
  const secret = b64urlDecode(p.get("secret") ?? "");
  if (secret == null) return null;
  // auth=key → le secret doit ressembler à un PEM (la validation
  // complète — parse russh — est faite à l'import, qui refuse un
  // secret tronqué).
  if (auth === "key" && !/-----BEGIN [A-Z ]*PRIVATE KEY-----/.test(secret)) {
    return null;
  }
  const pwRaw = p.get("pw");
  const pw = pwRaw == null ? undefined : b64urlDecode(pwRaw);
  if (pw === null) return null;
  const color = p.get("color") ?? undefined;
  const prof: PairV1 = { host, port, rport, user, auth, secret, pw };
  // color est validé par l'appelant (accentOf) — passé tel quel ici.
  if (color && /^[a-z]{1,16}$/.test(color)) prof.color = color;
  return { kind: "v1", p: prof };
}

/** Dispatch : JSON v2.1 → parsePairV2 ; URI agentcontrol → parsePairV1. */
export function parsePair(text: string): PairScan | null {
  const t = text.trim();
  if (t.startsWith("{")) return parsePairV2(t);
  return parsePairV1(t);
}
