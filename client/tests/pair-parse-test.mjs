// Test unitaire — parsing strict des payloads d'appairage B′ (v2.1),
// endpoint dégradé et repli A (URI v1). Règle : un QR malformé est
// REFUSÉ net (jamais réparé à l'aveugle) ; `hk` est obligatoire dès
// qu'une clé jetable `k` est présente (le pinning est la protection
// MITM — un QR sans empreintes ne doit pas appairer).
// Usage : node tests/pair-parse-test.mjs   — Node ≥ 22.18 requis
// (type stripping : pair-payload.ts importé sans transpilation).
import { parsePair } from "../src/features/pair-payload.ts";

let fails = 0;
const ok = (c, n) => {
  console.log((c ? "  PASS " : "  FAIL ") + n);
  if (!c) fails++;
};

const FP = "SHA256:" + "A".repeat(43);
const K = "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\n-----END OPENSSH PRIVATE KEY-----";
const v2 = (o = {}) =>
  JSON.stringify({ v: 2, host: "srv.local", port: 22, user: "dev",
    rport: 7331, k: K, hk: [FP], exp: 9999999999, ...o });

// --- v2.1 nominal ---
let r = parsePair(v2());
ok(r?.kind === "v2" && r.p.hk.length === 1 && r.p.rport === 7331, "v2.1 nominal → kind v2, hk + rport");
ok(parsePair(v2({ hk: [FP, FP + "BB"] }))?.p.hk.length === 2, "hk multi-entrées (plusieurs host keys)");

// --- rejet si `hk` manquant/malformé quand `k` présent (F-HK1) ---
ok(parsePair(v2({ hk: undefined })) === null, "v2 sans hk → refus");
ok(parsePair(v2({ hk: [] })) === null, "v2 hk vide → refus");
ok(parsePair(v2({ hk: "SHA256:AAA" })) === null, "v2 hk non-tableau → refus");
ok(parsePair(v2({ hk: ["MD5:aaaa"] })) === null, "v2 fp non-SHA256 → refus");
ok(parsePair(v2({ hk: ["SHA256:!"] })) === null, "v2 fp alphabet sale → refus");
ok(parsePair(v2({ hk: [FP, 42] })) === null, "v2 hk mixte sale → refus");
ok(parsePair(v2({ hk: new Array(20).fill(FP) })) === null, "v2 hk >16 → refus");

// --- `k` doit être un PEM plausible ---
ok(parsePair(v2({ k: "pas un pem" })) === null, "v2 k non-PEM → refus");
ok(parsePair(v2({ k: 42 })) === null, "v2 k non-chaîne → refus");

// --- endpoint dégradé : ni k ni hk ---
r = parsePair(JSON.stringify({ v: 2, host: "srv.local", port: 22, user: "dev", rport: 7331 }));
ok(r?.kind === "endpoint" && r.p.host === "srv.local", "v2 sans k/hk → kind endpoint");
ok(parsePair(JSON.stringify({ v: 2, host: "srv.local", port: 22, user: "dev", rport: 7331, hk: [FP] })) === null,
  "endpoint avec hk mais sans k → refus (QR tronqué, ne pinne pas)");

// --- champs bornés ---
ok(parsePair(v2({ host: "hôte espace" })) === null, "host avec espace → refus");
ok(parsePair(v2({ host: "a@b" })) === null, "host avec @ → refus");
ok(parsePair(v2({ host: "a/b" })) === null, "host avec / → refus");
ok(parsePair(v2({ port: 0 })) === null, "port 0 → refus");
ok(parsePair(v2({ port: 70000 })) === null, "port >65535 → refus");
ok(parsePair(v2({ rport: "7331" })) === null, "rport chaîne → refus");
ok(parsePair(v2({ user: "a b" })) === null, "user avec espace → refus");
ok(parsePair(v2({ user: "x".repeat(65) })) === null, "user >64 → refus");
ok(parsePair(JSON.stringify({ v: 3, host: "h", port: 1, user: "u", rport: 1, k: K, hk: [FP] })) === null,
  "v != 2 → refus");
ok(parsePair("{pas json") === null, "json invalide → refus");
ok(parsePair(JSON.stringify(null)) === null, "json null → refus");

// --- repli A v1 (secrets émis en base64url SANS padding) ---
const uri = (extra = "") =>
  "agentcontrol://pair?v=1&host=srv.local&port=22&rport=7331&user=dev&auth=pw&secret=cHc" + extra;
r = parsePair(uri());
ok(r?.kind === "v1" && r.p.secret === "pw" && r.p.host === "srv.local", "v1 pw nominal");
r = parsePair(
  "agentcontrol://pair?v=1&host=srv.local&port=22&rport=7331&user=dev&auth=key&secret=" +
    Buffer.from("-----BEGIN OPENSSH PRIVATE KEY-----\nx\n-----END OPENSSH PRIVATE KEY-----")
      .toString("base64url"));
ok(r?.kind === "v1" && r.p.auth === "key" && /BEGIN OPENSSH/.test(r.p.secret), "v1 key nominal (b64url)");
ok(parsePair("https://pair?v=1&host=x") === null, "schéma https → refus");
ok(parsePair("agentcontrol://other?v=1&host=x") === null, "action inconnue → refus");
ok(parsePair("agentcontrol://pair?v=2&host=x&user=u&auth=pw&secret=cHc") === null,
  "v1 avec v=2 → refus (ce n'est PAS un v2 — URI ≠ JSON)");
ok(parsePair(uri().replace("secret=cHc", "secret=%%%")) === null,
  "secret b64url sale → refus");
ok(parsePair(uri("&evil=1"))?.kind === "v1", "param inconnu ignoré (whitelist)");
ok(parsePair("agentcontrol://pair?v=1&host=x&user=u&auth=key&secret=cHc=") === null,
  "v1 auth=key sans PEM → refus");

// --- dispatch ---
ok(parsePair("  " + v2() + "  ")?.kind === "v2", "JSON espacé → v2");
ok(parsePair("") === null, "vide → refus");
ok(parsePair("ssh-ed25519 AAAA") === null, "pubkey brute → refus");

console.log(fails === 0 ? "\nTous les tests passent." : `\n${fails} échec(s).`);
process.exit(fails ? 1 : 0);
