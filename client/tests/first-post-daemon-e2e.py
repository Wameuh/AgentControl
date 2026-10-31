#!/usr/bin/env python3
"""E2E démon — first_post sur session service + signal orchestrateur (#105).

Deux volets du bug de l'item #105 :

1. `first_post` sur une session SERVICE (créée via `origin`) servait le
   prompt de spawn (boilerplate de mission) au lieu du premier post
   utile — le user_message « [Backlog] item » journalisé par
   l'orchestrateur natif. Désormais le prompt de spawn porte le flag
   `spawn_prompt` posé à l'émission, et `first_post` le saute (avec un
   repli « premier user_message portant l'origin de la session » pour
   les journaux écrits avant le flag).

2. `component_info backlog-orchestrator` doit signaler que le post est
   indisponible quand le flock partagé est détenu par le script Python
   legacy (qui ne le journalise pas) — sonde /proc/locks + cmdline.

Scénario : démon isolé (HOME+AGENTCONTROL_HOME temporaires, TCP
loopback) — `new_session` avec origin+prompt → first_post vide, puis
post « [Backlog] » injecté dans le journal → first_post le sert.
Journal réécrit SANS le flag (simulation legacy) → même verdict. Une
session humaine garde le comportement premier-UM. Enfin un faux
`backlog-orchestrator.py` prend le flock → lock_holder: legacy /
item_post: unavailable, puis phase blocked après `orchestrator_set`,
puis reprise par le natif quand le faux meurt.

Usage : cargo build --release -p agentcontrol-daemon &&
        python3 client/tests/first-post-daemon-e2e.py
Env   : AC_DAEMON (défaut target/release/agentcontrol-daemon),
        AC_MOCK  (défaut acp-mock-agent sur PATH, sinon /bin/true).
"""
import json, os, shutil, socket, subprocess, sys, tempfile, threading, time

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
DAEMON = os.environ.get("AC_DAEMON", os.path.join(ROOT, "target/release/agentcontrol-daemon"))
MOCK = os.environ.get("AC_MOCK") or shutil.which("acp-mock-agent") or "/bin/true"
ORCH_ORIGIN = "component:backlog-orchestrator"

PASS = 0
FAIL = 0

def check(name, ok, extra=""):
    global PASS, FAIL
    if ok:
        PASS += 1
        print("PASS", name)
    else:
        FAIL += 1
        print("FAIL", name, extra)


class Conn:
    """Ligne JSON par ligne sur le socket TCP du démon — démux par id."""
    def __init__(self, port):
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=30)
        self.f = self.sock.makefile("rwb")
        self.n = 0
        self.lock = threading.Lock()
        self.pending = {}
        self.running = True
        threading.Thread(target=self._reader, daemon=True).start()

    def _reader(self):
        while self.running:
            line = self.f.readline()
            if not line:
                break
            try:
                msg = json.loads(line)
            except ValueError:
                continue
            if "id" in msg and msg["id"] in self.pending:
                self.pending.pop(msg["id"]).append(msg)

    def rpc(self, req, timeout=15, **params):
        with self.lock:
            self.n += 1
            rid = self.n
            ev = []
            self.pending[rid] = ev
        self.f.write((json.dumps({"id": rid, "req": req, **params}) + "\n").encode())
        self.f.flush()
        t0 = time.time()
        while time.time() - t0 < timeout:
            if ev:
                return ev[0]
            time.sleep(0.02)
        raise TimeoutError(f"rpc {req} timeout")

    def result(self, req, **params):
        r = self.rpc(req, **params)
        if not r.get("ok"):
            raise RuntimeError(f"{req} failed: {r}")
        return r["result"]


def journal_path(home, sid):
    return os.path.join(home, "sessions", sid, "events.jsonl")


def append_um(path, text, origin=None, spawn_prompt=False):
    """Append un user_message au journal — la même ligne que produit
    l'emit interne de l'orchestrateur ([Backlog] item). seq = dernier+1.
    """
    last = 0
    if os.path.exists(path):
        with open(path) as f:
            for line in f:
                line = line.strip()
                if line:
                    try:
                        last = max(last, json.loads(line).get("seq", 0))
                    except ValueError:
                        pass
    ev = {"seq": last + 1, "ts": "2026-01-01T00:00:00Z",
          "type": "user_message", "text": text, "steer": False,
          "spawn_prompt": spawn_prompt}
    if origin is not None:
        ev["origin"] = origin
    with open(path, "a") as f:
        f.write(json.dumps(ev) + "\n")


def orch_status(c):
    return c.result("component_info", name="backlog-orchestrator").get("orchestrator", {})


FAKE_LEGACY = '''#!/usr/bin/env python3
import fcntl, sys, time
f = open(sys.argv[1], "w")
fcntl.flock(f, fcntl.LOCK_EX)
time.sleep(3600)
'''


def main():
    if not os.path.exists(DAEMON):
        print(f"daemon introuvable: {DAEMON} — cargo build --release -p agentcontrol-daemon")
        sys.exit(2)

    home = tempfile.mkdtemp(prefix="ac-firstpost-home-")
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    with open(os.path.join(home, "config.toml"), "w") as f:
        f.write(
            'bind_tcp = "127.0.0.1:%d"\n' % port
            + '\n[agents.mock]\ncommand = ["%s"]\n' % MOCK
            + '\n[backlog_orchestrator]\npoll_secs = 10\n'
            + 'watch_prs = false\nauto_patchnotes = false\n'
            + 'spawn_managers = false\n'
        )

    env = dict(os.environ, AGENTCONTROL_HOME=home, HOME=home)
    daemon = subprocess.Popen([DAEMON], env=env, stdout=subprocess.DEVNULL,
                              stderr=subprocess.STDOUT)
    c = None
    fake = None
    try:
        t0 = time.time()
        while time.time() - t0 < 15:
            try:
                c = Conn(port)
                break
            except OSError:
                if daemon.poll() is not None:
                    print("le démon est mort au démarrage")
                    sys.exit(2)
                time.sleep(0.1)
        else:
            print("listener introuvable après 15 s")
            sys.exit(2)

        # --- Session service : prompt de spawn sauté -------------------
        sid = c.result("new_session", agent="mock", cwd="/tmp",
                       title="Backlog-W105", origin=ORCH_ORIGIN,
                       prompt="MISSION boilerplate").get("session")
        check("session service créée", sid is not None, str(sid))

        r = c.result("first_post", session=sid)
        check("first_post : post null (prompt de spawn sauté)",
              r.get("post") is None and not r.get("truncated"), str(r))

        # Le post « [Backlog] item » arrive (emit interne orchestrateur).
        jp = journal_path(home, sid)
        append_um(jp, "[Backlog] item #105 — bug post jamais émis", ORCH_ORIGIN)
        r = c.result("first_post", session=sid)
        post = r.get("post") or {}
        check("first_post sert le post [Backlog], pas le prompt",
              post.get("text", "").startswith("[Backlog] item #105")
              and post.get("origin") == ORCH_ORIGIN,
              str(r))

        # Journal LEGACY (écrit avant le flag) : on retire la clé
        # `spawn_prompt` de chaque ligne — un daemon pré-flag la
        # sérialisait sans. Le repli « premier UM portant l'origin »
        # doit sauter le prompt tout de même.
        with open(jp) as f:
            lines = [json.loads(l) for l in f if l.strip()]
        for ev in lines:
            ev.pop("spawn_prompt", None)
        with open(jp, "w") as f:
            for ev in lines:
                f.write(json.dumps(ev) + "\n")
        r = c.result("first_post", session=sid)
        post = r.get("post") or {}
        check("journal legacy (sans flag) : post servi quand même",
              post.get("text", "").startswith("[Backlog] item #105"),
              str(r))

        # --- Session humaine : premier user_message servi --------------
        sid2 = c.result("new_session", agent="mock", cwd="/tmp",
                        title="humaine").get("session")
        r = c.result("first_post", session=sid2)
        check("session humaine : vide avant tout message",
              r.get("post") is None, str(r))
        append_um(journal_path(home, sid2), "salut humain")
        r = c.result("first_post", session=sid2)
        check("session humaine : premier UM servi",
              (r.get("post") or {}).get("text") == "salut humain", str(r))

        # --- Signal : flock détenu par le script legacy ----------------
        st_dir = os.path.join(home, ".agentbacklog")
        os.makedirs(st_dir, exist_ok=True)
        lock_path = os.path.join(st_dir, "orchestrator.lock")
        fake_path = os.path.join(home, "backlog-orchestrator.py")
        with open(fake_path, "w") as f:
            f.write(FAKE_LEGACY)
        fake = subprocess.Popen(
            ["python3", fake_path, lock_path],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        # Le flock est visible dans /proc/locks dès flock() — poll
        # jusqu'à ce que le détenteur soit vu (démarrage python ~100 ms).
        t0 = time.time()
        while time.time() - t0 < 5:
            o = orch_status(c)
            if o.get("lock_holder") not in (None, "none"):
                break
            time.sleep(0.2)
        check("lock_holder = legacy (script Python identifié)",
              o.get("lock_holder") == "legacy"
              and o.get("lock_holder_pid") == fake.pid,
              str({k: o.get(k) for k in ("lock_holder", "lock_holder_pid")}))
        check("item_post = unavailable sous le legacy",
              o.get("item_post") == "unavailable", str(o.get("item_post")))
        check("item_post_detail mentionne le legacy + le post",
              "legacy" in (o.get("item_post_detail") or "")
              and "[Backlog]" in (o.get("item_post_detail") or ""),
              str(o.get("item_post_detail")))

        # Activation → le natif voit le flock pris : phase blocked et le
        # détail nomme le détenteur legacy.
        c.result("orchestrator_set", enabled=True)
        t0 = time.time()
        while time.time() - t0 < 15:
            o = orch_status(c)
            if o.get("phase") == "blocked":
                break
            time.sleep(0.3)
        check("phase blocked quand le legacy détient le flock",
              o.get("phase") == "blocked", str(o.get("phase")))
        check("detail blocked nomme backlog-orchestrator.py",
              "backlog-orchestrator.py" in (o.get("detail") or ""),
              str(o.get("detail")))

        # Le legacy meurt → le natif prend le flock au prochain tick.
        fake.terminate(); fake.wait(timeout=5); fake = None
        t0 = time.time()
        while time.time() - t0 < 20:
            o = orch_status(c)
            if o.get("lock_holder") == "self":
                break
            time.sleep(0.5)
        check("reprise : lock_holder = self après mort du legacy",
              o.get("lock_holder") == "self", str(o.get("lock_holder")))
        check("item_post redevenu available",
              o.get("item_post") == "available", str(o.get("item_post")))
        c.result("orchestrator_set", enabled=False)
    finally:
        if fake is not None:
            fake.terminate()
        daemon.terminate()
        try:
            daemon.wait(timeout=5)
        except subprocess.TimeoutExpired:
            daemon.kill()
        if c is not None:
            c.running = False
            c.sock.close()

    print(f"\n{PASS} PASS / {FAIL} FAIL")
    sys.exit(1 if FAIL else 0)


if __name__ == "__main__":
    main()
