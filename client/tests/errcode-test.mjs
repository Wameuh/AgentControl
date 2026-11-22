// Test unitaire — codedErrorMsg : erreurs typées backend
// `update-failed:<kind>:<detail>` / `pair-failed:<kind>:<detail>`
// → libellé i18n du kind + detail brut (anglais, borné côté Rust).
// Kind inconnu ou erreur non typée → libellé `_other` + cause brute —
// le résultat remplace toujours l'erreur brute dans l'UI.
// Usage : node tests/errcode-test.mjs   — Node ≥ 22.18 requis
// (type stripping : errcode.ts + i18n importés sans transpilation).
import { codeLabel, codedErrorMsg } from "../src/core/errcode.ts";
import { initLang, setLang } from "../src/i18n/index.ts";

// Stubs DOM minimaux : i18n lit navigator/localStorage/document au
// setLang (Node ≥22 expose navigator — MOD résout en "Ctrl").
globalThis.localStorage ??= { getItem: () => null, setItem: () => {} };
globalThis.document ??= {
  documentElement: {},
  querySelectorAll: () => [],
};
initLang();

let fails = 0;
const ok = (c, n) => {
  console.log((c ? "  PASS " : "  FAIL ") + n);
  if (!c) fails++;
};

// --- update ---
ok(
  codedErrorMsg("update-failed:check:no release listed", "update") ===
    "The release could not be checked — no release listed",
  "updfail_check + detail"
);
ok(
  codedErrorMsg("update-failed:auth-cancelled:polkit rc=126", "update").startsWith(
    "Authentication cancelled"
  ),
  "updfail_auth_cancelled (kind avec tiret → clé snake)"
);
ok(
  codedErrorMsg("update-failed:auth-timeout:t", "update").startsWith(
    "No answer from the authentication agent"
  ),
  "updfail_auth_timeout"
);
ok(
  codedErrorMsg("update-failed:future-kind:det", "update") === "Update failed — det",
  "kind inconnu → updfail_other + detail"
);
ok(
  codedErrorMsg("plain backend cause", "update") ===
    "Update failed — plain backend cause",
  "erreur non typée → _other + brut"
);

// --- pair ---
ok(
  codedErrorMsg("pair-failed:ssh-only:x", "pair").startsWith(
    "Only SSH connections can be paired"
  ),
  "pairfail_ssh_only"
);
ok(
  codedErrorMsg("pair-failed:key-too-big:K=9999", "pair").startsWith(
    "Key too large for a QR code"
  ),
  "pairfail_key-too-big → pairfail_key_too_big"
);
ok(
  codedErrorMsg("pair-failed:nohk:z", "pair").startsWith(
    "No host key on the server"
  ),
  "pairfail_nohk"
);
ok(
  codedErrorMsg("pair-failed:transport:ssh: broken pipe", "pair").startsWith(
    "The SSH call to the server failed"
  ),
  "pairfail_transport"
);
ok(
  codedErrorMsg("surprise", "pair") === "Pairing failed — surprise",
  "pair non typée → _other + brut"
);

// --- codeLabel : codes `warn:` serveur → clés qr_warn_* ---
ok(
  codeLabel("hk-mismatch", "qr_warn").startsWith("The server's fingerprints"),
  "qr_warn_hk_mismatch (tiret → snake)"
);
ok(
  codeLabel("sshd-version-unknown", "qr_warn").startsWith("Could not determine"),
  "qr_warn_sshd_version_unknown"
);
ok(codeLabel("new-server-code", "qr_warn") === "new-server-code", "warn inconnu → brut");

// --- i18n : les kinds passent par les 4 dictionnaires ---
setLang("fr");
ok(
  codedErrorMsg("update-failed:install:x", "update").startsWith("L'installation a échoué"),
  "fr : updfail_install"
);
ok(
  codedErrorMsg("pair-failed:lock:x", "pair").includes("occupé"),
  "fr : pairfail_lock"
);
setLang("de");
ok(
  codedErrorMsg("pair-failed:lock:x", "pair").includes("beschäftigt"),
  "de : pairfail_lock"
);
ok(
  codedErrorMsg("update-failed:writable:d", "update").includes("beschreibbar"),
  "de : updfail_writable"
);
setLang("es");
ok(
  codedErrorMsg("pair-failed:lock:x", "pair").includes("ocupado"),
  "es : pairfail_lock"
);
setLang("en");

console.log(fails ? `${fails} FAIL` : "all ok");
process.exit(fails ? 1 : 0);
