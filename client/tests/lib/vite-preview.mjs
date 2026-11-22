// Lancement de `vite preview` partagé par les smokes puppeteer.
//
// Pourquoi un poll HTTP et pas du parsing de la sortie vite :
// - Sur le runner GHA, vite preview reste muet >60 s (ni stdout ni
//   stderr, ni exit) — que ce soit via npx ou node direct, la sortie
//   n'est PAS un contrat fiable.
// - Ce qu'on veut vraiment savoir, c'est que le serveur répond en HTTP.
//   On choisit nous-mêmes un port libre (listen(0) puis close), on
//   impose --strictPort et on sonde jusqu'à réponse ou deadline.
//
// La sortie du child est accumulée et recrachée dans les erreurs pour
// garder le diagnostic (crash vite, config, etc.).
//
// `--host 127.0.0.1` imposé : sans quoi vite n'écoute que sur ::1 sur
// certaines machines — et « localhost » ne résout alors plus pareil
// selon la stack (curl/chromium : ::1 OK ; node fetch : 127.0.0.1 →
// refusé). IP littérale des deux côtés = zéro DNS.

import { spawn } from "node:child_process";
import { createServer } from "node:net";

const freePort = () =>
  new Promise((res, rej) => {
    const s = createServer();
    s.once("error", rej);
    s.listen(0, "127.0.0.1", () => {
      const p = s.address().port;
      s.close(() => res(p));
    });
  });

export const startVitePreview = async () => {
  const port = await freePort();
  const base = `http://127.0.0.1:${port}`;
  const server = spawn(
    "node",
    ["node_modules/vite/bin/vite.js", "preview", "--host", "127.0.0.1", "--port", String(port), "--strictPort"],
    { stdio: ["ignore", "pipe", "pipe"], detached: true },
  );
  let out = "";
  server.stdout.on("data", (d) => { out += d; });
  server.stderr.on("data", (d) => { out += d; });
  const tail = () => out.trim() || "(vide)";
  await new Promise((res, rej) => {
    const deadline = Date.now() + 60000;
    const timer = setInterval(async () => {
      try {
        // 404 OK aussi : ce qui compte c'est que le socket réponde.
        await fetch(base, { signal: AbortSignal.timeout(2000) });
        clearInterval(timer);
        res();
      } catch {
        if (Date.now() > deadline) {
          clearInterval(timer);
          rej(new Error(`vite preview ne répond pas sur ${base} — sortie: ${tail()}`));
        }
      }
    }, 300);
    server.on("error", (e) => {
      clearInterval(timer);
      rej(new Error(`vite preview: spawn impossible — ${e.message}`));
    });
    server.on("exit", (code) => {
      clearInterval(timer);
      rej(new Error(`vite preview: exit ${code} — sortie: ${tail()}`));
    });
  });
  // Kill de groupe : vite peut forker (esbuild) — detached:true en fait
  // le leader, -pid balaie tout même si le test crash en cours de route.
  process.on("exit", () => { try { process.kill(-server.pid, "SIGKILL"); } catch {} });
  return { server, base };
};
