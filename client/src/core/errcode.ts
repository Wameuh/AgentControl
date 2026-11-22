// Erreurs typées backend — `<ns>-failed:<kind>:<detail>` (update.rs,
// pair.rs) : le kind est une clé stable traduite ici (`<prefix>_<kind>`),
// le detail un diagnostic anglais borné côté Rust affiché tel quel.
// Même patron que connectErrorMsg (conn-view) — la classe n'est jamais
// mémorisée. Kind inconnu ou préfixe absent → `<prefix>_other` + brut.

import { t, type Key } from "../i18n/index.ts";

const PREFIX = { update: "updfail", pair: "pairfail" } as const;

/** Message affichable d'une erreur `ns`-failed. Toujours défini — le
 *  résultat remplace l'erreur brute dans l'UI. */
export function codedErrorMsg(e: unknown, ns: keyof typeof PREFIX): string {
  const s = String(e).trim();
  const m = new RegExp(`^${ns}-failed:([a-z-]+)(?::([\\s\\S]+))?$`).exec(s);
  const detail = (m?.[2] ?? s).replace(/\s+/g, " ").trim();
  const p = PREFIX[ns];
  const key = `${p}_${m?.[1]?.replace(/-/g, "_") ?? "other"}` as Key;
  // t() renvoie la clé brute quand elle est absente → repli _other.
  const msg = m?.[1] && t(key) !== key ? t(key) : t(`${p}_other` as Key);
  return detail ? `${msg} — ${detail}` : msg;
}
