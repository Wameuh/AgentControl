// Test unitaire — tri semver de cmpVer sous identité rc compilée.
// Un client rc Windows/macOS s'identifie « X.Y.Z-rc.K » (AGENTCONTROL_RC
// compilée, version bundle toujours numérique) : l'updater doit alors
// proposer rc.K+1, puis la finale X.Y.Z — avant, l'artefact rapportait
// « X.Y.Z » et remoteVersion() « X.Y.Z-rc.K' » ne dépassait jamais la
// locale (prerelease < stable à X.Y.Z égal) → « à jour » à vie.
// Usage : node tests/cmpver-test.mjs   — Node ≥ 22.18 requis (type
// stripping : conn.ts est importé sans transpilation).
import { cmpVer } from "../src/core/conn.ts";

let fails = 0;
const ok = (c, n) => {
  console.log((c ? "  PASS " : "  FAIL ") + n);
  if (!c) fails++;
};

// Le cas du bug : la locale rc doit perdre face à la rc suivante et à
// la finale — les deux sont proposées en update.
ok(cmpVer("0.10.0-rc.2", "0.10.0-rc.1") > 0, "rc.K+1 > rc.K (même X.Y.Z)");
ok(cmpVer("0.10.0-rc.10", "0.10.0-rc.9") > 0, "ordre numérique des K (10 > 9)");
ok(cmpVer("0.10.0", "0.10.0-rc.9") > 0, "finale > toute rc du même X.Y.Z");
ok(cmpVer("0.10.0-rc.1", "0.10.0") < 0, "rc < stable — la régression corrigée");
ok(cmpVer("0.10.0-rc.1", "0.10.0-rc.1") === 0, "rc.K == rc.K → à jour");

// Garde-fous du tri général : la rc reste > versions antérieures, et
// un patch supérieur reste proposé depuis la rc.
ok(cmpVer("0.10.0-rc.1", "0.9.9") > 0, "rc X.Y.Z > stable antérieure");
ok(cmpVer("0.10.1", "0.10.0-rc.2") > 0, "patch ultérieur > rc");
ok(cmpVer("0.10.0", "0.10.0-rc.1+build") > 0, "build metadata ignorée");

process.exit(fails ? 1 : 0);
