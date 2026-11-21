// ---------------------------------------------------------------------------
// Composer : champ de saisie, envois (normal/steer/force), autocomplétion
// des slash-commandes, commandes builtin client, file d'attente des
// prompts, picker de sessions natives (reprise).
// ---------------------------------------------------------------------------

import { $, esc, notify, showErr } from "../core/dom";
import { icon, qIco } from "../ui/icons";
import { nav } from "../ui/nav";
import { call, callBg, isConnLost } from "../core/rpc";
import { app } from "../core/state";
import { isAndroid } from "../core/chrome";
import { engineCaps, debugNoFs } from "../core/engine";
import type { CmdInfo, SendMode } from "../core/types";
import { t } from "../i18n";
import { cmdDesc, cmdHint, cmdName } from "./modelbar";
import { sendPref } from "./settings";
import { Attachments } from "./attachments";
import { SttMic } from "./stt-mic";
import { syncSendMic } from "../mobile/composer-mobile";
import {
  probeFitInput,
  probeInputHandler,
  probeInputPaint,
  telemetryActive,
} from "./telemetry";
import type { ChatView } from "./chat-view";

// ---- Undo/redo du composer : pile custom d'états ----
// Chaque `.value =` programmatique vide la pile d'undo native du
// textarea — le composer garde donc sa propre pile {value, selection} :
// un pas par insertion applicative (setInput), un pas par rafale de
// frappe (debounce), un pas par collage/dépôt. Ctrl+Z / Ctrl+Shift+Z /
// Ctrl+Y sont interceptés dans le keydown ; jamais pendant une
// composition IME (l'autocorrect/clavier virtuel gèrent leurs undo).
// La pile est propre à la session affichée : sessionChanged la vide.

interface UndoState {
  v: string;
  s: number;
  e: number;
}

/// Frappe : deux edits espacés de plus que ce délai ouvrent un pas
/// d'undo distinct (sinon toute la saisie = un seul Ctrl+Z).
const UNDO_DEBOUNCE_MS = 800;
/// Borne de la pile (les très longs brouillons ne sont pas gardés à
/// l'infini — 100 pas × un prompt reste raisonnable en mémoire).
const UNDO_MAX = 100;

/// `field-sizing: content` laisse le navigateur dimensionner #input
/// tout seul (max-height borne) — plus de mesure JS (scrollHeight =
/// reflow forcé) à chaque frappe. Repli : miroir #input-ghost en flux
/// (fitInputGhost — aucune lecture de layout dans le chemin chaud).
/// `ac-nofs` en localStorage force le repli pour test A/B.
const FIELD_SIZING = engineCaps.fieldSizing && !debugNoFs();

// ---- Commandes built-in (toujours dispo, exécutées côté client) ----
const BUILTINS = (): { name: string; description: string; hint: string }[] => [
  { name: "model", description: t("builtin_model"), hint: "<model>" },
  { name: "resume", description: t("builtin_resume"), hint: "" },
  { name: "cancel", description: t("builtin_cancel"), hint: "" },
  { name: "fork", description: t("builtin_fork"), hint: "" },
  { name: "clear", description: t("builtin_clear"), hint: "" },
  { name: "new", description: t("builtin_new"), hint: "" },
  { name: "sleep", description: t("builtin_sleep"), hint: "" },
];

export class Composer {
  private chat!: ChatView;
  /** Pièces jointes : chips, uploads (.ac-uploads), chemins pour
   *  l'agent — module dédié (picker « + », drop, collage). */
  private attach = new Attachments();
  /** Rafraîchit la sidebar (injecté). */
  refresh: () => Promise<void> = async () => {};
  private acSel = 0;
  /** Champ déplié manuellement (bouton expand) — replie dès que le
      contenu rentre sous le cap normal. */
  private inpExpanded = false;
  /** Brouillons par session (id → texte non envoyé). La map ne contient
   *  que les sessions non affichées : le texte de la session courante
   *  vit dans #input et n'y revient qu'à la bascule suivante. Volatil —
   *  pas de persistance disque. */
  private drafts = new Map<string, string>();
  /** Session propriétaire du texte actuellement dans #input. */
  private draftSid: string | null = null;
  /** Dictée vocale on-device : bouton mic + injection live des partials
   *  dans #input (Android only, no-op ailleurs). */
  readonly mic = new SttMic();

  bind(chat: ChatView) {
    this.chat = chat;
  }

  // ---- Hauteur du champ : auto-grow borné + bouton expand ----
  // Le champ grandit avec le contenu jusqu'à ~1/3 de la hauteur du fil ;
  // au-delà il reste borné et scrollable, et le bouton « N ⌃ » permet de
  // l'étendre à ~2/3 (re-clic = repli). Le bouton disparaît dès que le
  // texte redevient court (ou le champ est vidé/envoyé).

  /** rAF de coalescence fitInput/refreshAc : la frappe produit un event
   *  input par touche — mesure (reflow forcé) et rebuild dropdown au
   *  plus une fois par frame, jamais dans le handler de la keystroke. */
  private uiRaf = 0;

  /** Pile d'undo (états à restaurer) / redo. */
  private undoStack: UndoState[] = [];
  private redoStack: UndoState[] = [];
  /** État connu du champ — référence « avant » pour la frappe : l'event
   *  input arrive APRÈS la mutation, la valeur d'avant n'est plus
   *  lisible que via ce snapshot continu. */
  private lastKnown: UndoState = { v: "", s: 0, e: 0 };
  private lastEditAt = 0;
  /** Groupe d'écritures coalescées (dictée STT) : tant que le groupe du
   *  dernier setInput est identique, pas de nouveau pas d'undo — une
   *  dictée entière = UN Ctrl+Z, pas un par partial. */
  private lastGroup: string | null = null;

  private scheduleUi() {
    if (this.uiRaf) return;
    this.uiRaf = requestAnimationFrame(() => {
      this.uiRaf = 0;
      this.refreshAc();
      this.fitInput();
    });
  }

  /** Écriture programmatique dans #input. `undoGroup` coalesce les
   *  écritures successives d'une même source (dictée) en un seul pas
   *  d'undo ; `skipUndo` pour les restaurations qui ne doivent pas être
   *  annulables (brouillon d'une autre session). */
  private setInput(
    v: string,
    opts?: { undoGroup?: string; skipUndo?: boolean },
  ) {
    const ta = $("input") as HTMLTextAreaElement;
    if (!opts?.skipUndo && ta.value !== v) this.pushUndo(opts?.undoGroup);
    ta.value = v;
    this.lastKnown = this.snapshot();
    // La dropdown suit la valeur même quand elle change sans event input
    // (envoi, applyAc, reprise de file) — sinon elle resterait affichée.
    // Différé à la frame : un programmatic set n'a pas besoin d'un reflow
    // synchrone (et la dictée enchaîne les setInput par partial).
    this.acStale = true;
    this.scheduleUi();
  }

  // ---- Undo/redo ----

  private snapshot(): UndoState {
    const ta = $("input") as HTMLTextAreaElement;
    const s = ta.selectionStart ?? ta.value.length;
    return { v: ta.value, s, e: ta.selectionEnd ?? s };
  }

  /** Empile un état comme pas d'undo (sommet identique = no-op ignoré)
   *  et vide la pile de redo — toute nouvelle édition invalide le futur. */
  private pushState(s: UndoState) {
    const top = this.undoStack[this.undoStack.length - 1];
    if (top && top.v === s.v) return;
    if (this.undoStack.length >= UNDO_MAX) this.undoStack.shift();
    this.undoStack.push(s);
    this.redoStack = [];
  }

  /** Avant un setInput : empile l'état courant — SAUF si le groupe est
   *  celui de la dernière écriture (rafale coalescée : dictée). */
  private pushUndo(group?: string) {
    if (group && group === this.lastGroup) return;
    this.pushState(this.snapshot());
    this.lastGroup = group ?? null;
  }

  /** Frappe/collage natif (event input) : un pas par rafale via debounce ;
   *  collage et dépôt forment leur propre pas immédiat. La composition
   *  IME et les historyUndo/Redo natifs ne passent pas par la pile —
   *  on se contente de resynchroniser la référence. */
  private noteUserEdit(e: InputEvent) {
    const cur = this.snapshot();
    const it = e.inputType ?? "";
    if (
      e.isComposing ||
      it === "insertCompositionText" ||
      it === "historyUndo" ||
      it === "historyRedo"
    ) {
      this.lastKnown = cur;
      return;
    }
    // Insertions ET coupes/départs obtiennent leur propre pas : un cut
    // absorbé dans la rafalle rendrait le texte coupé inatteignable
    // (seul l'état d'avant la rafale est empilé, pas l'état pré-coupe).
    const ownStep =
      it === "insertFromPaste" ||
      it === "insertFromDrop" ||
      it === "insertFromYank" ||
      it === "insertReplacementText" ||
      it === "deleteByCut" ||
      it === "deleteByDrag" ||
      it === "deleteByDrop";
    if (ownStep || Date.now() - this.lastEditAt > UNDO_DEBOUNCE_MS) {
      // lastKnown = état d'avant CET edit (mis à jour en fin de handler).
      this.pushState(this.lastKnown);
    }
    this.lastEditAt = Date.now();
    this.lastKnown = cur;
    this.lastGroup = null;
  }

  private undo() {
    const s = this.undoStack.pop();
    if (!s) return;
    this.pushRedo(this.snapshot());
    this.restore(s);
  }

  private redo() {
    const s = this.redoStack.pop();
    if (!s) return;
    this.pushUndoRaw(this.snapshot());
    this.restore(s);
  }

  /** Symétrique de pushState pour la pile de redo (pas de clear redo —
   *  c'est la pile d'undo qui l'alimente). */
  private pushRedo(s: UndoState) {
    if (this.redoStack.length >= UNDO_MAX) this.redoStack.shift();
    this.redoStack.push(s);
  }

  /** Empile sur undo SANS vider redo — utilisé par redo() lui-même. */
  private pushUndoRaw(s: UndoState) {
    if (this.undoStack.length >= UNDO_MAX) this.undoStack.shift();
    this.undoStack.push(s);
  }

  /** Réapplique un état complet (valeur + sélection) — pas d'event
   *  input, pas de nouveau pas d'undo. */
  private restore(s: UndoState) {
    const ta = $("input") as HTMLTextAreaElement;
    ta.value = s.v;
    const len = s.v.length;
    ta.setSelectionRange(Math.min(s.s, len), Math.min(s.e, len));
    this.lastKnown = this.snapshot();
    this.lastEditAt = 0; // la frappe suivante ouvre une nouvelle rafale
    this.lastGroup = null;
    this.acStale = true;
    this.scheduleUi();
  }

  /** La pile est propre à la session affichée : au changement, les
   *  états d'une autre session ne doivent ni être restaurés ni recevoir
   *  le brouillon comme pas d'undo. */
  private clearHistory() {
    this.undoStack = [];
    this.redoStack = [];
    this.lastEditAt = 0;
    this.lastGroup = null;
  }

  /** Hauteur/largeur du fil (#chat) pilotant les caps et le wrapping
   *  du champ — suivies par ResizeObserver (pas de lecture de layout
   *  dans le chemin chaud). */
  private chatH = 0;
  private chatW = 0;
  /** fitInput saute la mesure tant que ni la valeur ni la géométrie
   *  (resize → fitStale) n'ont changé : les scheduleUi sans contenu
   *  nouveau (setInput identique, restore, IME) ne paient pas de reflow. */
  private lastFitVal: string | null = null;
  private fitStale = true;
  /** padding/line-height du champ — stables (pas de réglage de fonte
   *  dans le client), lus une fois pour le libellé « N lignes ». */
  private fitMetrics: { padV: number; lh: number } | null = null;
  /** Dernier rendu de la pill « N lignes » — innerHTML n'est réécrit
   *  que si le compte, l'état expandé ou le titre (langue) changent :
   *  la frappe dans un champ overflowing ne doit pas muter le DOM à
   *  chaque frame. */
  private lastBtnLines = -1;
  private lastBtnExp: boolean | null = null;

  // ---- Repli sans field-sizing : miroir #input-ghost ----
  // Le div pré-wrap (mêmes métriques texte que #input) est le SEUL
  // enfant en flux de #input-wrap.grow : sa hauteur intrinsèque
  // dimensionne le wrap, #input est posé en absolute inset:0. Le
  // navigateur fait la mise en page — aucune lecture forcée
  // (scrollHeight = reflow synchrone proportionnel au fil sur les
  // vieux webview sans content-visibility, cause du lag #294).
  private ghostEl: HTMLElement | null = null;
  private ghostVal: string | null = null;
  /** Sonde « overflow » coalescée : lecture de ghost.offsetHeight
   *  APRÈS le layout de la frame (setTimeout post-rAF) — pas de
   *  reflow forcé dans le chemin de frappe. */
  private overTimer: number | undefined;

  /** Cap « petit » (~1/3 du fil) — même formule qu'avant, référence
   *  #chat (hauteur fixée par le viewport, suivie par ResizeObserver). */
  private smallCap(): number {
    const chatH = this.chatH || window.innerHeight;
    return Math.max(100, Math.round(chatH * 0.33));
  }

  private bigCap(): number {
    const small = this.smallCap();
    const chatH = this.chatH || window.innerHeight;
    return Math.max(small + 80, Math.round(chatH * 0.66));
  }

  /** Cap actif : étendu ↔ replié (inpExpanded). */
  private fitCap(): number {
    return this.inpExpanded ? this.bigCap() : this.smallCap();
  }

  /** État « déborde » + pill « N lignes » — partagé par les deux
   *  chemins de mesure (field-sizing et miroir). `need` = hauteur
   *  contenu nécessaire (px). */
  private paintFit(over: boolean, need: number) {
    const wrap = $("input-wrap");
    const btn = $("btn-expand");
    const inp = $("input") as HTMLTextAreaElement;
    wrap.classList.toggle("over", over);
    btn.hidden = !over;
    if (!over) return;
    if (!this.fitMetrics) {
      const cs = getComputedStyle(inp);
      this.fitMetrics = {
        padV: parseFloat(cs.paddingTop) + parseFloat(cs.paddingBottom),
        lh: parseFloat(cs.lineHeight) || 21,
      };
    }
    const lines = Math.max(
      inp.value.split("\n").length,
      Math.round((need - this.fitMetrics.padV) / this.fitMetrics.lh),
    );
    const title = t(this.inpExpanded ? "input_collapse" : "input_expand");
    if (
      lines !== this.lastBtnLines ||
      this.inpExpanded !== this.lastBtnExp ||
      title !== btn.title
    ) {
      this.lastBtnLines = lines;
      this.lastBtnExp = this.inpExpanded;
      btn.innerHTML =
        icon(this.inpExpanded ? "chev-down" : "chev-up") +
        `<span class="n">${lines}</span>`;
      btn.title = title;
      btn.setAttribute("aria-label", title);
    }
  }

  /** fitInput du chemin miroir : synchro du texte du miroir (une
   *  écriture par frame via le rAF amont), maj du cap seulement si
   *  géométrie/état a changé, sonde overflow différée. */
  private fitInputGhost(force: boolean) {
    const wrap = $("input-wrap");
    const inp = $("input") as HTMLTextAreaElement;
    wrap.classList.add("grow");
    const v = inp.value;
    if (v !== this.ghostVal && this.ghostEl) {
      this.ghostVal = v;
      // \n terminal : une div pre-wrap ne matérialise pas la dernière
      // ligne vide — la ZWSP (largeur nulle) la force, comme le curseur
      // du textarea sur sa ligne vide.
      this.ghostEl.textContent = v.endsWith("\n") ? v + "\u200B" : v;
    }
    if (force || this.fitStale) {
      this.fitStale = false;
      // Cap sur le WRAP (le chemin field-sizing le pose sur #input).
      // Resync des métriques : un changement de fonte
      // (thème, zoom) ne déclenche pas toujours un fitStale.
      this.syncGhostStyle();
      wrap.style.setProperty("--fit-cap", this.fitCap() + "px");
    }
    this.scheduleOverCheck();
  }

  private scheduleOverCheck() {
    if (this.overTimer !== undefined) return;
    this.overTimer = window.setTimeout(() => {
      this.overTimer = undefined;
      this.updateOver();
    }, 0);
  }

  /** Sonde post-layout : ghost.offsetHeight = hauteur contenu non
   *  cappée (le miroir déborde du wrap cappé, sa propre hauteur reste
   *  complète). Lecture à layout propre → quasi-gratuite. */
  private updateOver() {
    const g = this.ghostEl;
    const wrap = $("input-wrap");
    if (!g || !wrap) return;
    const need = g.offsetHeight;
    const over = need > this.smallCap();
    if (!over && this.inpExpanded) {
      // Rétracté : le cap doit redescendre petit sans attendre un
      // fitInput forcé (il n'y a plus de mesure périodique ici).
      this.inpExpanded = false;
      wrap.style.setProperty("--fit-cap", this.fitCap() + "px");
    }
    this.paintFit(over, need);
  }

  /** Recopie les métriques texte du champ sur le miroir — le miroir
   *  doit wrapper à l'identique (font, chasse, césure). Le padding
   *  reste en CSS : les règles .over partagées #input/#input-ghost
   *  doivent rester synchrones (commentaire dans chat.css). */
  private syncGhostStyle() {
    const g = this.ghostEl;
    const inp = $("input") as HTMLTextAreaElement;
    if (!g) return;
    const cs = getComputedStyle(inp);
    const PROPS = [
      "fontStyle", "fontVariant", "fontWeight", "fontStretch",
      "fontSize", "lineHeight", "fontFamily", "letterSpacing",
      "wordSpacing", "fontKerning", "fontFeatureSettings",
      "fontVariationSettings", "whiteSpace", "overflowWrap",
      "wordBreak", "hyphens", "tabSize", "direction", "unicodeBidi",
    ] as const;
    const gs = g.style as unknown as Record<string, string>;
    const c = cs as unknown as Record<string, string>;
    for (const p of PROPS) {
      if (c[p]) gs[p] = c[p];
    }
  }

  fitInput(force = false) {
    // En tête : m-input-empty dépend de la valeur courante, pas de la
    // mesure — tous les early-returns doivent le rafraîchir (C1 mobile).
    syncSendMic();
    // Sonde télémétrie (#297) : la mesure propre (scrollHeight = reflow
    // forcé) est le suspect n°1 des lag de frappe — chronométrée seulement
    // quand la collecte est active, skip précoce non compté (coût ~0).
    const t0 = telemetryActive() ? performance.now() : -1;
    const inp = $("input") as HTMLTextAreaElement;
    // Moteur sans field-sizing : le miroir #input-ghost dimensionne le
    // wrap en flux normal — aucune mesure JS (scrollHeight = reflow
    // synchrone proportionnel au fil sur les vieux webview).
    if (!FIELD_SIZING) {
      this.fitInputGhost(force);
      if (t0 >= 0) probeFitInput(performance.now() - t0);
      return;
    }
    if (!force && !this.fitStale && inp.value === this.lastFitVal) return;
    this.lastFitVal = inp.value;
    this.fitStale = false;
    // Mesure : hauteur nécessaire au contenu. En field-sizing le
    // navigateur dimensionne via max-height — scrollHeight lit le
    // contenu complet même cappé, pas de reset nécessaire.
    const need = inp.scrollHeight + 2;
    const over = need > this.smallCap();
    if (!over) this.inpExpanded = false;
    // Cap via custom property : consommée par max-height: var(--fit-cap)
    // dans chat.css (chemin field-sizing et miroir autosize du wrap).
    inp.style.setProperty("--fit-cap", this.fitCap() + "px");
    this.paintFit(over, need);
    if (t0 >= 0) probeFitInput(performance.now() - t0);
  }

  private async runBuiltin(name: string, arg: string): Promise<boolean> {
    if (!app.current) return false;
    switch (name) {
      case "model":
        if (!arg) {
          // /model nu : liste les modèles annoncés par l'agent
          // (config_options, fallback available_models) + le courant.
          const opt = app.current.config_options?.find(
            (o) => o.id === "model" || o.category === "model",
          );
          const items = (opt?.options ?? []).map((o) => ({
            v: o.value,
            label: o.name || o.value,
          }));
          // Merge : available_models complète les options déjà annoncées.
          for (const v of app.current.available_models) {
            if (!items.some((i) => i.v === v)) items.push({ v, label: v });
          }
          const cur = opt?.currentValue || app.current.model || "";
          this.chat.sysline(
            items.length
              ? t("model_list", {
                  models: items
                    .map((i) => (i.v === cur ? `${i.label} (${t("model_current")})` : i.label))
                    .join(", "),
                })
              : t("model_none"),
          );
          return true;
        }
        // Le démon résout tolérant (préfixe/nom univoque) et refuse les
        // valeurs hors liste annoncée — erreur avec candidats en retour.
        await call("set_model", { session: app.current.id, model: arg });
        return true;
      case "resume":
        await this.openNativePicker();
        return true;
      case "cancel":
        await call("cancel", { session: app.current.id });
        return true;
      case "clear":
      case "new": {
        // Sémantique /clear des CLI (Devin : alias /new) : journal vidé +
        // contexte agent reparti à zéro (session/new côté démon). La vue est
        // rechargée — le journal ne contient plus que le nouveau cycle.
        const id = app.current.id; // current peut changer pendant l'await
        await call("clear_session", { session: id });
        await this.chat.open(id);
        return true;
      }
      case "sleep":
        // Endort : libère le process, la session native reste reprenable —
        // le prochain envoi la réveille via session/load côté démon.
        await call("sleep_session", { session: app.current.id });
        await this.refresh();
        return true;
      case "fork": {
        // Le daemon libère la session native, /fork via le TUI de l'agent,
        // puis attache la session forkée (même groupe, titre + "_fork").
        this.chat.sysline(t("fork_started"));
        const r = await call<{ session: string }>("fork_session", {
          session: app.current.id,
        });
        await this.refresh();
        if (r.session) await this.chat.open(r.session);
        return true;
      }
    }
    return false;
  }

  /** Dialogue : sessions natives de l'agent (ACP session/list) → reprise. */
  private async openNativePicker() {
    if (!app.current) return;
    type Sess = { sessionId: string; title?: string; updatedAt?: string; cwd?: string };
    const dlg = $("dlg-resume") as HTMLDialogElement;
    const box = $("native-list") as HTMLElement;
    const filter = $("native-filter") as HTMLInputElement;
    const track = $("native-scrollbar") as HTMLElement;
    const thumb = $("native-scroll-thumb") as HTMLElement;
    box.innerHTML = "…";
    filter.value = "";
    track.classList.remove("on");
    dlg.showModal();
    filter.focus();

    let all: Sess[] = [];
    let loaded = false;
    let failed = false;
    // Actions parallèles à `rows` : Enter/↑↓ agissent via `acts[sel]`
    // (la ligne fallback d'erreur n'est pas une Sess).
    let acts: (() => void)[] = [];
    let sel = -1;
    let rows: HTMLElement[] = [];

    const matches = (s: Sess, q: string) => {
      const hay = `${s.sessionId} ${s.title ?? ""} ${s.cwd ?? ""} ${(s.updatedAt ?? "").slice(0, 16)}`.toLowerCase();
      return hay.includes(q);
    };

    const pick = (s: Sess | undefined) => {
      if (!s) return;
      dlg.close();
      notify("info", t("resume_wait"));
      void call("resume_session", { session: app.current!.id, native_id: s.sessionId })
        .catch((e) => showErr(String(e)))
        .then(() => this.chat.open(app.current!.id));
    };

    const syncThumb = () => {
      const h = box.clientHeight;
      const sh = box.scrollHeight;
      if (sh <= h) { track.classList.remove("on"); return; }
      track.classList.add("on");
      const th = Math.max(24, (h / sh) * h);
      const top = box.scrollTop / (sh - h) * (h - th);
      thumb.style.height = th + "px";
      thumb.style.top = top + "px";
    };

    const render = () => {
      if (failed) return; // état d'erreur : la ligne fallback reste
      const q = filter.value.trim().toLowerCase();
      const list = q ? all.filter((s) => matches(s, q)) : all;
      acts = [];
      rows = [];
      sel = -1;
      box.innerHTML = "";
      if (!list.length) {
        box.textContent = !loaded ? "…" : all.length ? t("resume_no_match") : t("no_native");
      }
      for (const s of list) {
        const row = document.createElement("div");
        row.className = "ac-item";
        row.innerHTML = `<span class="n">${esc(s.sessionId.slice(0, 12))}</span>` +
          `<span class="d">${esc(s.title || s.cwd || "")}</span>` +
          `<span class="h">${esc((s.updatedAt || "").slice(0, 16).replace("T", " "))}</span>`;
        const idx = rows.length;
        row.onclick = () => pick(s);
        row.onmouseenter = () => { sel = idx; syncSel(); };
        box.appendChild(row);
        rows.push(row);
        acts.push(() => pick(s));
      }
      box.scrollTop = 0;
      sel = rows.length ? 0 : -1;
      syncSel();
      syncThumb();
    };

    const syncSel = () => {
      rows.forEach((r, i) => r.classList.toggle("sel", i === sel));
    };

    // Navigation clavier : ↑/↓ sur les entrées, Entrée pour choisir,
    // Échap vide le filtre puis ferme (comme les pickers natifs).
    filter.onkeydown = (e) => {
      if (e.key === "ArrowDown" || e.key === "ArrowUp") {
        if (!rows.length) return;
        e.preventDefault();
        sel = (sel + (e.key === "ArrowDown" ? 1 : -1) + rows.length) % rows.length;
        rows[sel].scrollIntoView({ block: "nearest" });
        syncSel();
      } else if (e.key === "Enter") {
        e.preventDefault(); // pas de submit implicite du <form method=dialog>
        acts[sel]?.();
      } else if (e.key === "Escape") {
        if (filter.value) {
          e.preventDefault();
          filter.value = "";
          render();
        } else {
          dlg.close();
        }
      }
    };

    filter.oninput = render;
    box.onscroll = syncThumb;
    // window resize pendant le dialogue → recalcul du thumb.
    const onResize = () => syncThumb();

    // Scrollbar custom : la piste native (overlay fine) ne grab pas
    // dans <dialog> — thumb DOM draggable via pointer events. Handlers
    // nommés et retirés au close : thumb/track sont du DOM statique,
    // les re-attacher à chaque ouverture les cumulerait (wheel ×N).
    const onThumbDown = (e: PointerEvent) => {
      e.preventDefault();
      thumb.setPointerCapture(e.pointerId);
      const startY = e.clientY;
      const startTop = box.scrollTop;
      const trackH = box.clientHeight;
      const range = box.scrollHeight - trackH;
      const thumbH = thumb.offsetHeight;
      const move = (ev: PointerEvent) => {
        const dy = ev.clientY - startY;
        const dScroll = (dy / Math.max(1, trackH - thumbH)) * range;
        box.scrollTop = startTop + dScroll;
        syncThumb();
      };
      const up = () => {
        thumb.removeEventListener("pointermove", move);
        thumb.removeEventListener("pointerup", up);
        thumb.removeEventListener("pointercancel", up);
      };
      thumb.addEventListener("pointermove", move);
      thumb.addEventListener("pointerup", up);
      thumb.addEventListener("pointercancel", up);
    };
    // Clic sur la piste (hors thumb) : sauter à la position cliquée.
    // Molette au-dessus de la piste = scroll aussi (la piste n'est pas
    // dans #native-list, le wheel ne la traverserait pas).
    const onTrackWheel = (e: WheelEvent) => {
      e.preventDefault();
      box.scrollTop += e.deltaY;
      syncThumb();
    };
    const onTrackDown = (e: PointerEvent) => {
      if (e.target !== track) return;
      e.preventDefault(); // garder le focus dans le filtre (↑/↓/Entrée)
      const r = track.getBoundingClientRect();
      const frac = (e.clientY - r.top - thumb.offsetHeight / 2) / Math.max(1, r.height - thumb.offsetHeight);
      box.scrollTop = Math.max(0, Math.min(1, frac)) * (box.scrollHeight - box.clientHeight);
      syncThumb();
    };
    thumb.addEventListener("pointerdown", onThumbDown);
    track.addEventListener("wheel", onTrackWheel, { passive: false });
    track.addEventListener("pointerdown", onTrackDown);
    window.addEventListener("resize", onResize);
    dlg.addEventListener("close", () => {
      window.removeEventListener("resize", onResize);
      thumb.removeEventListener("pointerdown", onThumbDown);
      track.removeEventListener("wheel", onTrackWheel);
      track.removeEventListener("pointerdown", onTrackDown);
      filter.onkeydown = filter.oninput = box.onscroll = null;
    }, { once: true });

    try {
      const r = await call<{ sessions?: Sess[] }>(
        "list_native_sessions",
        { session: app.current.id },
      );
      all = r.sessions ?? [];
      loaded = true;
      render();
    } catch (e) {
      // Agent mort → impossible de lister ; proposer la reprise de la session courante
      loaded = true;
      failed = true;
      all = [];
      box.innerHTML = `<div class="errline">${esc(String(e))}</div>`;
      const row = document.createElement("div");
      row.className = "ac-item";
      row.innerHTML = `<span class="n">${icon("play")}</span><span class="d">${t("resume_current")}</span>`;
      const act = async () => {
        dlg.close();
        notify("info", t("resume_wait"));
        await call("resume_session", { session: app.current!.id }).catch((er) => showErr(String(er)));
        await this.chat.open(app.current!.id);
      };
      row.onclick = () => void act();
      row.onmouseenter = () => { sel = 0; syncSel(); };
      box.appendChild(row);
      rows = [row];
      acts = [act];
      sel = 0;
      syncSel();
    }
  }

  // ---- File d'attente des prompts (queue_list/edit/drop/promote) ----
  // #queue-bar = bande résumé (chips tronquées) ; le bouton horloge +
  // compteur ouvre #queue-panel : messages complets multi-lignes, édition
  // en place via queue_edit, promote/drop par entrée et action secondaire
  // « reprendre dans le composer » (copie + drop).

  /** Panneau d'inspection ouvert — persiste tant que la file n'est pas
   *  vide (la file qui se vide referme tout, édition comprise). */
  private qOpen = false;
  /** Dernier snapshot de queue_list — les re-renders déclenchés par les
   *  boutons l'utilisent sans refetch. */
  private qSnapshot: string[] = [];
  /** Édition inline en cours : index dans la file + texte serveur au
   *  moment de l'ouverture (un écart = entrée déplacée/supprimée
   *  ailleurs → éditeur refermé) + brouillon tapé et curseur, qui
   *  survivent aux rebuilds de refreshQueue pendant la frappe. */
  private qEditing: {
    index: number;
    orig: string;
    draft: string;
    sel?: [number, number];
  } | null = null;
  /** Conns dont le démon ne connaît pas QueueEdit (« parse: unknown
   *  variant ») : l'édition y est désactivée (tooltip) — list/promote/
   *  drop sont des ReqKind plus anciens et restent utilisables. */
  private qEditBroken = new Set<string>();
  /** Capability `queue_steer` du démon (injectée depuis conns.cap) —
   *  le bouton steer des entrées est masqué sur les vieux démons. */
  private qSteerCapable: (conn: string | undefined) => boolean = () => false;
  /** Capability `queue_clear` du démon — le bouton « tout vider » de
   *  l'en-tête du panneau est masqué sur les vieux démons. */
  private qClearCapable: (conn: string | undefined) => boolean = () => false;
  /** Timer de désarmement du « tout vider » (confirm armé 2.5 s). */
  private qClearTimer: ReturnType<typeof setTimeout> | undefined;
  /** Rebuild du panneau en cours : le blur d'une textarea détachée par
   *  innerHTML ne doit ni sauvegarder ni annuler (l'éditeur est recréé
   *  juste après si l'entrée est toujours là). */
  private qRebuilding = false;

  async refreshQueue() {
    const bar = $("queue-bar");
    if (!app.current) {
      bar.hidden = true;
      this.closeQueuePanel();
      return;
    }
    const sid = app.current.id;
    try {
      const r = await callBg<{ queue: string[] }>("queue_list", { session: sid });
      // La session affichée a pu changer pendant l'appel → ne pas rendre
      // une file qui n'est plus celle de la vue.
      if (app.current?.id !== sid) return;
      this.qSnapshot = r.queue;
      if (!r.queue.length) {
        bar.hidden = true;
        this.closeQueuePanel();
        return;
      }
      bar.hidden = false;
      // Entrée éditée déplacée/supprimée entre-temps (drop/promote ici
      // ou par un autre client) → fermer l'éditeur plutôt que risquer
      // d'écraser un autre slot (le protocole est index-based).
      if (this.qEditing && r.queue[this.qEditing.index] !== this.qEditing.orig) {
        this.qEditing = null;
      }
      this.renderQueueBar(sid);
      this.renderQueuePanel();
    } catch (e) {
      // Transitoire conn : garder le dernier état connu de la file — le
      // prochain refresh (retour conn, nouvelle action) re-synchronisera.
      if (isConnLost(e, app.current?.conn ?? "")) return;
      bar.hidden = true;
      this.closeQueuePanel();
    }
  }

  /** Bande résumé : bouton d'ouverture (horloge + compteur) puis un chip
   *  tronqué par message avec les actions rapides. */
  private renderQueueBar(sid: string) {
    const bar = $("queue-bar");
    const queue = this.qSnapshot;
    // conn RPC : la clé réelle de la session (absente → call() injecte
    // currentConn ou laisse get_conn résoudre la conn par défaut) ;
    // "default" n'est une clé valide que côté client (qEditBroken).
    const conn = app.current?.conn;
    const connKey = conn ?? "default";
    bar.innerHTML = "";
    const open = qIco("clock", t("queue_inspect"));
    open.id = "queue-open";
    open.classList.toggle("on", this.qOpen);
    open.insertAdjacentHTML("beforeend", `<span class="n">${queue.length}</span>`);
    open.onclick = () => this.toggleQueuePanel();
    bar.appendChild(open);
    if (isAndroid()) {
      // Sur mobile la barre se réduit à une puce (compteur + aperçu du
      // 1er message) : les entrées et leurs actions vivent dans la
      // feuille basse #queue-panel, pas dans une rangée de chips.
      open.classList.add("q-chip");
      open.innerHTML =
        `${icon("clock")}<b>${esc(t("queue_chip", { n: queue.length }))}</b>` +
        `<span class="q-ex">${queue[0] ? `· ${esc(queue[0])}` : ""}</span>`;
      return;
    }
    const broken = this.qEditBroken.has(connKey);
    queue.forEach((txt, i) => {
      const d = document.createElement("div");
      d.className = "q-item";
      const sp = document.createElement("span");
      sp.className = "q-txt";
      sp.textContent = txt;
      sp.title = txt;
      d.appendChild(sp);
      // ✏️ = ouvrir le panneau directement en édition sur cette entrée.
      const e = qIco("edit", broken ? t("queue_edit_unsupported") : t("queue_edit_title"));
      e.disabled = broken;
      e.onclick = () => {
        this.qEditing = { index: i, orig: txt, draft: txt };
        if (!this.qOpen) this.toggleQueuePanel();
        else this.renderQueuePanel();
      };
      // Steer : injection dans le tour en cours SANS l'annuler (≠ ⤒
      // qui annule) — refus → remise en file par le démon. Masqué sur
      // les démons ne connaissant pas queue_steer.
      if (this.qSteerCapable(conn)) {
        const st = qIco("steer", t("queue_steer"));
        st.onclick = () => void this.queueSteer(sid, i, conn);
        d.appendChild(st);
      }
      // Tête de file + cancel → envoi forcé (reste de file préservée)
      const p = qIco("top", t("queue_send_now"));
      p.onclick = () => this.queueCall("queue_promote", { session: sid, index: i, conn });
      const x = qIco("close", t("queue_remove"));
      x.onclick = () => this.queueCall("queue_drop", { session: sid, index: i, conn });
      d.append(e, p, x);
      bar.appendChild(d);
    });
  }

  /** Panneau d'inspection : chaque message en entier (multi-lignes),
   *  dans l'ordre, avec édition inline / promote / drop / reprise dans
   *  le composer. */
  private renderQueuePanel() {
    const panel = $("queue-panel");
    if (!this.qOpen || !this.qSnapshot.length || !app.current) {
      panel.hidden = true;
      return;
    }
    const sid = app.current.id;
    const conn = app.current.conn;
    const connKey = conn ?? "default";
    const queue = this.qSnapshot;
    panel.hidden = false;
    $("qp-count").textContent = String(queue.length);
    // « Tout vider » : masqué sur les démons sans queue_clear — le
    // panneau n'existe que file non vide, la garde suffit.
    const clr = $("qp-clear") as HTMLButtonElement;
    clr.hidden = !this.qClearCapable(conn);
    if (clr.hidden) this.disarmQueueClear();
    const list = $("qp-list");
    // Rebuild pendant une édition (refreshQueue déclenché par un nouveau
    // message en file) : la textarea va être recréée — sauvegarder le
    // curseur pour ne pas sauter en fin de texte à chaque frappe.
    const activeEl = document.activeElement;
    if (
      this.qEditing &&
      activeEl instanceof HTMLTextAreaElement &&
      activeEl.classList.contains("qp-edit")
    ) {
      this.qEditing.sel = [activeEl.selectionStart, activeEl.selectionEnd];
    }
    this.qRebuilding = true;
    list.innerHTML = "";
    this.qRebuilding = false;
    const broken = this.qEditBroken.has(connKey);
    queue.forEach((txt, i) => {
      const row = document.createElement("div");
      row.className = "qp-item";
      const idx = document.createElement("span");
      idx.className = "qp-idx";
      idx.textContent = `${i + 1}`;
      row.appendChild(idx);
      const acts = document.createElement("span");
      acts.className = "qp-acts";
      if (this.qEditing?.index === i && !broken) {
        this.renderQueueEditor(row, acts, sid, conn, i);
      } else {
        const sp = document.createElement("span");
        sp.className = "qp-txt";
        sp.textContent = txt;
        row.appendChild(sp);
        const e = qIco("edit", broken ? t("queue_edit_unsupported") : t("queue_edit_title"));
        e.disabled = broken;
        e.onclick = () => {
          this.qEditing = { index: i, orig: txt, draft: txt };
          this.renderQueuePanel();
        };
        // Action secondaire : copier dans le composer + retirer de la
        // file (perte de position — l'édition inline couvre le cas
        // « corriger avant envoi »).
        const m = qIco("down", t("queue_to_composer"));
        m.onclick = () => {
          this.setInput(txt);
          ($("input") as HTMLTextAreaElement).focus();
          this.queueCall("queue_drop", { session: sid, index: i, conn });
        };
        acts.append(e, m);
      }
      if (this.qSteerCapable(conn)) {
        const st = qIco("steer", t("queue_steer"));
        st.onclick = () => void this.queueSteer(sid, i, conn);
        acts.appendChild(st);
      }
      const p = qIco("top", t("queue_send_now"));
      p.onclick = () => this.queueCall("queue_promote", { session: sid, index: i, conn });
      const x = qIco("close", t("queue_remove"));
      x.onclick = () => this.queueCall("queue_drop", { session: sid, index: i, conn });
      acts.append(p, x);
      // preventDefault sur mousedown de chaque bouton : sinon le clic
      // déplace le focus → blur de la textarea d'édition → done() →
      // rebuild synchrone qui détache le bouton avant son click →
      // action silencieusement perdue (le focus reste, le clic part,
      // et le refreshQueue de l'action referme l'éditeur si l'entrée
      // a bougé).
      for (const b of acts.querySelectorAll<HTMLButtonElement>("button")) {
        b.onmousedown = (e) => e.preventDefault();
      }
      row.appendChild(acts);
      list.appendChild(row);
    });
  }

  /** queue_* puis re-render : l'erreur part en toast plutôt que de
   *  finir en unhandled rejection (conn down, index périmé…). */
  private queueCall(method: string, params: Record<string, unknown>) {
    // queue_drop : le message retiré ne sera jamais journalisé —
    // retirer l'écho pending correspondant (sinon bulle orpheline).
    const dropped =
      method === "queue_drop"
        ? this.qSnapshot[params.index as number]
        : undefined;
    const sid = params.session as string;
    void call(method, params)
      .then(() => {
        if (dropped != null) this.chat.dropEchoesByText(sid, dropped);
        return this.refreshQueue();
      })
      .catch((e) => showErr(String(e)));
  }

  /** Bouton « steer » d'une entrée : le démon la sort de la file et
   *  tente un session/prompt concurrent (tour préservé — ≠ promote qui
   *  annule). `delivered` de la réponse est traduit en toast ; la file
   *  renvoyée met la vue à jour immédiatement. */
  private async queueSteer(sid: string, index: number, conn: string | undefined) {
    try {
      const r = await call<{
        delivered: "steer" | "sent" | "requeued" | "held" | "gone";
        reason?: string | null;
        queue?: string[];
      }>("queue_steer", { session: sid, index, conn });
      if (Array.isArray(r.queue)) this.qSnapshot = r.queue;
      switch (r.delivered) {
        case "steer":
          // Refus tardif possible : l'event système du démon déclenche
          // déjà un refresh — ce rattrapage couvre les cas sans push.
          setTimeout(() => void this.refreshQueue(), 2500);
          break;
        case "sent":
          notify("info", t("queue_steer_sent"));
          break;
        case "requeued":
          notify("warn", t("queue_steer_refused"));
          break;
        case "held":
          notify("warn", t(
            r.reason === "compact" ? "queue_steer_compact"
            : r.reason === "asleep" ? "queue_steer_asleep"
            : r.reason === "not_ready" ? "queue_steer_notready"
            : "queue_steer_wait",
          ));
          break;
        case "gone":
          notify("warn", t("queue_edit_failed"));
          break;
      }
    } catch (e) {
      showErr(String(e));
    }
    await this.refreshQueue();
  }

  /** Item en édition inline : textarea + ✓/✗. Sauvegarde = {mod}+Entrée
   *  ou perte de focus ; Échap = annulation. preventDefault sur le
   *  mousedown des boutons : sans ça le clic tirerait le blur de la
   *  textarea (= sauvegarde) avant leur onclick — l'annuler serait perdu. */
  private renderQueueEditor(
    row: HTMLElement,
    acts: HTMLElement,
    sid: string,
    conn: string | undefined,
    i: number,
  ) {
    const ed = this.qEditing!;
    const ta = document.createElement("textarea");
    ta.className = "qp-edit";
    ta.value = ed.draft;
    ta.rows = Math.min(10, Math.max(2, ed.draft.split("\n").length));
    ta.spellcheck = false;
    ta.title = t("queue_edit_hint");
    ta.setAttribute("aria-label", ta.title);
    ta.addEventListener("input", () => {
      ed.draft = ta.value;
    });
    row.appendChild(ta);
    let finished = false;
    const done = (save: boolean) => {
      if (finished) return;
      // Blur zombie : la textarea est détachée par un rebuild de la
      // liste (refreshQueue), pas par un choix de l'utilisateur → la
      // nouvelle instance de l'éditeur prend le relais, rien à faire.
      // isConnected couvre un blur dispatché après détachement (timing
      // webview), indépendamment du flag synchrone qRebuilding.
      if (this.qRebuilding || !ta.isConnected) {
        finished = true;
        return;
      }
      finished = true;
      const text = ta.value;
      // qEditing invalidé entre-temps (entrée déplacée → le rebuild a
      // détaché cette textarea, dont le blur rappelle done) : ne rien
      // écrire, l'index viserait un autre slot.
      const live = this.qEditing === ed;
      this.qEditing = null;
      // Texte vidé → pas de prompt zombie en file : annulation +
      // avertissement plutôt que queue_edit "".
      if (save && live && text !== ed.orig && !text.trim()) {
        notify("warn", t("queue_edit_empty"));
      }
      if (!save || !live || text === ed.orig || !text.trim()) {
        this.renderQueuePanel();
        return;
      }
      call<{ ok: boolean }>("queue_edit", {
        session: sid,
        index: i,
        text,
        conn,
      })
        .then((r) => {
          // Index périmé (entrée déplacée/supprimée par ailleurs).
          if (!r.ok) notify("warn", t("queue_edit_failed"));
          // L'écho pending du message édité doit porter le texte qui
          // sera journalisé à la livraison (réconciliation par texte).
          else this.chat.retextEcho(sid, ed.orig, text);
        })
        .catch((e) => {
          this.markQueueEditBroken(conn ?? "default", e);
          showErr(String(e));
        })
        .finally(() => this.refreshQueue());
    };
    ta.addEventListener("keydown", (e) => {
      if (e.key === "Escape") {
        e.preventDefault();
        // Scoper la touche : un Échap remontant à window fermerait aussi
        // un éventuel overlay ouvert au-dessus (settings…).
        e.stopPropagation();
        done(false);
      }
      if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
        e.preventDefault();
        done(true);
      }
    });
    ta.addEventListener("blur", () => done(true));
    const save = qIco("check", t("queue_save"));
    save.onmousedown = (e) => e.preventDefault();
    save.onclick = () => done(true);
    const cancel = qIco("close", t("queue_cancel_edit"));
    cancel.onmousedown = (e) => e.preventDefault();
    cancel.onclick = () => done(false);
    acts.append(save, cancel);
    queueMicrotask(() => {
      if (ed.sel) {
        const [s, e2] = ed.sel;
        ta.setSelectionRange(Math.min(s, ta.value.length), Math.min(e2, ta.value.length));
      }
      ta.focus();
    });
  }

  private toggleQueuePanel() {
    if (this.qOpen) {
      this.closeQueuePanel();
    } else if (this.qSnapshot.length && app.current) {
      this.qOpen = true;
      nav.push("queue");
      if (isAndroid()) nav.showScrim("queue");
      this.renderQueuePanel();
      $("queue-open")?.classList.add("on");
    }
  }

  /** Ferme le panneau (✕, file vide, session changée, back Android) —
   *  dépile aussi l'entrée nav et l'état « on » du bouton horloge. */
  closeQueuePanel() {
    if (this.qOpen) nav.close("queue");
    nav.hideScrim("queue");
    this.qOpen = false;
    this.qEditing = null;
    this.disarmQueueClear();
    const panel = $("queue-panel");
    if (panel) panel.hidden = true;
    $("queue-open")?.classList.remove("on");
  }

  /** Désarme le « tout vider » : libellé de base + classe armed
   *  retirée (expiré au bout de 2.5 s ou au 2e clic). */
  private disarmQueueClear() {
    if (this.qClearTimer) {
      clearTimeout(this.qClearTimer);
      this.qClearTimer = undefined;
    }
    const b = $("qp-clear") as HTMLButtonElement | null;
    if (b) {
      b.classList.remove("armed");
      b.textContent = t("queue_clear");
    }
  }

  /** Vieux démon : ReqKind::QueueEdit inconnu → « parse: unknown
   *  variant ». L'édition est désactivée pour la conn de l'appel. */
  private markQueueEditBroken(connKey: string, e: unknown) {
    if (/unknown variant|parse:/i.test(String(e))) {
      this.qEditBroken.add(connKey);
    }
  }

  // ---- /compact : la compaction tourne en arrière-plan côté agent après un
  // idle immédiat — le démon passe la session en "compacting" et retient
  // les prompts en file (visibles dans la barre de file) jusqu'au signal
  // de fin (contexte réduit, message de fin, cancel, timeout). Un prompt
  // livré pendant le compact l'annulerait.

  async sendInput(mode: SendMode = "normal") {
    const txt = ($("input") as HTMLTextAreaElement).value.trim();
    if (!app.current) return;
    // Built-ins client (interceptent avant envoi à l'agent)
    const bm = txt ? /^\/(\w+)(?:\s+(.*))?$/.exec(txt) : null;
    if (bm && BUILTINS().some((b) => b.name === bm[1])) {
      this.setInput("");
      try {
        await this.runBuiltin(bm[1], (bm[2] ?? "").trim());
      } catch (e) {
        showErr(String(e));
      }
      return;
    }
    // Slash-commande ni builtin ni exposée par l'agent (available_commands) :
    // l'envoyer en texte brut coûte un tour pour rien (ex. /fork chez Devin =
    // TUI-only, jamais routé). Avertir localement. Liste vide = agent sans
    // annonce → on ne peut pas trancher, on laisse passer.
    if (
      bm &&
      app.current.available_commands.length > 0 &&
      !app.current.available_commands.some(
        (c) => cmdName(c).toLowerCase() === bm[1].toLowerCase(),
      )
    ) {
      this.setInput("");
      this.chat.sysline(t("cmd_unknown", { cmd: bm[1] }), "warn");
      return;
    }
    // Pièces jointes : envoyer maintenant perdrait les chemins des
    // uploads en vol — avertir plutôt que tronquer le message.
    if (this.attach.pending()) {
      notify("warn", t("att_wait"));
      return;
    }
    const files = this.attach.ready();
    if (!txt && !files.length) return;
    // Le message porte les chemins absolus — l'agent lit les pièces
    // avec ses outils fichiers habituels (bornés au même workspace).
    const atLines = files
      .map((f) => `[fichier joint: ${f.name} → ${f.path}]`)
      .join("\n");
    const payload = txt
      ? atLines
        ? `${txt}\n\n${atLines}`
        : txt
      : atLines;
    const sid = app.current.id; // peut changer pendant l'await
    this.setInput("");
    // Écho optimiste : la bulle user apparaît dès l'Enter — le
    // user_message du journal n'arrive qu'à la livraison (file,
    // réveil), le RPC send n'enfile que le prompt. Le pending est
    // réconcilié à l'arrivée du vrai event ; retiré si le RPC échoue.
    const echoId = this.chat.addEcho(sid, payload, { steer: mode === "steer" });
    try {
      await call("send", {
        session: sid,
        text: payload,
        force: mode === "force",
        steer: mode === "steer",
      });
      this.attach.sent(sid);
    } catch (e) {
      this.chat.dropEcho(sid, echoId);
      showErr(String(e));
      // Envoi refusé : le brouillon n'est vidé que sur succès — rendre
      // le texte à SA session (champ, ou draft garé si l'utilisateur a
      // changé de session entre-temps), fusionné à une frappe plus
      // récente. Pas de regare sous une session qui n'existe plus.
      if (app.current?.id === sid) {
        const v = ($("input") as HTMLTextAreaElement).value;
        this.setInput([v, txt].filter(Boolean).join("\n"));
      } else if (txt && app.lastSessions.some((s) => s.id === sid)) {
        this.drafts.set(
          sid,
          [this.drafts.get(sid), txt].filter(Boolean).join("\n"),
        );
      }
    }
    // Commande slash non-builtin : l'agent l'applique au prochain tour (ex.
    // /compact → "context compacted" n'arrive qu'au message suivant) → hint.
    // (/compact : le démon arme la retenue "compacting" à la livraison.)
    if (bm) {
      this.chat.sysline(t("cmd_sent_hint", { cmd: bm[1] }));
    }
    void this.refreshQueue();
  }

  /** Envoie dans un mode explicite — voie publique pour les actions
   *  mobiles (appui long Envoyer → steer/file, C1) qui ne passent pas
   *  par le bouton principal ni le bouton alternatif. */
  sendWith(mode: SendMode) {
    void this.sendInput(mode);
  }

  // ---- Autocomplete des slash-commands ----
  private moveAcSel(dir: number) {
    const items = [...$("cmd-ac").querySelectorAll<HTMLElement>(".ac-item")];
    if (!items.length) return;
    this.acSel = (this.acSel + dir + items.length) % items.length;
    items.forEach((el, i) => el.classList.toggle("sel", i === this.acSel));
    items[this.acSel].scrollIntoView({ block: "nearest" });
  }

  private applyAc(name: string) {
    this.setInput("/" + name + " ");
    ($("input") as HTMLTextAreaElement).focus();
  }

  /** Requête pour laquelle la dropdown a été construite + référence de
   *  la liste de commandes — rebuild sauté tant que ni l'une ni l'autre
   *  n'a changé (la frappe fait varier la valeur à chaque touche, mais
   *  pas toujours la query slash : texte libre, collage…). */
  private acQuery: string | null = null;
  private acCmds: CmdInfo[] | null = null;
  /** Valeur modifiée depuis le dernier refreshAc rendu — le keydown
   *  resynchronise avant d'agir sur une sélection qui pourrait être
   *  d'une frame de retard. */
  private acStale = false;
  /** Dropdown masquée par Échap/blur : un rAF en vol ne doit pas la
   *  rouvrir — seule une vraie frappe (event input) la ré-arme. */
  private acDismissed = false;

  private refreshAc() {
    const inp = $("input") as HTMLTextAreaElement;
    const ac = $("cmd-ac");
    const v = inp.value;
    const m = /^\/([\w-]*)$/.exec(v);
    this.acStale = false;
    if (!m || !app.current || this.acDismissed) {
      ac.hidden = true;
      this.acQuery = null;
      this.acCmds = null;
      return;
    }
    const q = m[1].toLowerCase();
    const cmds = app.current.available_commands;
    if (q === this.acQuery && cmds === this.acCmds && !ac.hidden) return;
    this.acQuery = q;
    this.acCmds = cmds;
    const matches = [
      ...cmds,
      ...BUILTINS(),
    ]
      .filter((c) => cmdName(c).toLowerCase().startsWith(q))
      .slice(0, 12);
    if (!matches.length) {
      ac.hidden = true;
      return;
    }
    ac.innerHTML = "";
    this.acSel = 0;
    matches.forEach((c: CmdInfo, i: number) => {
      const n = cmdName(c);
      const el = document.createElement("div");
      el.className = "ac-item" + (i === 0 ? " sel" : "");
      el.dataset.name = n;
      const h = cmdHint(c);
      el.innerHTML = `<span class="n">/${esc(n)}</span>` +
        (h ? `<span class="h">${esc(h)}</span>` : "") +
        `<span class="d">${esc(cmdDesc(c))}</span>`;
      el.title = cmdDesc(c);
      el.onclick = () => this.applyAc(n);
      ac.appendChild(el);
    });
    ac.hidden = false;
  }

  /** Mode par défaut d'Entrée/bouton Envoyer (préférence paramètres). */
  private defaultMode(): SendMode {
    return sendPref() === "steer" ? "steer" : "normal";
  }

  /** Mode du bouton secondaire = l'inverse de la préférence : file ↔
   *  steer. C'est LA voie « autre mode » sur mobile (pas d'Alt+Entrée). */
  private altMode(): SendMode {
    return this.defaultMode() === "steer" ? "normal" : "steer";
  }

  /** Icône, tooltip, label et visibilité du bouton d'envoi alternatif —
   *  suivent la préférence (sans reload) et l'état de la session.
   *  Rôle steer : pertinent seulement pendant un vrai tour (hors working
   *  un steer tombe dans le flux normal — même effet que Envoyer).
   *  Rôle file : envoi normal — pertinent quand le steer ferait autre
   *  chose, donc agent occupé (working / waiting_input : un normal part
   *  en file). Pendant compact les deux modes sont retenus en file —
   *  bouton sans différence → caché. */
  syncAltBtn() {
    this.attach.syncVisible();
    const b = $("btn-alt") as HTMLButtonElement;
    const queue = this.altMode() === "normal";
    const st = app.current?.state;
    b.hidden = queue
      ? st !== "working" && st !== "waiting_input"
      : st !== "working";
    const ic = queue ? "clock" : "steer";
    if (b.dataset.icon !== ic) {
      b.dataset.icon = ic;
      b.innerHTML = icon(ic);
    }
    // data-i18n-title tenu à jour : applyStatic (changement de langue)
    // retraduit depuis l'attribut, pas depuis b.title.
    const key = queue ? "queue_title" : "steer_title";
    b.dataset.i18nTitle = key;
    b.title = t(key);
    b.setAttribute("aria-label", b.title);
  }

  /** Hints clavier du champ — dépendent du mode d'envoi choisi. */
  refreshHints() {
    // Un placeholder plus long peut wrapper → refit, et le titre de la
    // pill suit la langue courante.
    this.fitInput(true);
    this.syncAltBtn(); // bouton alternatif : suit la préférence/langue
    if (isAndroid()) return; // placeholder mobile sans raccourcis
    ($("input") as HTMLTextAreaElement).placeholder =
      sendPref() === "steer" ? t("input_ph_steer") : t("input_ph");
  }

  /** Session affichée changée : le brouillon comme les chips de pièces
   *  jointes appartiennent à la session qui les a vus — garés sous son
   *  id, ceux de la session affichée sont réinstallés. */
  sessionChanged() {
    const cur = app.current?.id ?? null;
    if (cur !== this.draftSid) {
      const text = ($("input") as HTMLTextAreaElement).value;
      if (this.draftSid) {
        // Le texte affiché appartient à la session quittée.
        if (text) this.drafts.set(this.draftSid, text);
        else this.drafts.delete(this.draftSid);
      }
      // Réinstaller le brouillon de la session affichée ; un texte tapé
      // sans session ouverte est attribué à celle qui arrive (il visait
      // l'agent à venir).
      let next = cur ? (this.drafts.get(cur) ?? "") : "";
      if (cur) this.drafts.delete(cur);
      if (!this.draftSid && cur && text) next = next ? `${next}\n${text}` : text;
      this.draftSid = cur;
      // La pile d'undo est propre à la session quittée : le brouillon
      // restauré ne doit pas être annulable (ni recevoir de pas).
      this.clearHistory();
      this.setInput(next, { skipUndo: true });
    }
    this.attach.onSessionChanged();
  }

  /** Session supprimée : son brouillon et ses pièces garées partent
   *  avec elle (session_deleted, ou onSessionGone depuis markGone —
   *  gare d'abord le contenu affiché sous son id). Cas défensif : si la
   *  session visée est encore affichée, son texte est jeté du champ. */
  dropSession(id: string) {
    this.drafts.delete(id);
    if (this.draftSid === id) {
      this.draftSid = null;
      // Le texte affiché appartenait à la session supprimée — sa pile
      // d'undo part avec elle.
      this.clearHistory();
      this.setInput("", { skipUndo: true });
    }
    this.attach.dropSession(id);
  }

  /** Capacité « fichiers » du démon courant (injecté depuis conns.cap)
   *  — le + est masqué si le démon ignore write_file/create_path. */
  bindAttachCapable(capable: (conn: string | undefined) => boolean) {
    this.attach.capable = capable;
  }

  /** Capacité « steer de file » du démon (injectée depuis conns.cap)
   *  — bouton steer des entrées masqué si queue_steer est inconnu. */
  bindQueueSteerCapable(capable: (conn: string | undefined) => boolean) {
    this.qSteerCapable = capable;
  }

  /** Capacité « clear de file » du démon (injectée depuis conns.cap)
   *  — bouton « tout vider » masqué si queue_clear est inconnu. */
  bindQueueClearCapable(capable: (conn: string | undefined) => boolean) {
    this.qClearCapable = capable;
  }

  init() {
    this.attach.init();
    this.draftSid = app.current?.id ?? null;
    this.lastKnown = this.snapshot();
    this.mic.init({
      setInput: (v, group) => this.setInput(v, group ? { undoGroup: group } : undefined),
    });
    this.refreshHints();
    $("btn-send").onclick = () => this.sendInput(this.defaultMode());
    $("btn-alt").onclick = () => this.sendInput(this.altMode());
    $("input").addEventListener("keydown", (e) => {
      // Undo/redo custom (la pile native est vide par les .value=) —
      // jamais pendant une composition IME : l'autocorrect et le
      // clavier virtuel Android ont leurs propres annulations.
      if (!e.isComposing && (e.ctrlKey || e.metaKey) && !e.altKey) {
        const k = e.key.toLowerCase();
        if (k === "z" && !e.shiftKey) {
          e.preventDefault();
          this.undo();
          return;
        }
        if (k === "y" || (k === "z" && e.shiftKey)) {
          e.preventDefault();
          this.redo();
          return;
        }
      }
      const ac = $("cmd-ac");
      // Le refresh coalescé sur rAF peut ne pas avoir encore vu la
      // dernière frappe : resynchroniser avant d'agir sur la sélection
      // (sinon Entrée/Tab appliquent une commande périmée).
      if (!ac.hidden && this.acStale) this.refreshAc();
      if (!ac.hidden) {
        const sel = ac.querySelector<HTMLElement>(".ac-item.sel");
        if (e.key === "ArrowDown" || e.key === "ArrowUp") {
          e.preventDefault();
          this.moveAcSel(e.key === "ArrowDown" ? 1 : -1);
          return;
        }
        if ((e.key === "Tab" || e.key === "Enter") && sel) {
          e.preventDefault();
          this.applyAc(sel.dataset.name!);
          return;
        }
        if (e.key === "Escape") {
          ac.hidden = true;
          this.acDismissed = true;
          return;
        }
      }
      if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
        e.preventDefault();
        // Ctrl+Entrée (Cmd+Entrée sur macOS) = envoi forcé (annule le
        // tour en cours).
        this.sendInput("force");
        return;
      }
      if (e.key === "Enter" && e.altKey) {
        e.preventDefault();
        // Alt+Entrée = l'autre mode (queue ↔ steer selon la préférence).
        this.sendInput(this.altMode());
        return;
      }
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        this.sendInput(this.defaultMode());
      }
    });
    $("input").addEventListener("input", (e) => {
      // Sondes télémétrie (#297) : durée du handler + délai event→paint.
      // t0 = début du traitement ; le rAF de sonde est enregistré APRÈS
      // celui de scheduleUi → il mesure la frame complète.
      const t0 = telemetryActive() ? performance.now() : -1;
      // En dictée : la frappe ne doit pas être écrasée par le prochain
      // partial — le segment live se ré-ancre au curseur (hors
      // composition IME, sinon chaque lettre committerait l'autocorrect).
      this.mic.onUserInput(e as InputEvent);
      this.noteUserEdit(e as InputEvent);
      this.acStale = true;
      this.acDismissed = false;
      this.scheduleUi();
      if (t0 >= 0) {
        probeInputHandler(performance.now() - t0);
        probeInputPaint(t0);
      }
    });
    $("input").addEventListener("blur", () => {
      // délai pour laisser le click sur un item passer — le masquage
      // arme aussi acDismissed pour qu'un rAF en vol ne rouvre pas la
      // dropdown après coup.
      setTimeout(() => {
        $("cmd-ac").hidden = true;
        this.acDismissed = true;
      }, 150);
    });

    // ✕ du panneau d'inspection de la file (aussi cliqué par le back
    // Android via nav.rawCloseOverlay).
    $("qp-close").onclick = () => this.closeQueuePanel();

    // « Tout vider » : danger-armed (pattern ctxmenu/components) —
    // 1er clic arme le libellé de confirmation, 2e clic = queue_clear
    // (drain atomique côté démon ; le tour en cours n'est pas touché).
    const qpClear = $("qp-clear") as HTMLButtonElement;
    qpClear.onclick = () => {
      if (!app.current) return;
      if (!qpClear.classList.contains("armed")) {
        qpClear.classList.add("armed");
        qpClear.textContent = t("queue_clear_armed");
        this.qClearTimer = setTimeout(() => this.disarmQueueClear(), 2500);
        return;
      }
      this.disarmQueueClear();
      const sid = app.current.id;
      const conn = app.current.conn;
      void call<{ cleared: number; queue?: string[] }>("queue_clear", {
        session: sid,
        conn,
      })
        .then((r) => {
          if (Array.isArray(r.queue)) this.qSnapshot = r.queue;
          // File purgée : les échos pending de ces messages ne seront
          // jamais journalisés — les retirer (bulles orphelines).
          this.chat.clearEchoes(sid);
          this.refreshQueue();
        })
        .catch((e) => showErr(String(e)));
    };

    // Bouton « N lignes » : bascule replié ↔ étendu, puis redonne le focus
    // au champ pour continuer la frappe.
    $("btn-expand").onclick = () => {
      this.inpExpanded = !this.inpExpanded;
      this.fitInput(true);
      ($("input") as HTMLTextAreaElement).focus();
    };
    // Les caps dépendent de la hauteur du fil (resize, rotation mobile,
    // panneaux) — #chat la suit sans reflow côté fitInput.
    const chatEl = $("chat");
    this.chatH = chatEl?.clientHeight || window.innerHeight;
    this.chatW = chatEl?.clientWidth || window.innerWidth;
    if (chatEl) {
      new ResizeObserver(() => {
        const h = chatEl.clientHeight;
        const w = chatEl.clientWidth;
        if (h > 0 && (h !== this.chatH || w !== this.chatW)) {
          this.chatH = h;
          this.chatW = w;
          this.fitStale = true;
          this.fitInput();
        }
      }).observe(chatEl);
    }
    // Largeur du champ : #chat ne bouge pas quand les boutons du
    // composer (alt/mic/attach) apparaissent — #input-wrap, si.
    const wrapEl = $("input-wrap");
    let wrapW = wrapEl?.clientWidth ?? 0;
    if (wrapEl) {
      new ResizeObserver(() => {
        const w = wrapEl.clientWidth;
        if (w > 0 && w !== wrapW) {
          wrapW = w;
          this.fitStale = true;
          this.fitInput();
        }
      }).observe(wrapEl);
    }
    window.addEventListener("resize", () => {
      this.fitStale = true; // largeur changée → le contenu re-wrappé
      this.fitInput();
    });
    // Miroir autosize (repli sans field-sizing) : métriques texte
    // recopiées une fois — le padding reste porté par le CSS partagé.
    this.ghostEl = document.getElementById("input-ghost");
    if (!FIELD_SIZING) this.syncGhostStyle();
    this.fitInput();

    $("btn-cancel").onclick = () =>
      app.current && call("cancel", { session: app.current.id }).catch((e) => showErr(String(e)));

    // Reprise d'un agent mort : spawn + session/load peuvent durer
    // plusieurs secondes — le bouton montre l'attente, l'échec part en
    // toast persistant (dialogue fermé à ce stade).
    const resumeBtn = $("btn-resume") as HTMLButtonElement;
    resumeBtn.onclick = async () => {
      if (!app.current) return;
      resumeBtn.disabled = true;
      resumeBtn.classList.add("spin");
      try {
        await call("resume_session", { session: app.current.id });
        await this.chat.open(app.current.id);
      } catch (e) {
        showErr(String(e));
      } finally {
        resumeBtn.disabled = false;
        resumeBtn.classList.remove("spin");
      }
    };

    // « Contexte neuf » pendant un long chargement : abandonne la
    // session native (session/new, journal conservé) — issue explicite
    // quand le replay d'une très longue session est trop long (#271).
    const freshBtn = $("btn-fresh") as HTMLButtonElement;
    freshBtn.onclick = async () => {
      if (!app.current) return;
      freshBtn.disabled = true;
      freshBtn.classList.add("spin");
      try {
        await call("resume_session", { session: app.current.id, fresh: true });
        await this.chat.open(app.current.id);
      } catch (e) {
        showErr(String(e));
      } finally {
        freshBtn.disabled = false;
        freshBtn.classList.remove("spin");
      }
    };

    // 💤 = endort la session (libère le process ; réveil auto au prochain
    // envoi) — cachée du header sur desktop, utilisée par le menu ⋮ mobile.
    $("btn-sleep").onclick = () =>
      app.current &&
      call("sleep_session", { session: app.current.id })
        .then(() => this.refresh())
        .catch((e) => showErr(String(e)));

    // ⟳ = vrai rechargement : le cache de conversation est ignoré.
    $("btn-refresh").onclick = () => app.current && this.chat.open(app.current.id, true);

    // Double-clic sur le titre → renommage inline. La session est capturée
    // au début : app.current peut changer pendant l'édition (clic sur une
    // autre ligne → blur → done), et le RPC doit viser la session éditée.
    $("chat-title").addEventListener("dblclick", () => {
      if (!app.current) return;
      const sess = app.current;
      const tl = $("chat-title");
      const parent = tl.parentElement;
      const inp = document.createElement("input");
      inp.className = "title-edit";
      inp.value = sess.title;
      tl.replaceWith(inp);
      inp.focus();
      inp.select();
      let finished = false;
      const done = async (save: boolean) => {
        if (finished) return;
        finished = true;
        const v = inp.value.trim();
        // Restaurer le titre quoiqu'il arrive : si l'input a été détaché
        // par un chemin externe, replaceWith ne remettrait rien et le
        // header resterait sans #chat-title (setChatTitle en no-op).
        if (inp.isConnected) inp.replaceWith(tl);
        else if (parent && !tl.isConnected) parent.appendChild(tl);
        if (save && v && v !== sess.title) {
          sess.title = v;
          if (app.current?.id === sess.id) this.chat.setChatTitle();
          await call("rename_session", { session: sess.id, title: v, conn: sess.conn })
            .catch((e) => showErr(String(e)));
          void this.refresh();
        }
      };
      inp.addEventListener("keydown", (e) => {
        if (e.key === "Enter") done(true);
        if (e.key === "Escape") done(false);
      });
      inp.addEventListener("blur", () => done(true));
    });
  }
}
