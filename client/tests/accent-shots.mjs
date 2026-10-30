// Shots des réglages d'apparence : 5 accents × dark/light + fond animé.
// Vérifie au passage les valeurs calculées de --accent et la persistance
// (localStorage → classes :root au rechargement).
// Usage : npm run build && node tests/accent-shots.mjs
import puppeteer from "puppeteer-core";
import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";

const server = spawn("npx", ["vite", "preview", "--port", "4173", "--strictPort"], { cwd: process.cwd(), stdio: "ignore" });
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
    return {
      accent: cs.getPropertyValue("--accent").trim(),
      cls: document.documentElement.className,
      anim: window.MeshBackground?.isAnimated?.(),
      mood: window.MeshBackground?.getMood?.(),
      checkbox: document.getElementById("s-bganim")?.checked,
    };
  });
const openSettings = async () => {
  await p.click("#tb-settings");
  await new Promise((r) => setTimeout(r, 300));
};

const EXPECT = {
  dark: { sage: "#cdb68c", gold: "#e0c996", grey: "#c6cdd5", violet: "#c4a9dc", red: "#f2a79a" },
  light: { sage: "#8a6f3e", gold: "#8a6a1a", grey: "#5d6b78", violet: "#6b4d8c", red: "#a03d33" },
};
// Teinte « agent » du mesh par mood quand l'accent est le défaut (preset
// conservé : bicolore sable/sauge = identité). Sous accent choisi, elle
// doit au contraire dériver de l'accent (famille de teinte, jamais vert).
const AGENT_PRESET = { dark: [147, 173, 140], light: [88, 118, 78] };
const hslOf = ([r, g, b]) => {
  r /= 255; g /= 255; b /= 255;
  const max = Math.max(r, g, b), min = Math.min(r, g, b);
  const l = (max + min) / 2;
  if (max === min) return { h: 0, s: 0, l };
  const d = max - min;
  const s = l > 0.5 ? d / (2 - max - min) : d / (max + min);
  const h = max === r ? ((g - b) / d + (g < b ? 6 : 0))
    : max === g ? (b - r) / d + 2 : (r - g) / d + 4;
  return { h: h * 60, s, l };
};
const parseHex = (s) => [1, 3, 5].map((i) => parseInt(s.slice(i, i + 2), 16));
const hueDist = (a, b) => Math.min(Math.abs(a - b), 360 - Math.abs(a - b));
let fail = 0;
const check = (label, got, want) => {
  const ok = got === want;
  if (!ok) fail++;
  console.log(`${ok ? "ok  " : "FAIL"} ${label}: ${got} (attendu ${want})`);
};
const checkAgent = (theme, accent, st) => {
  const agent = st.mood?.agent?.map(Math.round);
  if (!agent) { check(`mood agent ${theme}/${accent}`, "absent", "rgb"); return; }
  if (accent === "sage") {
    check(`mood agent ${theme}/${accent}`, agent.join(","), AGENT_PRESET[theme].join(","));
    return;
  }
  const a = hslOf(parseHex(st.accent)), g = hslOf(agent);
  if (a.s < 0.15) check(`mood agent ${theme}/${accent} désaturé`, g.s < 0.2, true);
  else check(`mood agent ${theme}/${accent} Δhue`, hueDist(a.h, g.h) <= 70, true);
};

await p.goto("http://localhost:4173/#desktop", { waitUntil: "networkidle0" });
await new Promise((r) => setTimeout(r, 400));
// Connexion mock + ouverture de la 1re session → les shots chat montrent
// de vraies bulles user (contraste --msg-user-fg sur --msg-user-bg).
const connectOnce = async () => {
  await p.click("#btn-connect");
  await new Promise((r) => setTimeout(r, 300));
  await p.click("#c-go");
  await new Promise((r) => setTimeout(r, 700));
  await p.click(".sess");
  await new Promise((r) => setTimeout(r, 500));
};
for (const theme of ["dark", "light"]) {
  for (const accent of ["sage", "gold", "grey", "violet", "red"]) {
    // boot() recharge la page → connexion mock à refaire à chaque itération.
    await boot({ "ac-theme": theme, "ac-accent": accent });
    await connectOnce();
    const st = await probe();
    check(`--accent ${theme}/${accent}`, st.accent, EXPECT[theme][accent]);
    check(`class ${theme}/${accent}`, st.cls.includes(`theme-${accent}`) || accent === "sage", true);
    checkAgent(theme, accent, st);
    await openSettings();
    await shot(`30-${theme}-${accent}-settings`);
    await p.click("#settings-close");
    await shot(`31-${theme}-${accent}-chat`);
  }
}

// Fond animé off : rendu statique + case décochée, persistant au reload.
await boot({ "ac-bganim": "0" });
check("anim off (pref)", (await probe()).anim, false);
await openSettings();
check("anim off (checkbox)", (await probe()).checkbox, false);
await shot("32-bganim-off");
// Re-cocher via l'UI → setAnimated(true) + pref persistée.
await p.click("#s-bganim");
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
