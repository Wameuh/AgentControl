// E2E « ouverture des liens » (#372) — clic sur un <a href> du fil :
//   http(s)/mailto → invoke `open_url_ext` (plus le plugin muet) ;
//   invoke rejeté → toast d'avertissement + lien copié ;
//   href non externe (file:, agentcontrol:, relatif) → bloqué avec
//   toast + copie, JAMAIS passé à l'opener ni de navigation webview.
// Usage : npm run build && node tests/link-open-e2e.mjs
//   (chromium via $CHROMIUM ou /usr/bin/chromium)
import puppeteer from "puppeteer-core";
import { startVitePreview } from "./lib/vite-preview.mjs";

const CHROMIUM = process.env.CHROMIUM ?? "/usr/bin/chromium";
const { server, base: BASE } = await startVitePreview();

const MOCK = `(function () {
  // Espions : invoke open_url_ext journalisé, writeText clipboard
  // remplacé par un enregistreur (pas de permission clipboard en
  // headless — le repli execCommand de copyText ne serait pas visible).
  window.__openCalls = [];
  window.__clip = null;
  window.__openUrlFail = false;
  if (navigator.clipboard) {
    navigator.clipboard.writeText = (t) => {
      window.__clip = String(t);
      return Promise.resolve();
    };
  }
  const SESSIONS = [
    { id: "s1", conn: "local", agent: "devin", title: "Liens test",
      cwd: "/home/agent/api", created_at: "2026-10-20T10:00:00Z",
      available_modes: [], available_models: [], available_commands: [],
      config_options: [], state: "idle" },
  ];
  const EVENTS = [
    { seq: 1, type: "agent_text",
      text: "Doc : [le site](https://example.com/docs) et https://example.com/nue en vrac." },
  ];
  function rpc(method, args) {
    switch (method) {
      case "list_sessions": return SESSIONS;
      case "list_agents": return [{ id: "devin", name: "Devin" }];
      case "session_info": return { meta: SESSIONS[0], pending: [], alive: true, last_seq: EVENTS.length };
      case "events": return { events: EVENTS, last_seq: EVENTS.length };
      case "daemon_status": return { version: "9.9.9", sessions: 1 };
      case "pr_list": return [];
      default: return null;
    }
  }
  window.__TAURI_INTERNALS__ = {
    metadata: { currentWindow: { label: "main" }, currentWebview: { label: "main" } },
    transformCallback(cb) { return ++cbId; },
    runCallback(id, data) {},
    convertFileSrc(p) { return p; },
    invoke(cmd, args) {
      const p = Promise.resolve();
      if (cmd === "open_url_ext") {
        window.__openCalls.push(args?.url);
        return window.__openUrlFail
          ? p.then(() => { throw new Error("aucun opener disponible"); })
          : p.then(() => null);
      }
      if (cmd === "platform") return p.then(() => "linux");
      if (cmd === "app_version") return p.then(() => "9.9.9");
      if (cmd === "plugin:event|listen") return p.then(() => ++cbId);
      if (cmd.startsWith("plugin:")) return p.then(() => null);
      switch (cmd) {
        case "connect": return p.then(() => ({ version: "9.9.9", sessions: 1, servers: 1 }));
        case "call": return p.then(() => rpc(args.method, args.params ?? args));
        case "subscribe": return p.then(() => null);
        case "other_instances": return p.then(() => []);
        case "saved_connections": return p.then(() => []);
        case "ssh_hosts": return p.then(() => []);
        case "ssh_pubkey": return p.then(() => "ssh-ed25519 AAAA x");
        default: return p.then(() => null);
      }
    },
  };
  let cbId = 0;
})();`;

const b = await puppeteer.launch({ executablePath: CHROMIUM, headless: "new", args: ["--no-sandbox"] });
const p = await b.newPage();
await p.setViewport({ width: 1280, height: 860 });
p.on("console", (m) => { if (m.type() === "error") console.log("[page]", m.text()); });
await p.evaluateOnNewDocument(MOCK);

let pass = 0, fail = 0;
const check = (name, ok, extra = "") => {
  if (ok) { pass++; console.log("PASS", name); }
  else { fail++; console.log("FAIL", name, extra); }
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const click = (sel) => p.evaluate((s) => document.querySelector(s)?.click(), sel);
const ev = (fn, ...args) => p.evaluate(fn, ...args);

await p.goto(`${BASE}/`, { waitUntil: "networkidle0" });
await p.waitForSelector("#btn-connect", { timeout: 15000 });
await click("#btn-connect"); await sleep(300);
await p.type("#c-host", "me@pi5.local"); await click("#c-go"); await sleep(900);
await p.evaluate(() => [...document.querySelectorAll(".sess")].find((s) => s.textContent.includes("Liens test"))?.click());
await sleep(900);

// Les deux formes de lien sont rendues : markdown [t](url) → <a>,
// URL nue → <a class="ulink">.
const links = await ev(() => [...document.querySelectorAll("#events a[href]")].map((a) => a.getAttribute("href")));
check("rendu : 2 liens http(s) dans le fil", links.filter((h) => /^https?:/.test(h)).length === 2, JSON.stringify(links));

// --- clic nominal → open_url_ext reçoit l'URL ---
await ev(() => [...document.querySelectorAll("#events a[href]")].find((a) => a.getAttribute("href") === "https://example.com/docs")?.click());
await sleep(300);
check("clic md-lien → open_url_ext(https://example.com/docs)",
  await ev(() => window.__openCalls.includes("https://example.com/docs")),
  JSON.stringify(await ev(() => window.__openCalls)));

await ev(() => [...document.querySelectorAll("#events a[href]")].find((a) => a.getAttribute("href") === "https://example.com/nue")?.click());
await sleep(300);
check("clic ulink → open_url_ext(https://example.com/nue)",
  await ev(() => window.__openCalls.includes("https://example.com/nue")));

// mailto passe aussi par l'opener (allowlist http/https/mailto).
await ev(() => {
  const a = document.createElement("a");
  a.href = "mailto:test@example.com";
  a.textContent = "mail";
  document.querySelector("#events").appendChild(a);
  a.click();
});
await sleep(300);
check("clic mailto → open_url_ext(mailto:…)",
  await ev(() => window.__openCalls.includes("mailto:test@example.com")),
  JSON.stringify(await ev(() => window.__openCalls)));

// --- invoke rejeté → toast warn + lien copié ---
await ev(() => { window.__openUrlFail = true; window.__clip = null; });
await ev(() => [...document.querySelectorAll("#events a[href]")].find((a) => a.getAttribute("href") === "https://example.com/docs")?.click());
await sleep(400);
check("échec opener → toast warn visible",
  await ev(() => {
    const t = [...document.querySelectorAll(".toast.t-warn")].find((x) => x.getClientRects().length > 0);
    return !!t && /link|lien/i.test(t.textContent);
  }));
check("échec opener → URL copiée dans le presse-papiers",
  await ev(() => window.__clip === "https://example.com/docs"),
  String(await ev(() => window.__clip)));

// --- href non externe → bloqué, feedback, jamais d'invoke opener ---
const before = await ev(() => window.__openCalls.length);
await ev(() => {
  const a = document.createElement("a");
  a.href = "agentcontrol://session/s1";
  a.textContent = "interne";
  document.querySelector("#events").appendChild(a);
  a.click();
});
await sleep(400);
check("lien non externe → aucun appel opener",
  await ev(() => window.__openCalls.length) === before);
check("lien non externe → toast warn + href copié",
  await ev(() => {
    const t = [...document.querySelectorAll(".toast.t-warn")].some((x) => x.getClientRects().length > 0 && /link|lien/i.test(x.textContent));
    return t && window.__clip === "agentcontrol://session/s1";
  }),
  String(await ev(() => window.__clip)));

// javascript: aussi bloqué (jamais de navigation).
await ev(() => {
  const a = document.createElement("a");
  a.href = "javascript:alert(1)";
  a.textContent = "js";
  document.querySelector("#events").appendChild(a);
  a.click();
});
await sleep(300);
check("javascript: → toujours aucun appel opener",
  await ev(() => window.__openCalls.length) === before);

console.log(`\n${pass} PASS, ${fail} FAIL`);
await b.close();
server.kill();
process.exit(fail ? 1 : 0);
