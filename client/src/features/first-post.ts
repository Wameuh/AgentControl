// ---------------------------------------------------------------------------
// « Premier post » : bouton ⤒ du header conversation — ouvre une boîte avec
// le premier user_message du cycle courant. Le journal est vidé à /clear,
// /new et au rebind côté démon → le post servi est toujours celui du cycle
// en cours, jamais un reste du cycle précédent.
// ---------------------------------------------------------------------------

import { $, copyText, showErr } from "../core/dom";
import { call } from "../core/rpc";
import { app } from "../core/state";
import { t } from "../i18n";

interface FirstPost {
  seq: number;
  ts: string;
  text: string;
  steer: boolean;
}

/** Câble le bouton ⤒ + le dialogue. `capable` = démon sait répondre à
 *  first_post (capability daemon_status) — sinon bouton masqué. */
export function initFirstPost(capable: (conn: string) => boolean) {
  const dlg = $("dlg-first") as HTMLDialogElement;
  const meta = $("first-meta");
  const body = $("first-body");
  const copyBtn = $("first-copy") as HTMLButtonElement;
  let current = "";

  $("btn-first").onclick = async () => {
    const s = app.current;
    if (!s) return;
    if (!capable(s.conn ?? "default")) {
      showErr(t("first_unsupported"));
      return;
    }
    current = "";
    meta.textContent = "";
    body.textContent = t("loading");
    copyBtn.disabled = true;
    dlg.showModal();
    try {
      const r = await call<{ post: FirstPost | null }>("first_post", {
        session: s.id,
      });
      if (!r.post) {
        meta.textContent = "";
        body.textContent = t("first_none");
        return;
      }
      const p = r.post;
      current = p.text;
      meta.textContent = `#${p.seq} · ${new Date(p.ts).toLocaleString()}`;
      body.textContent = p.text;
      copyBtn.disabled = false;
    } catch (e) {
      body.textContent = String(e);
    }
  };

  copyBtn.onclick = async () => {
    if (!current) return;
    await copyText(current);
    copyBtn.textContent = t("copied");
    setTimeout(() => (copyBtn.textContent = t("copy")), 1200);
  };
}
