#!/usr/bin/env python3
"""E2E démon queue_steer — vérifie steer_queued de bout en bout avec un
vrai démon et un agent ACP stub (pas de navigateur requis).

Scénarios couverts :
  1. steer accepté  → entrée sortie de file, user_message steer + event
     système « promu » au journal, file vide.
  2. refus rapide   → « requeued », entrée remise à sa position, event
     « steer refusé — message remis en file », pas de doublon au drain.
  3. retenue compact → « held »/« compact », file inchangée ; le message
     en file est ensuite livré par le drain à la fin du compact.
  4. index périmé   → « gone » sur file vide.
  5. entrée peer    → queue_steer fonctionne sans émettre de
     user_message (le peer_message existe déjà).

Usage : cargo build --release -p agentcontrol-daemon &&
        python3 client/tests/queue-steer-daemon-e2e.py
Env   : AC_DAEMON (défaut target/release/agentcontrol-daemon),
        AC_STUB   (défaut /tmp/acp-steer-stub.py — auto-généré si absent).
"""
import json, os, socket, subprocess, sys, tempfile, threading, time

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
DAEMON = os.environ.get("AC_DAEMON", os.path.join(ROOT, "target/release/agentcontrol-daemon"))
STUB = os.environ.get("AC_STUB", "/tmp/acp-steer-stub.py")
MODE = "/tmp/acp-stub-mode"
REL = "/tmp/acp-stub-rel"
SAY = "/tmp/acp-stub-say"

STUB_SRC = '''#!/usr/bin/env python3
import sys, json, os, time, threading
MODE = "%s"; REL = "%s"; SAY = "%s"
held = []
out_lock = threading.Lock()
def send(v):
    with out_lock:
        sys.stdout.write(json.dumps(v) + "\\n"); sys.stdout.flush()
def mode():
    try: return open(MODE).read().strip()
    except OSError: return "hold"
def chunk(sid, text):
    send({"jsonrpc": "2.0", "method": "session/update", "params": {
        "sessionId": sid, "update": {"sessionUpdate": "agent_message_chunk",
        "content": {"type": "text", "text": text}}}})
def handle(msg):
    mid, m = msg.get("id"), msg.get("method")
    if m == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "protocolVersion": 1,
            "agentCapabilities": {"loadSession": False},
            "agentInfo": {"name": "steer-stub", "version": "1.0"}}})
    elif m in ("session/new", "session/load"):
        send({"jsonrpc": "2.0", "id": mid, "result": {"sessionId": "native-1"}})
    elif m == "session/prompt":
        params = msg.get("params", {})
        text = "".join(b.get("text", "") for b in params.get("prompt", []))
        md = mode()
        if md == "accept":
            chunk(params.get("sessionId", ""), "[ok] " + text[:40])
            send({"jsonrpc": "2.0", "id": mid, "result": {"stopReason": "end_turn"}})
        elif md == "refuse":
            send({"jsonrpc": "2.0", "id": mid, "error": {
                "code": -32000, "message": "steer refused"}})
        else:
            held.append((mid, params.get("sessionId", "")))
    elif m == "session/cancel":
        for h, s in held[:]:
            send({"jsonrpc": "2.0", "id": h, "result": {"stopReason": "cancelled"}})
            held.remove((h, s))
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "result": {}})
def watcher():
    while True:
        # Release unitaire : UN prompt, puis le fichier est consommé —
        # chaque release() du driver ne termine qu'un seul tour.
        if os.path.exists(REL) and held:
            try: os.remove(REL)
            except OSError: pass
            h, s = held.pop(0)
            chunk(s, "turn done")
            if mode() == "holderr":
                send({"jsonrpc": "2.0", "id": h, "error": {
                    "code": -32000, "message": "steer refused"}})
            else:
                send({"jsonrpc": "2.0", "id": h,
                      "result": {"stopReason": "end_turn"}})
        # SAY n'est consommé que s'il porte du contenu — une lecture en
        # vol pendant l'écriture ne doit pas avaler un message vide.
        if os.path.exists(SAY):
            try: txt = open(SAY).read().strip()
            except OSError: txt = ""
            if txt:
                chunk("native-1", txt)
                try: os.remove(SAY)
                except OSError: pass
        time.sleep(0.05)
threading.Thread(target=watcher, daemon=True).start()
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try: handle(json.loads(line))
    except Exception: pass
''' % (MODE, REL, SAY)

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

def set_mode(m):
    with open(MODE, "w") as f:
        f.write(m)

def release():
    open(REL, "w").close()

def clear_release():
    try:
        os.remove(REL)
    except OSError:
        pass

def say(txt):
    # Écriture atomique : le watcher du stub ne voit que le contenu
    # complet, jamais un fichier vide en cours d'écriture.
    with open(SAY + ".tmp", "w") as f:
        f.write(txt)
    os.replace(SAY + ".tmp", SAY)


class Conn:
    """Ligne JSON par ligne sur le socket TCP du démon — démux par id,
    les pushes (subscribe) sont collectés à part."""
    def __init__(self, port):
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=30)
        self.f = self.sock.makefile("rwb")
        self.n = 0
        self.lock = threading.Lock()
        self.pending = {}
        self.pushes = []
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
            else:
                self.pushes.append(msg)

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


def wait_for(fn, timeout=15, step=0.1):
    t0 = time.time()
    while time.time() - t0 < timeout:
        v = fn()
        if v:
            return v
        time.sleep(step)
    return fn()


def main():
    if not os.path.exists(DAEMON):
        print(f"daemon introuvable: {DAEMON} — cargo build --release -p agentcontrol-daemon")
        sys.exit(2)
    os.makedirs(os.path.dirname(STUB) or "/tmp", exist_ok=True)
    if not os.path.exists(STUB):
        with open(STUB, "w") as f:
            f.write(STUB_SRC)
        os.chmod(STUB, 0o755)
    set_mode("hold")
    clear_release()
    try:
        os.remove(SAY)
    except OSError:
        pass

    home = tempfile.mkdtemp(prefix="ac-steer-home-")
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    with open(os.path.join(home, "config.toml"), "w") as f:
        f.write(
            'bind_tcp = "127.0.0.1:%d"\n'
            "auto_resume = false\n\n"
            "[a2a]\n"
            "enabled = true\n\n"
            "[agents.stub]\n"
            'name = "Stub"\n'
            'command = ["python3", "%s"]\n' % (port, STUB)
        )

    env = dict(os.environ, AGENTCONTROL_HOME=home)
    daemon = subprocess.Popen([DAEMON], env=env, stdout=subprocess.DEVNULL,
                              stderr=subprocess.STDOUT)
    try:
        # Attente du listener
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

        sid = c.result("new_session", agent="stub", cwd="/tmp", title="steer-e2e")["session"]
        # Le spawn est async : attendre le handshake complet (idle).
        ok = wait_for(lambda: c.result("session_info", session=sid)
                      .get("meta", {}).get("state") == "idle", 20)
        check("session prête (idle après spawn)", ok,
              c.result("session_info", session=sid).get("meta", {}).get("state"))

        # --- 1. Steer accepté -------------------------------------------------
        c.result("send", session=sid, text="first")        # tour retenu (busy)
        time.sleep(0.4)
        c.result("send", session=sid, text="second")       # en file
        q = c.result("queue_list", session=sid)["queue"]
        check("second en file", q == ["second"], str(q))

        set_mode("accept")
        r = c.result("queue_steer", session=sid, index=0)
        check("steer accepté → delivered=steer", r["delivered"] == "steer", str(r))
        check("steer : file renvoyée vide", r.get("queue") == [], str(r))

        evs = c.result("events", session=sid, limit=50)["events"]
        um = [e for e in evs if e["type"] == "user_message" and e.get("text") == "second"]
        sy = [e for e in evs if e["type"] == "system"]
        check("user_message steer=true « second » au journal",
              len(um) == 1 and um[0].get("steer") is True, str(um))
        check("event « promu en steer par l'utilisateur » au journal",
              any("promu en steer" in (e.get("text") or "") for e in sy),
              str([e.get("text") for e in sy]))
        check("« second » absent de queue_list",
              c.result("queue_list", session=sid)["queue"] == [])

        # --- 2. Refus rapide → requeue ---------------------------------------
        c.result("send", session=sid, text="third")        # en file (busy)
        q = wait_for(lambda: c.result("queue_list", session=sid)["queue"] == ["third"], 5)
        check("third en file", q)
        set_mode("refuse")
        r = c.result("queue_steer", session=sid, index=0)
        check("refus rapide → delivered=requeued", r["delivered"] == "requeued", str(r))
        check("requeued : entrée remise en file", r.get("queue") == ["third"], str(r))
        time.sleep(0.3)
        evs = c.result("events", session=sid, limit=50)["events"]
        check("event « steer refusé — message remis en file »",
              any(e["type"] == "system" and "remis en file" in (e.get("text") or "")
                  for e in evs),
              str([e.get("text") for e in evs if e["type"] == "system"]))

        # --- 2b. Régression : steer refusé d'un /compact --------------------
        # arm_compact tiré par le steer puis prompt refusé → la retenue
        # doit être désarmée (sinon la file reste bloquée ~180 s).
        c.result("send", session=sid, text="/compact")
        q = wait_for(lambda: "/compact" in
                     c.result("queue_list", session=sid)["queue"], 5)
        check("/compact en file pour le steer", q,
              str(c.result("queue_list", session=sid)["queue"]))
        r = c.result("queue_steer", session=sid, index=1)
        check("/compact refusé → requeued", r["delivered"] == "requeued", str(r))
        c.result("send", session=sid, text="after-cmd")
        r = c.result("queue_steer", session=sid, index=2)
        check("retenue désarmée : steer suivant pas held/compact",
              r["delivered"] == "requeued" and r.get("reason") != "compact", str(r))
        st = c.result("session_info", session=sid).get("meta", {}).get("state")
        check("état pas resté « compacting »", st != "compacting", st)
        # Nettoie « after-cmd » (restant en file) — la phase compact suit.
        c.result("queue_drop", session=sid, index=2)
        c.result("queue_drop", session=sid, index=1)
        q = wait_for(lambda: c.result("queue_list", session=sid)["queue"] == ["third"], 5)
        check("file purgée pour la phase compact", q,
              str(c.result("queue_list", session=sid)["queue"]))

        # --- 2c. Régression : refus tardif en dernier volant -----------------
        # /compact steeré retenu côté agent (holderr) pendant que le tour
        # « first » finit : à l'erreur tardive le steer est le dernier
        # prompt en vol → prompt_rpc a posé « compacting » sur la retenue
        # armée. Le désarmement doit redresser l'état ET relancer le drain
        # — busy est resté vrai sans runner, la seule réinsertion
        # laisserait l'entrée orpheline indéfiniment.
        c.result("queue_drop", session=sid, index=0)   # « third » — file vide
        set_mode("holderr")        # retenir + répondre en erreur au release
        c.result("send", session=sid, text="/compact")
        q = wait_for(lambda: c.result("queue_list", session=sid)
                     ["queue"] == ["/compact"], 5)
        check("/compact en file pour le refus tardif", q,
              str(c.result("queue_list", session=sid)["queue"]))
        r = c.result("queue_steer", session=sid, index=0)
        check("steer retenu côté agent → verdict optimiste",
              r["delivered"] == "steer", str(r))
        release()                  # « first » rend la main (erreur, fin quand même)
        time.sleep(0.5)
        release()                  # refus tardif du /compact steeré — dernier volant
        q = wait_for(lambda: c.result("queue_list", session=sid)["queue"] == [], 10)
        check("refus tardif : /compact re-livré par le drain (pas orphelin)", q,
              str(c.result("queue_list", session=sid)["queue"]))
        st = c.result("session_info", session=sid).get("meta", {}).get("state")
        check("état pas resté « compacting » fantôme", st != "compacting", st)
        release()                  # /compact re-livré → erreur → désarm drain
        ok = wait_for(lambda: c.result("session_info", session=sid)
                      .get("meta", {}).get("state") == "idle", 10)
        check("session de retour idle après le refus tardif", ok,
              c.result("session_info", session=sid).get("meta", {}).get("state"))
        q = c.result("queue_list", session=sid)["queue"]
        check("file vide en fin de refus tardif", q == [], str(q))

        # --- 3. Compact hold --------------------------------------------------
        # La phase 2c a terminé le tour « first » et vidé la file —
        # re-stage un tour retenu pour remettre la session busy.
        set_mode("hold")
        c.result("send", session=sid, text="t3")        # livré direct (idle) → retenu
        ok = wait_for(lambda: c.result("session_info", session=sid)
                      .get("meta", {}).get("state") == "working", 5)
        check("tour t3 retenu (session busy)", ok,
              c.result("session_info", session=sid).get("meta", {}).get("state"))
        c.result("send", session=sid, text="/compact")  # en file derrière le tour
        q = wait_for(lambda: c.result("queue_list", session=sid)["queue"] == ["/compact"], 5)
        check("/compact en file derrière le tour", q,
              str(c.result("queue_list", session=sid)["queue"]))
        release()                     # fin du tour « t3 » → /compact drainé, retenue armée
        time.sleep(0.6)
        c.result("send", session=sid, text="fourth")       # retenu par le compact
        q = wait_for(lambda: c.result("queue_list", session=sid)["queue"] == ["fourth"], 5)
        check("fourth retenu par le compact", q,
              str(c.result("queue_list", session=sid)["queue"]))
        r = c.result("queue_steer", session=sid, index=0)
        check("compact → held/compact", r["delivered"] == "held" and r.get("reason") == "compact",
              str(r))
        check("held : file inchangée", r.get("queue") == ["fourth"], str(r))

        say("compact done")           # signal agent → fin de la retenue
        release()                     # le tour /compact rend la main aussi
        q = wait_for(lambda: c.result("queue_list", session=sid)["queue"] == [], 10)
        check("fin du compact → fourth drainé", q,
              str(c.result("queue_list", session=sid)["queue"]))

        # --- 4. Index périmé --------------------------------------------------
        r = c.result("queue_steer", session=sid, index=0)
        check("file vide → gone", r["delivered"] == "gone", str(r))

        # --- 5. Entrée peer ---------------------------------------------------
        # Tour « fourth » encore retenu (mode hold) → peer_send en file.
        # peer_send exige une session émettrice réelle + a2a.enabled.
        sid2 = c.result("new_session", agent="stub", cwd="/tmp",
                        title="steer-peer")["session"]
        ok = wait_for(lambda: c.result("session_info", session=sid2)
                      .get("meta", {}).get("state") == "idle", 20)
        check("session peer prête", ok)
        n_user_before = len([e for e in c.result("events", session=sid, limit=80)["events"]
                             if e["type"] == "user_message"])
        c.result("peer_send", **{"from": sid2, "session": sid,
                                 "text": "coucou du peer", "delivery": "queue"})
        q = wait_for(lambda: any("coucou du peer" in t for t in
                     c.result("queue_list", session=sid)["queue"]), 5)
        check("message peer en file", q,
              str(c.result("queue_list", session=sid)["queue"]))
        set_mode("accept")
        r = c.result("queue_steer", session=sid, index=0)
        check("peer → delivered=steer", r["delivered"] == "steer", str(r))
        evs = c.result("events", session=sid, limit=80)["events"]
        n_user_after = len([e for e in evs if e["type"] == "user_message"])
        check("steer peer : aucun user_message ajouté",
              n_user_after == n_user_before, f"{n_user_before}→{n_user_after}")
        check("steer peer : peer_message tracé",
              any(e["type"] == "peer_message" and "coucou" in (e.get("text") or "")
                  for e in evs), str([e["type"] for e in evs]))

        release()                     # tout termine proprement
        clear_release()

    finally:
        daemon.terminate()
        try:
            daemon.wait(timeout=10)
        except subprocess.TimeoutExpired:
            daemon.kill()

    print(f"\n{PASS} PASS, {FAIL} FAIL")
    sys.exit(1 if FAIL else 0)


if __name__ == "__main__":
    main()
