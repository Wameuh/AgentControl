// ---------------------------------------------------------------------------
// Tampon circulaire des logs côté web : hors Tauri il n'y a pas de
// client.log (client_core::log n'existe pas — le frontend tourne dans le
// navigateur). Les lignes `log_write` (logUi) y sont versées pour que le
// panneau logs live et les visionneuses .log-details aient un flux à
// suivre. Offset = cumul d'octets du flux logique (fin de ligne
// comprise) — même contrat que client_core::log_read.
// ---------------------------------------------------------------------------

const MAX_LINES = 4000;

interface Entry {
  /** Cumul d'octets en fin de ligne (line + '\n'). */
  end: number;
  text: string;
}

const buf: Entry[] = [];
let total = 0;

function push(level: string, msg: string) {
  const iso = new Date().toISOString();
  const text = `[${iso}] [${level.toUpperCase()}] ui: ${msg}`;
  total += text.length + 1;
  buf.push({ end: total, text });
  while (buf.length > MAX_LINES) buf.shift();
}

/** Lignes complètes après `offset` (null = dernière page), budget
 *  `limit` octets — forme {offset, data, size} du contrat log_read. */
function read(offset: number | null, limit: number): {
  offset: number;
  data: string;
  size: number;
} {
  const cap = Math.max(4096, Math.min(limit || 64 * 1024, 256 * 1024));
  if (offset == null || offset > total || buf.length === 0) {
    // Dernière page : les entrées récentes dans le budget, sans couper
    // une ligne (elles le sont déjà — une entrée = une ligne).
    let bytes = 0;
    let i = buf.length;
    while (i > 0 && bytes + buf[i - 1].text.length + 1 <= cap) {
      bytes += buf[i - 1].text.length + 1;
      i--;
    }
    const sel = buf.slice(i);
    return {
      offset: total,
      data: sel.length ? sel.map((e) => e.text).join("\n") + "\n" : "",
      size: total,
    };
  }
  // Incrémental : même budget `cap` — les entrées ajoutées depuis
  // `offset`, les plus récentes si le lot déborde (les plus anciennes
  // sautées restent derrière l'offset renvoyé → pas de trou logique :
  // offset renvoyé = total, donc les sautées ne seront jamais relues —
  // on préfère tout émettre borné).
  const sel = buf.filter((e) => e.end > offset);
  let bytes = 0;
  let i = sel.length;
  while (i > 0 && bytes + sel[i - 1].text.length + 1 <= cap) {
    bytes += sel[i - 1].text.length + 1;
    i--;
  }
  const kept = sel.slice(i);
  return {
    offset: total,
    data: kept.length ? kept.map((e) => e.text).join("\n") + "\n" : "",
    size: total,
  };
}

/** Les N dernières lignes (contrat log_tail). */
function tail(n: number): string {
  return buf.slice(-n).map((e) => e.text).join("\n");
}

export const clientLog = { push, read, tail };
