// @ts-nocheck
/*
 * Website adaptation of the presentation renderer in assets/theme/source/.
 * Fixed world coordinates: resizing reveals more terrain without scaling it.
 * Low-poly mesh background.
 * A jittered triangle grid, flat-shaded by point lights. The main light follows
 * the pointer and drifts on its own when the pointer is idle. Extra "agent"
 * lights wander independently. Only owns the canvas; the deck drives it through
 * window.MeshBackground.setMood().
 */
(() => {
  "use strict";

  const canvas = document.getElementById("mesh-bg");
  if (!canvas || !canvas.getContext) return;
  const ctx = canvas.getContext("2d", { alpha: false });
  const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)");
  // « Fond animé » (réglage Paramètres, ac-bganim) : préférence persistée —
  // à défaut, on respecte prefers-reduced-motion. Android : toujours
  // statique (batterie), la préférence ne s'y applique pas.
  const IS_ANDROID = /Android/i.test(navigator.userAgent);
  const storedAnim = localStorage.getItem("ac-bganim");
  let animOn = storedAnim === null ? !reducedMotion.matches : storedAnim === "1";
  const animated = () => animOn && !IS_ANDROID;

  const MOODS = {
    sand: { base: [18, 22, 20], light: [205, 182, 140], agent: [147, 173, 140] },
    ember: { base: [21, 19, 18], light: [197, 111, 106], agent: [147, 173, 140] },
    lichen: { base: [16, 21, 18], light: [160, 186, 150], agent: [205, 182, 140] },
    night: { base: [12, 15, 20], light: [116, 138, 172], agent: [147, 173, 140] },
    // Thème clair : même maillage, fond papier, accents nettement plus
    // foncés que la base → lumière "négative" (facettes sombres, sobre)
    paper: { base: [233, 229, 216], light: [160, 128, 78], agent: [88, 118, 78], dark: true },
  };
  const MAX_AGENTS = 4;
  const IDLE_AFTER_MS = 3000;

  let width = 0;
  let height = 0;
  let dpr = 1;
  const cell = 88;
  const fieldWidth = window.innerWidth;
  const fieldHeight = window.innerHeight;
  let initialized = false;
  let sceneTime = 0;
  let vertices = [];
  let triangles = [];
  let running = false;
  let frameId = 0;
  let lastTime = 0;

  const mood = { base: [...MOODS.sand.base], light: [...MOODS.sand.light], agent: [...MOODS.sand.agent], intensity: 1, dark: false };
  let target = { ...MOODS.sand, intensity: 1 };
  let agentCount = 0;

  const pointer = { x: 0, y: 0, active: false, last: -Infinity };
  const mainLight = { x: 0, y: 0 };
  const agentLights = Array.from({ length: MAX_AGENTS }, (_, i) => ({ x: 0, y: 0, alpha: 0, seed: 1.3 + i * 1.9 }));

  const seededRandom = (seed) => () => {
    seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };

  const drift = (time, seed, out) => {
    out.x = fieldWidth * (0.5 + 0.38 * Math.sin(time * 0.00011 * (1 + seed * 0.13) + seed * 2.1));
    out.y = fieldHeight * (0.48 + 0.34 * Math.sin(time * 0.00017 * (1 + seed * 0.07) + seed * 1.3 + 1.1));
  };

  function build() {
    dpr = Math.min(window.devicePixelRatio || 1, 1.5);
    width = window.innerWidth;
    height = window.innerHeight;
    // Keep CSS pixels and world pixels 1:1 even during an interactive resize.
    canvas.style.width = `${width}px`;
    canvas.style.height = `${height}px`;
    canvas.width = Math.round(width * dpr);
    canvas.height = Math.round(height * dpr);

    const cols = Math.ceil(width / cell) + 2;
    const rows = Math.ceil(height / cell) + 2;
    vertices = [];
    for (let r = 0; r <= rows; r += 1) {
      for (let c = 0; c <= cols; c += 1) {
        const random = seededRandom(20261012 ^ Math.imul(c + 4096, 73856093) ^ Math.imul(r + 4096, 19349663));
        vertices.push({
          x: (c - 1) * cell + (random() - 0.5) * cell * 0.72,
          y: (r - 1) * cell + (random() - 0.5) * cell * 0.72,
          z0: (random() - 0.5) * cell * 0.6,
          amp: cell * (0.05 + random() * 0.1),
          phase: random() * Math.PI * 2,
          speed: 0.00022 + random() * 0.00034,
          z: 0,
        });
      }
    }
    vertices.forEach((v) => { v.z = sceneTime ? v.z0 + Math.sin(sceneTime * v.speed + v.phase) * v.amp : v.z0; });

    const at = (r, c) => vertices[r * (cols + 1) + c];
    triangles = [];
    for (let r = 0; r < rows; r += 1) {
      for (let c = 0; c < cols; c += 1) {
        const a = at(r, c);
        const b = at(r, c + 1);
        const d = at(r + 1, c);
        const e = at(r + 1, c + 1);
        const diagonal = seededRandom(20261012 ^ Math.imul(c + 4096, 83492791) ^ Math.imul(r + 4096, 2971215073));
        if (diagonal() < 0.5) triangles.push([a, b, e], [a, e, d]);
        else triangles.push([a, b, d], [b, e, d]);
      }
    }

    if (!initialized) {
      mainLight.x = fieldWidth * 0.66;
      mainLight.y = fieldHeight * 0.34;
      agentLights.forEach((light, i) => drift(i * 4000, light.seed, light));
      initialized = true;
    }
  }

  function shade(tri, lights) {
    const [p, q, s] = tri;
    const ux = q.x - p.x, uy = q.y - p.y, uz = q.z - p.z;
    const vx = s.x - p.x, vy = s.y - p.y, vz = s.z - p.z;
    let nx = uy * vz - uz * vy;
    let ny = uz * vx - ux * vz;
    let nz = ux * vy - uy * vx;
    const length = Math.hypot(nx, ny, nz) || 1;
    nx /= length; ny /= length; nz /= length;
    if (nz < 0) { nx = -nx; ny = -ny; nz = -nz; }

    const cx = (p.x + q.x + s.x) / 3;
    const cy = (p.y + q.y + s.y) / 3;
    const cz = (p.z + q.z + s.z) / 3;
    const facet = (nx * 0.7 - ny * 0.5) * 9;
    let r = mood.base[0] + facet;
    let g = mood.base[1] + facet;
    let b = mood.base[2] + facet;

    for (let i = 0; i < lights.length; i += 1) {
      const light = lights[i];
      const dx = light.x - cx, dy = light.y - cy, dz = light.z - cz;
      const distance = Math.hypot(dx, dy, dz);
      const lambert = Math.max(0, (nx * dx + ny * dy + nz * dz) / distance);
      const falloff = 1 / (1 + (distance * distance) / (light.radius * light.radius));
      const sgn = mood.dark ? -1.35 : 1;
      const k = sgn * lambert * lambert * falloff * light.power;
      r += light.color[0] * k;
      g += light.color[1] * k;
      b += light.color[2] * k;
    }
    return [r, g, b];
  }

  function draw() {
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    // Reset edge antialiasing each frame, including the first frame after resize.
    ctx.fillStyle = `rgb(${mood.base[0] | 0},${mood.base[1] | 0},${mood.base[2] | 0})`;
    ctx.fillRect(0, 0, width, height);
    const diagonal = Math.hypot(fieldWidth, fieldHeight);
    const lights = [{
      x: mainLight.x, y: mainLight.y, z: cell * 2.4,
      radius: diagonal * 0.3, power: 0.62 * mood.intensity, color: mood.light,
    }];
    agentLights.forEach((light) => {
      if (light.alpha < 0.01) return;
      lights.push({
        x: light.x, y: light.y, z: cell * 1.8,
        radius: diagonal * 0.15, power: 0.5 * light.alpha * mood.intensity, color: mood.agent,
      });
    });

    ctx.lineJoin = "round";
    ctx.lineWidth = 1;
    for (let i = 0; i < triangles.length; i += 1) {
      const tri = triangles[i];
      const [r, g, b] = shade(tri, lights);
      ctx.beginPath();
      ctx.moveTo(tri[0].x, tri[0].y);
      ctx.lineTo(tri[1].x, tri[1].y);
      ctx.lineTo(tri[2].x, tri[2].y);
      ctx.closePath();
      ctx.fillStyle = `rgb(${r | 0},${g | 0},${b | 0})`;
      ctx.fill();
      ctx.strokeStyle = `rgb(${(r + 5) | 0},${(g + 5) | 0},${(b + 5) | 0})`;
      ctx.stroke();
    }
  }

  function update(time, dt) {
    sceneTime = time;
    const ease = (rate) => 1 - Math.exp(-dt * rate);

    const m = ease(0.0024);
    for (let i = 0; i < 3; i += 1) {
      mood.base[i] += (target.base[i] - mood.base[i]) * m;
      mood.light[i] += (target.light[i] - mood.light[i]) * m;
      mood.agent[i] += (target.agent[i] - mood.agent[i]) * m;
    }
    mood.intensity += (target.intensity - mood.intensity) * m;

    const idle = !pointer.active || time - pointer.last > IDLE_AFTER_MS;
    const goal = { x: pointer.x, y: pointer.y };
    if (idle) drift(time, 0, goal);
    const follow = ease(idle ? 0.0011 : 0.0065);
    mainLight.x += (goal.x - mainLight.x) * follow;
    mainLight.y += (goal.y - mainLight.y) * follow;

    const fade = ease(0.0022);
    agentLights.forEach((light, i) => {
      light.alpha += ((i < agentCount ? 1 : 0) - light.alpha) * fade;
      drift(time, light.seed, light);
    });

    for (let i = 0; i < vertices.length; i += 1) {
      const v = vertices[i];
      v.z = v.z0 + Math.sin(time * v.speed + v.phase) * v.amp;
    }
  }

  function frame(time) {
    const dt = Math.min(64, time - lastTime || 16);
    lastTime = time;
    update(time, dt);
    draw();
    frameId = window.requestAnimationFrame(frame);
  }

  function snapToTarget() {
    mood.base = [...target.base];
    mood.light = [...target.light];
    mood.agent = [...target.agent];
    mood.intensity = target.intensity;
    agentLights.forEach((light, i) => { light.alpha = i < agentCount ? 1 : 0; });
  }

  function start() {
    if (running || !animated() || document.hidden) return;
    running = true;
    lastTime = performance.now();
    frameId = window.requestAnimationFrame(frame);
  }

  function stop() {
    running = false;
    window.cancelAnimationFrame(frameId);
  }

  function setMood(name, options = {}) {
    const preset = MOODS[name] || MOODS.sand;
    target = { ...preset, intensity: options.intensity ?? 1 };
    mood.dark = !!preset.dark;
    agentCount = Math.max(0, Math.min(MAX_AGENTS, options.agents | 0));
    if (!running) {
      snapToTarget();
      draw();
    }
  }

  let resizeFrame = 0;
  window.addEventListener("resize", () => {
    window.cancelAnimationFrame(resizeFrame);
    resizeFrame = window.requestAnimationFrame(() => {
      build();
      draw();
    });
  });

  window.addEventListener("pointermove", (event) => {
    pointer.x = event.clientX;
    pointer.y = event.clientY;
    pointer.active = true;
    pointer.last = performance.now();
  }, { passive: true });
  window.addEventListener("mouseout", (event) => {
    if (!event.relatedTarget) pointer.active = false;
  });

  document.addEventListener("visibilitychange", () => {
    if (document.hidden) stop();
    else start();
  });
  const onMotionPreference = () => {
    // Sans préférence explicite, l'état suit le réglage OS.
    if (localStorage.getItem("ac-bganim") === null) animOn = !reducedMotion.matches;
    if (!animated()) {
      stop();
      snapToTarget();
      draw();
    } else {
      start();
    }
  };
  if (reducedMotion.addEventListener) reducedMotion.addEventListener("change", onMotionPreference);

  function setAnimated(on) {
    animOn = !!on;
    localStorage.setItem("ac-bganim", animOn ? "1" : "0");
    if (animated()) {
      start();
    } else {
      // Fige sur la cible de mood courante — frame statique propre.
      stop();
      snapToTarget();
      draw();
    }
  }

  build();
  snapToTarget();
  draw();
  start();

  window.MeshBackground = { setMood, setAnimated, isAnimated: animated };
})();
