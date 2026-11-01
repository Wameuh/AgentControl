// ---------------------------------------------------------------------------
// Appairage par QR code — protocole B′ (clé jetable + host key épinglée),
// repli A (credential permanent, opt-in averti) et mode dégradé H
// (endpoint seul, clé posée à la main).
//
// Émetteur (desktop) : endpoint éditable résolu par pair_resolve →
// pair_begin installe côté serveur la ligne authorized_keys forcée
// (pair.sh <nonce> <rport>, clé K à 5 min, one-shot) → QR v2.1 rendu
// dans dlg-qr avec compte à rebours + états (waiting/paired/expired) ;
// pair_poll suit l'installation, pair_cleanup retire toujours la ligne K.
// Récepteur (appareil) : getUserMedia + jsQR → parsing strict
// (pair-payload.ts, module pur) → confirmation user@host + empreinte →
// pair_attempt (russh + pinning hk) exige pair-ok:<fp> == notre clé →
// cfg sauvegardée avec host_keys persistées (toutes les connexions
// natives suivantes restent épinglées).
// Aucun secret (K, PEM, payload) n'est loggé ni persisté côté client.
// ---------------------------------------------------------------------------

import { toCanvas } from "qrcode";
import jsQR from "jsqr";
import { invoke } from "../core/bridge";
import { $, esc } from "../core/dom";
import { accentOf, isAndroid } from "../core/chrome";
import { t } from "../i18n";
import { parsePair } from "./pair-payload";
import type { SavedConn } from "../core/types";

/// QR v1 (credential complet) : auto-fermeture — un passe-partout ne
/// reste pas affiché indéfiniment.
const QR_CRED_TTL_S = 120;

// ---------------------------------------------------------------------------
// Émission : bouton « Appairer » d'une connexion SSH sauvegardée.
// ---------------------------------------------------------------------------

interface PairSession {
  /** cfg du profil (transport serveur pour poll/cleanup). */
  cfg: SavedConn;
  /** nonce courant — la ligne K doit mourir à la fermeture. */
  nonce: string | null;
  /** compte à rebours affiché (borne exp serveur). */
  left: number;
  mode: "setup" | "v2" | "creds" | "endpoint";
}

let session: PairSession | null = null;
let qrTimer: ReturnType<typeof setInterval> | undefined;
let qrPoll: ReturnType<typeof setInterval> | undefined;
let qrClose: ReturnType<typeof setTimeout> | undefined;
let devicesBound = false;

function stopTimers() {
  clearInterval(qrTimer);
  clearInterval(qrPoll);
  clearTimeout(qrClose);
  qrTimer = qrPoll = qrClose = undefined;
}

function qrStatus(msg: string, err = false) {
  const el = $("qr-status");
  el.textContent = msg;
  el.classList.toggle("err", err);
  el.hidden = !msg;
}

/** Rend `payload` sur le canvas du dialogue (EC L = capacité max). */
async function drawQr(payload: string) {
  const canvas = $("qr-canvas") as HTMLCanvasElement;
  await toCanvas(canvas, payload, {
    errorCorrectionLevel: "L",
    margin: 1,
    width: 300,
  });
  canvas.hidden = false;
}

/** Compte à rebours de l'expiration (borne serveur : `exp` epoch ; la
 *  vérité reste pair_poll → "expired", l'affichage est indicatif). */
function startCountdown(s: PairSession) {
  const tick = () => {
    $("qr-ttl").textContent = s.left > 0 ? t("qr_ttl", { s: s.left }) : "";
  };
  tick();
  clearInterval(qrTimer);
  qrTimer = setInterval(() => {
    if (--s.left >= 0) tick();
    if (s.left <= 0) clearInterval(qrTimer);
  }, 1000);
}

/** pair_poll → waiting/paired:<label>/expired/gone — état affiché ;
 *  paired et expired stoppent la boucle (la ligne K est déjà morte). */
function startPolling(s: PairSession) {
  clearInterval(qrPoll);
  qrPoll = setInterval(() => {
    const nonce = s.nonce;
    if (nonce == null) return;
    invoke<string>("pair_poll", { cfg: s.cfg, nonce })
      .then((st) => {
        if (st.startsWith("paired:")) {
          qrStatus(t("qr_paired", { label: st.slice(7) || "?" }));
          stopTimers();
        } else if (st === "expired" || st === "gone") {
          qrStatus(t(st === "expired" ? "qr_expired" : "qr_gone"), true);
          stopTimers();
          ($("qr-canvas") as HTMLCanvasElement).hidden = true;
        }
      })
      .catch(() => {}); // coupure ssh transitoire : le poll reprend
  }, 2000);
}

// ---------------------------------------------------------------------------
// Accès managés (authorized_keys) — surface partagée entre la modale QR,
// l'éditeur de connexion et la section Connections des paramètres.
// pair_devices relit le fichier serveur : pas d'état dupliqué côté client
// (authorized_keys reste la source de vérité, révocable hors app).
// ---------------------------------------------------------------------------

/** Accès managé lu côté serveur : appareil appairé (dev), clé jetable K
 *  survivante (tmp — exp = epoch serveur ou "-"), clé de l'app (app). */
export interface PairAccess {
  kind: "dev" | "tmp" | "app";
  fp: string;
  label?: string;
  ts?: string;
  nonce?: string;
  exp?: string;
}

/** Cibles d'une liste d'accès : `status` affiche « aucun » /
 *  « indisponible » (échec ssh — silencieux, jamais de showErr brut). */
export interface AccessEls {
  ul: HTMLElement;
  status?: HTMLElement | null;
}

let ownFpCache: string | null | undefined;

/** fp SHA256 de la clé de l'app — recalculé depuis la ligne
 *  authorized_keys exposée par ssh_pubkey (blob → sha256 → base64
 *  OpenSSH). Permet de marquer « cette app » dans les entrées `app`. */
async function ownKeyFp(): Promise<string | null> {
  if (ownFpCache !== undefined) return ownFpCache;
  try {
    const line = await invoke<string>("ssh_pubkey");
    const b64 = line.trim().split(/\s+/)[1];
    if (!b64) return (ownFpCache = null);
    const raw = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
    const d = new Uint8Array(await crypto.subtle.digest("SHA-256", raw));
    let s = "";
    for (const b of d) s += String.fromCharCode(b);
    ownFpCache = `SHA256:${btoa(s).replace(/=+$/, "")}`;
  } catch {
    ownFpCache = null;
  }
  return ownFpCache;
}

/** Liste + révocation des accès managés — générique : toute surface qui
 *  fournit un <ul> (+ ligne d'état optionnelle) l'utilise ; le rechargement
 *  après revoke et à l'ouverture est inclus. `onErr` reçoit les erreurs de
 *  révocation (la modale QR les affiche dans son bandeau ; ailleurs, la
 *  ligne d'état fait foi). */
export async function renderAccesses(
  cfg: SavedConn,
  els: AccessEls,
  onErr?: (msg: string) => void,
): Promise<void> {
  const { ul, status } = els;
  ul.innerHTML = "";
  const setStatus = (msg: string | null) => {
    if (!status) return;
    if (msg == null) {
      status.hidden = true;
      return;
    }
    status.textContent = msg;
    status.hidden = false;
  };
  const report = onErr ?? ((m: string) => setStatus(m));
  const [devs, ownFp] = await Promise.all([
    invoke<PairAccess[]>("pair_devices", { cfg }).catch(() => null),
    ownKeyFp(),
  ]);
  if (devs == null) {
    setStatus(t("pair_acc_unavail"));
    return;
  }
  if (devs.length === 0) {
    setStatus(t("pair_acc_none"));
    return;
  }
  setStatus(null);
  const now = Math.floor(Date.now() / 1000);
  for (const d of devs) {
    const li = document.createElement("li");
    const fpEl =
      `<span class="qr-dev-fp" title="${esc(d.fp)}">${esc(d.fp.slice(0, 20))}…</span>`;
    if (d.kind === "tmp") {
      // Clé K survivante : exp encore future = appairage en cours ailleurs,
      // sinon orpheline — les deux se révoquent pareil.
      const alive = Number(d.exp) > now;
      li.innerHTML = fpEl +
        `<span class="qr-dev-label">${esc(t("pair_acc_tmp"))}</span>` +
        `<span class="qr-dev-badge">${esc(t(alive ? "pair_acc_live" : "pair_acc_expired"))}</span>`;
    } else if (d.kind === "app") {
      const own = ownFp != null && d.fp === ownFp;
      li.innerHTML = fpEl +
        `<span class="qr-dev-label">${esc(t("pair_acc_app"))}</span>` +
        (own ? `<span class="qr-dev-badge">${esc(t("pair_acc_self"))}</span>` : "");
    } else {
      li.innerHTML = fpEl +
        `<span class="qr-dev-label">${esc(d.label ?? "?")}</span>` +
        `<span class="qr-dev-ts">${esc(d.ts ?? "")}</span>`;
    }
    li.insertAdjacentHTML("beforeend",
      `<button type="button" class="qr-dev-rev danger">${esc(t("qr_revoke"))}</button>`);
    const btn = li.querySelector(".qr-dev-rev") as HTMLButtonElement;
    // Révocation en 2 taps (arme/désarme — motif du bouton supprimer).
    btn.onclick = () => {
      if (!btn.classList.contains("armed")) {
        btn.classList.add("armed");
        btn.textContent = t("qr_revoke_sure");
        setTimeout(() => {
          btn.classList.remove("armed");
          btn.textContent = t("qr_revoke");
        }, 2500);
        return;
      }
      btn.disabled = true;
      invoke("pair_revoke", { cfg, fp: d.fp })
        .then(() => void renderAccesses(cfg, els, onErr))
        .catch((e) => {
          btn.disabled = false;
          report(String(e));
        });
    };
    ul.appendChild(li);
  }
}

/** pair_begin → QR v2.1 + polling ; l'endpoint édité est celui mis dans
 *  le payload (l'install elle-même passe par la cfg du profil). */
/** Champ port borné — un nombre hors plage serait rejeté côté serde
 *  avec un message peu clair ; on clamp ici (regex host géré par Rust). */
function epPort(): number {
  const n = Number(($("qr-port") as HTMLInputElement).value);
  return Math.min(65535, Math.max(1, Number.isFinite(n) && n > 0 ? Math.floor(n) : 22));
}

async function beginPair(s: PairSession) {
  const host = ($("qr-host") as HTMLInputElement).value.trim();
  const port = epPort();
  const user = ($("qr-user") as HTMLInputElement).value.trim();
  qrStatus(t("qr_installing"));
  ($("qr-gen") as HTMLButtonElement).disabled = true;
  // Régénération : la ligne K de la session précédente doit mourir
  // AVANT d'en poser une nouvelle (une seule clé jetable à la fois).
  if (s.nonce != null) await killActiveNonce(s);
  try {
    const r = await invoke<{
      payload: string; exp: number; nonce: string;
      warn: string | null; sweep: string;
    }>("pair_begin", { cfg: s.cfg, host, port, user });
    // K installée : plus de retour — le canvas montre le QR, le poll
    // suit l'état, le cleanup tuera la ligne si le dialogue se ferme.
    s.nonce = r.nonce;
    s.mode = "v2";
    s.left = Math.max(0, r.exp - Math.floor(Date.now() / 1000));
    await drawQr(r.payload);
    const warnEl = $("qr-warn");
    // warn: fps ≠ known_hosts (dur) ; sweep inline-only = pas de cron
    // serveur → une ligne K expirée non consommée n'est balayée qu'au
    // prochain appairage (hint — l'expiry-time sshd borne déjà la clé).
    const sweepHint = r.sweep === "inline-only" ? t("qr_sweep_hint") : "";
    if (r.warn || sweepHint) {
      warnEl.textContent = [r.warn, sweepHint].filter(Boolean).join(" ");
      warnEl.hidden = false;
    } else {
      warnEl.hidden = true;
    }
    qrStatus(t("qr_waiting"));
    startCountdown(s);
    startPolling(s);
  } catch (e) {
    qrStatus(String(e), true);
  } finally {
    ($("qr-gen") as HTMLButtonElement).disabled = false;
  }
}

/** Kill la ligne K d'une session v2 en cours (changement de mode) —
 *  factorisé : credQr, endpointQr, régénération et close s'en servent. */
async function killActiveNonce(s: PairSession) {
  if (s.nonce == null) return;
  const prev = s.nonce;
  s.nonce = null;
  stopTimers();
  await invoke("pair_cleanup", { cfg: s.cfg, nonce: prev }).catch(() => {});
}

/** Repli A : URI v1 « profil complet » — opt-in (avertissement affiché
 *  en permanence) puis QR autodestructif à TTL court. */
async function credQr(s: PairSession) {
  await killActiveNonce(s);
  const uri = await invoke<string>("pair_payload", { cfg: s.cfg });
  s.mode = "creds";
  s.nonce = null;
  s.left = QR_CRED_TTL_S;
  const warnEl = $("qr-warn");
  warnEl.textContent = t("qr_creds_warn");
  warnEl.hidden = false;
  await drawQr(uri);
  qrStatus(t("qr_creds_live"), true);
  startCountdown(s);
  // TTL dur → fermeture forcée (le code EST le credential).
  clearTimeout(qrClose);
  qrClose = setTimeout(() => {
    const dlg = $("dlg-qr") as HTMLDialogElement;
    if (dlg.open) dlg.close();
  }, QR_CRED_TTL_S * 1000 + 500);
}

/** Mode dégradé H : endpoint seul — pas de secret, pas d'expiration ;
 *  l'utilisateur pose la pubkey de l'appareil à la main. */
async function endpointQr(s: PairSession) {
  const host = ($("qr-host") as HTMLInputElement).value.trim();
  const user = ($("qr-user") as HTMLInputElement).value.trim();
  const rport = s.cfg.remote_port ?? 7331;
  s.mode = "endpoint";
  await killActiveNonce(s);
  const port = epPort();
  await drawQr(JSON.stringify({ v: 2, host, port, user, rport }));
  $("qr-ttl").textContent = "";
  $("qr-warn").hidden = true;
  qrStatus(t("qr_endpoint_live"));
}

/** Point d'entrée émission — dlg-qr s'ouvre sur l'endpoint éditable
 *  (résolu par pair_resolve : alias ssh -G → hôte réel pour le QR). */
export async function shareQr(c: SavedConn) {
  const dlg = $("dlg-qr") as HTMLDialogElement;
  if (dlg.open) dlg.close();
  session = { cfg: c, nonce: null, left: 0, mode: "setup" };
  ($("qr-canvas") as HTMLCanvasElement).hidden = true;
  $("qr-ttl").textContent = "";
  $("qr-warn").hidden = true;
  qrStatus("");
  ($("qr-gen") as HTMLButtonElement).disabled = false;
  // Endpoint pré-rempli (erreur non fatale : l'utilisateur saisit).
  const ep = await invoke<{ host: string; port: number; user: string }>(
    "pair_resolve", { cfg: c }).catch(() => ({
      host: c.host ?? "", port: c.port ?? 22, user: c.user ?? "",
    }));
  ($("qr-host") as HTMLInputElement).value = ep.host;
  ($("qr-port") as HTMLInputElement).value = String(ep.port);
  ($("qr-user") as HTMLInputElement).value = ep.user;
  void renderAccesses(c, {
    ul: $("qr-dev-list"),
    status: $("qr-dev-empty"),
  }, (e) => qrStatus(e, true)).catch(() => {});
  dlg.showModal();
}

/** Fermeture : timers + poll coupés, canvas effacé (le bitmap porte le
 *  secret), ligne K retirée côté serveur si une session était posée. */
export function initPairing() {
  const dlg = $("dlg-qr") as HTMLDialogElement;
  dlg.addEventListener("close", () => {
    stopTimers();
    const s = session;
    session = null;
    const cv = $("qr-canvas") as HTMLCanvasElement;
    cv.getContext("2d")?.clearRect(0, 0, cv.width, cv.height);
    cv.hidden = true;
    if (s?.nonce != null) {
      invoke("pair_cleanup", { cfg: s.cfg, nonce: s.nonce }).catch(() => {});
    }
  });
  if (devicesBound) return;
  devicesBound = true;
  ($("qr-gen") as HTMLButtonElement).onclick = () => {
    if (session) void beginPair(session);
  };
  ($("qr-alt-creds") as HTMLButtonElement).onclick = () => {
    if (session) void credQr(session).catch((e) => qrStatus(String(e), true));
  };
  ($("qr-alt-ep") as HTMLButtonElement).onclick = () => {
    if (session) void endpointQr(session).catch((e) => qrStatus(String(e), true));
  };
}

// ---------------------------------------------------------------------------
// Réception : scan caméra → payload validé → confirmation → cfg prête
// pour connect()/save_connection(). null = annulé ; throw = erreur à
// afficher par l'appelant (caméra, payload invalide, appairage refusé).
// ---------------------------------------------------------------------------

/** Confirmation générique — titre/message/bouton paramétrés (réutilisée
 *  par les trois kinds ET la re-confirmation de host key modifiée). */
export function confirmPair(h3: string, msg: string, go: string): Promise<boolean> {
  const dlg = $("dlg-pair") as HTMLDialogElement;
  if (dlg.open) return Promise.resolve(false);
  dlg.querySelector("h3")!.textContent = h3;
  $("pair-msg").textContent = msg;
  ($("pair-go") as HTMLButtonElement).textContent = go;
  return new Promise((resolve) => {
    ($("pair-go") as HTMLButtonElement).onclick = () => {
      resolve(true);
      dlg.close();
    };
    dlg.addEventListener("close", () => resolve(false), { once: true });
    dlg.showModal();
  });
}

/** Boucle de décodage : frame vidéo → canvas offscreen → jsQR. */
async function scanLoop(video: HTMLVideoElement): Promise<string | null> {
  const canvas = document.createElement("canvas");
  const ctx = canvas.getContext("2d", { willReadFrequently: true })!;
  const dlg = $("dlg-scan") as HTMLDialogElement;
  while (dlg.open) {
    if (video.readyState >= video.HAVE_CURRENT_DATA && video.videoWidth > 0) {
      canvas.width = video.videoWidth;
      canvas.height = video.videoHeight;
      ctx.drawImage(video, 0, 0);
      const img = ctx.getImageData(0, 0, canvas.width, canvas.height);
      const qr = jsQR(img.data, img.width, img.height, {
        inversionAttempts: "attemptBoth",
      });
      if (qr?.data) return qr.data;
    }
    await new Promise((r) => requestAnimationFrame(r));
  }
  return null;
}

/** Label de l'appareil dans authorized_keys — la plateforme identifie
 *  la machine physique (l'utilisateur peut la renommer après coup). */
function deviceLabel(): string {
  if (isAndroid()) return "ac-android";
  const ua = navigator.userAgent;
  if (/iPhone|iPad/.test(ua)) return "ac-ios";
  if (/Mac OS X/.test(ua)) return "ac-mac";
  if (/Windows/.test(ua)) return "ac-win";
  return "ac-linux";
}

export async function scanPair(): Promise<Record<string, unknown> | null> {
  const dlg = $("dlg-scan") as HTMLDialogElement;
  const video = $("scan-video") as HTMLVideoElement;
  const media = navigator.mediaDevices;
  if (!media?.getUserMedia) throw new Error(t("qr_cam_none"));
  const stream = await media.getUserMedia({
    video: { facingMode: "environment" },
    audio: false,
  });
  let text: string | null = null;
  try {
    video.srcObject = stream;
    dlg.showModal();
    await video.play().catch(() => {});
    text = await scanLoop(video);
  } finally {
    dlg.close();
    stream.getTracks().forEach((tr) => tr.stop());
    video.srcObject = null;
  }
  if (text == null) return null; // dialog fermé sans code
  const scan = parsePair(text);
  if (!scan) throw new Error(t("qr_bad"));

  const where = (u: string, h: string, p: number) =>
    `${u ? u + "@" : ""}${h}:${p}`;

  if (scan.kind === "v2") {
    const p = scan.p;
    const fp = p.hk![0];
    const ok = await confirmPair(
      t("pair_h"),
      t("pair_msg_v2", {
        where: where(p.user, p.host, p.port),
        fp: `${fp.slice(0, 20)}…${fp.slice(-8)}`,
      }),
      t("pair_go_v2"),
    );
    if (!ok) return null;
    // K jetable + pinning hk : pair-ok:<fp> vérifié côté Rust —
    // l'empreinte rendue est celle de NOTRE clé app, preuve réelle.
    await invoke<string>("pair_attempt", {
      host: p.host,
      port: p.port,
      user: p.user,
      kPem: p.k,
      hk: p.hk,
      label: deviceLabel(),
    });
    return {
      kind: "ssh",
      host: p.host,
      port: p.port,
      remote_port: p.rport,
      user: p.user || undefined,
      native: true,
      host_keys: p.hk,
    };
  }
  if (scan.kind === "endpoint") {
    const p = scan.p;
    const ok = await confirmPair(
      t("pair_h"),
      t("pair_msg_ep", { where: where(p.user, p.host, p.port) }),
      t("pair_go_ep"),
    );
    if (!ok) return null;
    return {
      kind: "ssh",
      host: p.host,
      port: p.port,
      remote_port: p.rport,
      user: p.user || undefined,
      native: true,
    };
  }
  // Repli A — credential complet du profil.
  const p = scan.p;
  const ok = await confirmPair(
    t("pair_h"),
    t("pair_msg", {
      where: where(p.user, p.host, p.port),
      auth: t(p.auth === "key" ? "pair_auth_key" : "pair_auth_pw"),
    }),
    t("pair_go"),
  );
  if (!ok) return null;
  const cfg: Record<string, unknown> = {
    kind: "ssh",
    host: p.host,
    port: p.port,
    remote_port: p.rport,
    user: p.user || undefined,
    native: true,
  };
  if (p.color && accentOf(p.color)) cfg.color = p.color;
  if (p.auth === "key") {
    cfg.key = await invoke<string>("pair_import_key", {
      pem: p.secret,
      name: `${p.host}_${p.port}`,
    });
    if (p.pw != null) cfg.password = p.pw;
  } else {
    cfg.password = p.secret;
  }
  return cfg;
}
