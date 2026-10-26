// ---------------------------------------------------------------------------
// Helpers DOM génériques
// ---------------------------------------------------------------------------

export const $ = <T extends HTMLElement>(id: string) =>
  document.getElementById(id) as T;

export function esc(s: string): string {
  const d = document.createElement("div");
  d.textContent = s;
  return d.innerHTML;
}

// Pour les valeurs insérées dans un attribut "…" : esc() n'échappe pas '"'.
export const q = (s: string) => s.replace(/"/g, "&quot;");

/** Erreur visible et copiable (remplace alert). */
export function showErr(msg: string) {
  const bar = $("err-bar");
  $("err-text").textContent = msg;
  bar.hidden = false;
}
$("err-close").addEventListener("click", () => ($("err-bar").hidden = true));

export function mkEv(cls: string, seq: number): HTMLElement {
  const d = document.createElement("div");
  d.className = `ev ${cls}`;
  d.dataset.seq = String(seq);
  return d;
}

export function qBtn(txt: string, title: string): HTMLButtonElement {
  const b = document.createElement("button");
  b.textContent = txt;
  b.title = title;
  return b;
}

export async function copyText(s: string) {
  try {
    await navigator.clipboard.writeText(s);
  } catch {
    const ta = document.createElement("textarea");
    ta.value = s;
    document.body.appendChild(ta);
    ta.select();
    document.execCommand("copy");
    ta.remove();
  }
}
