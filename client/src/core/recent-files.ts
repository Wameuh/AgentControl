// ---------------------------------------------------------------------------
// Fichiers récemment ouverts — une pile courte par session, persistée dans
// localStorage sous "ac-recent-files" ({ "conn|sid": [chemins] }). Alimente
// la palette quick-open (Ctrl+P) : fichiers vus récemment en tête quand la
// recherche est vide. La clé inclut la connexion : deux démons peuvent
// réutiliser le même id de session pour des workspaces différents.
// ---------------------------------------------------------------------------

const STORE_KEY = "ac-recent-files";
/** Entrées conservées par session. */
const MAX_PER_SESSION = 10;
/** Cap sur le nombre de sessions dans le store — sinon il grossirait
 *  sans borne au fil des sessions créées/supprimées. */
const MAX_SESSIONS = 25;

type Store = Record<string, string[]>;

function read(): Store {
  try {
    const v = JSON.parse(localStorage.getItem(STORE_KEY) ?? "{}");
    return v && typeof v === "object" ? v : {};
  } catch {
    return {};
  }
}

/** Clé de bucket : conn + id de session (le cwd n'est pas dans la clé —
 *  une session garde son workspace, et les recents doivent la suivre). */
export function recentsId(sid: string, conn?: string): string {
  return `${conn ?? "default"}|${sid}`;
}

/** Les derniers chemins ouverts de la session (plus récent en tête). */
export function recentFiles(id: string): string[] {
  return read()[id] ?? [];
}

/** Enregistre une ouverture : le chemin passe en tête (dédoublonné),
 *  la pile est bornée à MAX_PER_SESSION. Chemins relatifs ou absolus,
 *  stockés tels qu'ouverts. */
export function pushRecentFile(id: string, path: string) {
  const s = read();
  s[id] = [path, ...(s[id] ?? []).filter((p) => p !== path)].slice(0, MAX_PER_SESSION);
  // Purge des sessions les plus anciennes au-delà du cap (l'ordre des
  // clés JSON suit l'insertion — les plus vieilles partent d'abord).
  const keys = Object.keys(s);
  if (keys.length > MAX_SESSIONS) {
    for (const k of keys.slice(0, keys.length - MAX_SESSIONS)) delete s[k];
  }
  try {
    localStorage.setItem(STORE_KEY, JSON.stringify(s));
  } catch {
    /* quota plein — les recents sont un confort, pas une donnée */
  }
}
