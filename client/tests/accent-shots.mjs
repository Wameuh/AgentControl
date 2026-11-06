// Shots des réglages d'apparence : 5 accents × 2 intensités (douce/forte)
// × dark/light + fond animé. Vérifie au passage les valeurs calculées de
// --accent, les contrastes (accent/--bg ≥3:1, bulle user ≥4.5:1, puits de
// code ≥4.5:1) et la persistance (localStorage → classes :root au
// rechargement).
// Usage : npm run build && node tests/accent-shots.mjs
import puppeteer from "puppeteer-core";
import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";

const PORT = Number(process.env.SHOTS_PORT || 4173);
const server = spawn("npx", ["vite", "preview", "--port", String(PORT), "--strictPort"], { cwd: process.cwd(), stdio: "ignore" });
await new Promise((r) => setTimeout(r, 1800));
const b = await puppeteer.launch({
  executablePath: process.env.CHROME || "/usr/bin/chromium",
  headless: "new",
  args: ["--no-sandbox"],
});
const p = await b.newPage();
await p.setViewport({ width: 1280, height: 800, deviceScaleFactor: 1 });
await p.evaluateOnNewDocument(readFileSync("tests/tauri-mock.js", "utf8"));

const shot = async (n, ms = 350) => {
  await new Promise((r) => setTimeout(r, ms));
  await p.screenshot({ path: `shots/${n}.png` });
  console.log("->", n);
};
// localStorage posé dans la page puis reload → vérifie l'application au boot
// (persistance) : le chemin réel d'un rechargement d'app.
const boot = async (prefs) => {
  await p.evaluate((pr) => {
    localStorage.clear();
    for (const [k, v] of Object.entries(pr)) localStorage.setItem(k, v);
  }, prefs);
  await p.reload({ waitUntil: "networkidle0" });
  await new Promise((r) => setTimeout(r, 400));
};
const probe = () =>
  p.evaluate(() => {
    const cs = getComputedStyle(document.documentElement);
    const code = document.querySelector(".md .md-code");
    const ccs = code ? getComputedStyle(code) : null;
    const user = document.querySelector(".msg.user");
    const ucs = user ? getComputedStyle(user) : null;
    // Résolution des vars de surface (color-mix non évalué par
    // getPropertyValue) — même technique de sonde que core/chrome.ts.
    const el = document.createElement("i");
    el.style.display = "none";
    document.body.appendChild(el);
    const pick = (v) => {
      el.style.color = `var(${v})`;
      return getComputedStyle(el).color;
    };
    const bg = pick("--bg");
    const accentRgb = pick("--accent");
    el.remove();
    return {
      accent: cs.getPropertyValue("--accent").trim(),
      accentRgb,
      bg,
      cls: document.documentElement.className,
      anim: window.MeshBackground?.isAnimated?.(),
      mood: window.MeshBackground?.getMood?.(),
      checkbox: document.getElementById("s-bganim")?.checked,
      code: ccs ? { bg: ccs.backgroundColor, fg: ccs.color } : null,
      user: ucs ? { bg: ucs.backgroundColor, fg: ucs.color } : null,
    };
  });
const openSettings = async () => {
  await p.evaluate(() => document.querySelector("#tb-settings")?.click());
  await p.waitForSelector("#settings-close", { visible: true });
};

const EXPECT = {
  dark: {
    soft: { sage: "#cdb68c", gold: "#e0c996", grey: "#c6cdd5", violet: "#c4a9dc", red: "#f2a79a" },
    strong: { sage: "#bed34a", gold: "#f2b01e", grey: "#8fbcef", violet: "#b06ef0", red: "#f95a3c" },
  },
  light: {
    soft: { sage: "#8a6f3e", gold: "#8a6a1a", grey: "#5d6b78", violet: "#6b4d8c", red: "#a03d33" },
    strong: { sage: "#5f7310", gold: "#986000", grey: "#2f6cb2", violet: "#7d28d4", red: "#cf2a12" },
  },
};
// Teinte « agent » du mesh par mood quand l'accent est le défaut (preset
// conservé : bicolore sable/sauge = identité). Sous accent choisi, elle
// doit au contraire dériver de l'accent (famille de teinte, jamais vert).
const AGENT_PRESET = { dark: [147, 173, 140], light: [88, 118, 78] };
// Même convention que rgbToHsl (core/chrome.ts) : h 0-360, s/l 0-100.
const hslOf = ([r, g, b]) => {
  r /= 255; g /= 255; b /= 255;
  const max = Math.max(r, g, b), min = Math.min(r, g, b);
  const l = (max + min) / 2;
  if (max === min) return { h: 0, s: 0, l: l * 100 };
  const d = max - min;
  const s = l > 0.5 ? d / (2 - max - min) : d / (max + min);
  const h = max === r ? ((g - b) / d + (g < b ? 6 : 0))
    : max === g ? (b - r) / d + 2 : (r - g) / d + 4;
  return { h: h * 60, s: s * 100, l: l * 100 };
};
const parseHex = (s) => [1, 3, 5].map((i) => parseInt(s.slice(i, i + 2), 16));
// getComputedStyle sérialise en « rgb(r, g, b) » pour les couleurs
// littérales mais « color(srgb r g b) » (canaux 0-1) pour les
// color-mix — parser les deux formes.
const parseRgb = (s) => {
  const m = s.match(/color\(srgb ([\d.]+) ([\d.]+) ([\d.]+)/);
  if (m) return [1, 2, 3].map((i) => Math.round(parseFloat(m[i]) * 255));
  return (s.match(/[\d.]+/g) ?? []).slice(0, 3).map(Number);
};
const hueDist = (a, b) => Math.min(Math.abs(a - b), 360 - Math.abs(a - b));
// Ratio de contraste WCAG (luminance relative) — AA texte = 4.5:1.
const lum = ([r, g, b]) => {
  const f = (v) => ((v /= 255) <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4);
  return 0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b);
};
const contrast = (c1, c2) => {
  const [a, b] = [lum(c1), lum(c2)].sort((x, y) => y - x);
  return (a + 0.05) / (b + 0.05);
};
let fail = 0;
const check = (label, got, want) => {
  const ok = got === want;
  if (!ok) fail++;
  console.log(`${ok ? "ok  " : "FAIL"} ${label}: ${got} (attendu ${want})`);
};
// En thème clair (« paper ») les couleurs de lumière sont SOUSTRAITES
// de la base (sgn<0 dans mesh-bg#shade) : la teinte visible des
// facettes est le résidu `base − light`, pas la lumière elle-même —
// c'est elle qui doit rester dans la famille de l'accent (sinon
// complémentaire : violet → facettes vertes, c'était le bug #81).
const visible = (theme, base, c) =>
  theme === "light" ? base.map((b, i) => Math.max(0, b - c[i])) : c;
const checkAgent = (theme, accent, int, st) => {
  const agent = st.mood?.agent?.map(Math.round);
  const base = st.mood?.base?.map(Math.round);
  if (!agent || !base) { check(`mood agent ${theme}/${accent}/${int}`, "absent", "rgb"); return; }
  if (accent === "sage" && int === "soft") {
    check(`mood agent ${theme}/${accent}/${int}`, agent.join(","), AGENT_PRESET[theme].join(","));
    return;
  }
  const a = hslOf(parseHex(st.accent)), g = hslOf(visible(theme, base, agent));
  // Famille de teinte : Δhue ≤ 70°, ou quasi-achromatique (gris → gris).
  check(`mood agent ${theme}/${accent}/${int} famille`, hueDist(a.h, g.h) <= 70 || g.s < 20, true);
};
// Lumière principale : en dark elle *est* l'accent ; en light, son
// résidu sur le papier doit rester dans sa famille (violet → violet
// sombre, jamais vert). Accent par défaut en douce : preset non dérivé
// — skip (sauge forte est dérivée comme les autres).
const checkLight = (theme, accent, int, st) => {
  if (accent === "sage" && int === "soft") return;
  const light = st.mood?.light?.map(Math.round);
  const base = st.mood?.base?.map(Math.round);
  if (!light || !base) { check(`mood light ${theme}/${accent}/${int}`, "absent", "rgb"); return; }
  const a = hslOf(parseHex(st.accent)), g = hslOf(visible(theme, base, light));
  check(`mood light ${theme}/${accent}/${int} famille`, hueDist(a.h, g.h) <= 70 || g.s < 20, true);
};
// Puits de code (.md-code) : le couple fond/texte doit respecter WCAG
// AA dans CHAQUE combo — --code-bg était un puits sombre jamais remappé
// en thème clair (texte --ink sombre dessus = illisible, bug #83).
const checkCode = (theme, accent, int, st) => {
  if (!st.code) { check(`code ${theme}/${accent}/${int}`, "absent", ".md-code"); return; }
  const r = contrast(parseRgb(st.code.bg), parseRgb(st.code.fg));
  check(`code ${theme}/${accent}/${int} contraste ≥4.5`, r >= 4.5, true);
};
// Accent sur --bg : seuil « composant interactif » WCAG (≥3:1) — la
// version forte doit rester lisible sur les surfaces qu'elle teinte.
const checkAccentBg = (theme, accent, int, st) => {
  const r = contrast(parseRgb(st.accentRgb), parseRgb(st.bg));
  check(`accent/bg ${theme}/${accent}/${int} ≥3`, r >= 3, true);
};
// Bulle utilisateur : texte sur fond accent — AA texte (≥4.5:1) exigé
// pour la variante forte ; un accent flashy peut exiger la bascule
// clair/foncé de --msg-user-fg. En douce on ne touche pas aux couleurs
// actuelles (défaut inchangé) : sage/gold clair sont à ~4.2 — plancher
// de non-régression 4:1.
const checkUser = (theme, accent, int, st) => {
  if (!st.user) { check(`user ${theme}/${accent}/${int}`, "absent", ".msg.user"); return; }
  const r = contrast(parseRgb(st.user.bg), parseRgb(st.user.fg));
  const min = int === "strong" ? 4.5 : 4;
  check(`user ${theme}/${accent}/${int} contraste ≥${min}`, r >= min, true);
};

await p.goto(`http://localhost:${PORT}/#desktop`, { waitUntil: "networkidle0" });
await new Promise((r) => setTimeout(r, 400));
// Connexion mock + ouverture de la 1re session → les shots chat montrent
// de vraies bulles user (contraste --msg-user-fg sur --msg-user-bg).
const connectOnce = async () => {
  // La vue connexion peut être déjà auto-ouverte (accueil sans connexion)
  // — #btn-connect est un toggle : ne cliquer que si elle est cachée.
  await p.evaluate(() => {
    if (document.getElementById("conn-view")?.hidden)
      document.querySelector("#btn-connect")?.click();
  });
  await p.waitForSelector("#c-go", { visible: true });
  await p.evaluate(() => document.querySelector("#c-go")?.click());
  await p.waitForSelector(".sess", { visible: true });
  await p.evaluate(() => document.querySelector(".sess")?.click());
  await new Promise((r) => setTimeout(r, 500));
};
// Matrice complète : à chaque (thème, accent), les deux intensités sont
// testées à la suite — la version forte doit aussi différer de la douce
// (accent ET surface), c'est le but du réglage.
for (const theme of ["dark", "light"]) {
  for (const accent of ["sage", "gold", "grey", "violet", "red"]) {
    let soft = null;
    for (const int of ["soft", "strong"]) {
      // boot() recharge la page → connexion mock à refaire à chaque itération.
      await boot({
        "ac-theme": theme,
        "ac-accent": accent,
        "ac-accent-strong": int === "strong" ? "1" : "0",
      });
      await connectOnce();
      const st = await probe();
      const tag = `${theme}/${accent}/${int}`;
      check(`--accent ${tag}`, st.accent, EXPECT[theme][int][accent]);
      check(`class theme- ${tag}`, st.cls.includes(`theme-${accent}`), accent !== "sage");
      check(`class strong ${tag}`, st.cls.includes("accent-strong"), int === "strong");
      checkAgent(theme, accent, int, st);
      checkLight(theme, accent, int, st);
      checkCode(theme, accent, int, st);
      checkAccentBg(theme, accent, int, st);
      checkUser(theme, accent, int, st);
      if (int === "soft") {
        soft = st;
      } else {
        check(`accent ≠ douce ${theme}/${accent}`, st.accent !== soft.accent, true);
        check(`bg ≠ douce ${theme}/${accent}`, st.bg !== soft.bg, true);
      }
      await openSettings();
      await shot(`30-${theme}-${accent}-${int}-settings`);
      await p.evaluate(() => document.querySelector("#settings-close")?.click());
      await shot(`31-${theme}-${accent}-${int}-chat`);
    }
  }
}

// Transition en session (sans reload) : rouge → sauge doit rendre le
// preset au mood.agent, sauge → rouge le re-dériver — couvre le reset de
// meshColors.agentTint que le boot à chaque combo n'exerce pas.
await boot({ "ac-theme": "light", "ac-accent": "red" });
await connectOnce();
await openSettings();
await p.select("#s-accent", "sage");
await new Promise((r) => setTimeout(r, 300));
checkAgent("light", "sage", "soft", await probe());
await p.select("#s-accent", "red");
await new Promise((r) => setTimeout(r, 300));
checkAgent("light", "red", "soft", await probe());
// Bascule d'intensité en session : sauge douce (preset) → sauge forte
// (teinte dérivée) — la classe accent-strong s'applique sans reload.
await p.select("#s-accent", "sage");
await p.select("#s-accent-int", "strong");
await new Promise((r) => setTimeout(r, 300));
checkAgent("light", "sage", "strong", await probe());
await p.select("#s-accent-int", "soft");
await new Promise((r) => setTimeout(r, 300));
checkAgent("light", "sage", "soft", await probe());
await p.evaluate(() => document.querySelector("#settings-close")?.click());

// Fond animé off : rendu statique + case décochée, persistant au reload.
await boot({ "ac-bganim": "0" });
check("anim off (pref)", (await probe()).anim, false);
await openSettings();
check("anim off (checkbox)", (await probe()).checkbox, false);
await shot("32-bganim-off");
// Re-cocher via l'UI → setAnimated(true) + pref persistée.
await p.evaluate(() => document.querySelector("#s-bganim")?.click());
await new Promise((r) => setTimeout(r, 200));
check("anim on (pref)", await p.evaluate(() => localStorage.getItem("ac-bganim")), "1");

// prefers-reduced-motion sans préférence stockée → anim off par défaut.
await p.evaluate(() => localStorage.clear());
await p.emulateMediaFeatures([{ name: "prefers-reduced-motion", value: "reduce" }]);
await p.reload({ waitUntil: "networkidle0" });
await new Promise((r) => setTimeout(r, 400));
check("reduced-motion → anim off", (await probe()).anim, false);

await b.close();
server.kill();
console.log(fail ? `${fail} échec(s)` : "done");
process.exit(fail ? 1 : 0);
