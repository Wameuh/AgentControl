#!/usr/bin/env python3
"""E2E démon — resync des specs catalogue au chargement (bug #97).

Une spec persistée est un SNAPSHOT de l'entrée catalogue à l'install :
avant le fix, un composant installé avant l'enrichissement de l'entrée
(params_schema/render/healthcheck…) gardait l'ancienne surface à vie —
agentreviewer installé pré-#76 n'exposait pas le param `repos`.

Scénario : components.toml avec une spec « agentreviewer » vieux format
(sans catalog/params_schema/render) + params utilisateur (valeur à
conserver, clé zombie retirée du catalogue, valeur invalide sous le
schéma resserré). Au boot du démon isolé :
  - les champs catalogue-owned sont rafraîchis (params_schema avec
    `repos`, render, healthcheck sentinelle, catalog = "agentreviewer"),
  - les choix utilisateur survivent (params.repo, description,
    autostart),
  - zombie + valeur invalide sont purgés (component chargé quand même),
  - components.toml N'EST PAS réécrit au boot (merge en mémoire),
  - `component_set_params` accepte ensuite la nouvelle clé `repos`
    (l'ancienne spec aurait répondu « paramètre non déclaré ») et le
    rendu écrit la config managée.

Usage : cargo build --release -p agentcontrol-daemon &&
        python3 client/tests/comp-resync-daemon-e2e.py
Env   : AC_DAEMON (défaut target/release/agentcontrol-daemon).
"""
import json, os, socket, subprocess, sys, tempfile, threading, time

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
DAEMON = os.environ.get("AC_DAEMON", os.path.join(ROOT, "target/release/agentcontrol-daemon"))

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


def main():
    if not os.path.exists(DAEMON):
        print(f"daemon introuvable: {DAEMON} — cargo build --release -p agentcontrol-daemon")
        sys.exit(2)

    home = tempfile.mkdtemp(prefix="ac-resync-home-")
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    with open(os.path.join(home, "config.toml"), "w") as f:
        f.write('bind_tcp = "127.0.0.1:%d"\n' % port)

    # Spec « agentreviewer » vieux format : ni catalog/params_schema/
    # render — params posés « à la main » (valeur à garder, clé zombie,
    # entier invalide) + description/autostart choix utilisateur.
    components_toml = os.path.join(home, "components.toml")
    with open(components_toml, "w") as f:
        f.write(
            '[[components]]\n'
            'name = "agentreviewer"\n'
            'kind = "process"\n'
            'command = ["agent-reviewer", "--watch"]\n'
            'autostart = false\n'
            'restart = "on-failure"\n'
            'restart_sec = 5\n'
            'description = "Watcher custom descr"\n'
            '\n'
            '[components.healthcheck]\n'
            'kind = "command"\n'
            'argv = ["sh", "-c", "test -r ~/.config/agentreviewer/config.toml"]\n'
            '\n'
            '[components.params]\n'
            'repo = "Wameuh/AgentControl"\n'
            'repo_dir = "/tmp/clone"\n'
            'zombie = "dead-key"\n'
            'max_parallel = "not-an-int"\n'
            '\n'
            # Entrée sans schéma : ses params sont libres (set_params
            # accepte toute clé) — le resync ne doit pas les purger.
            '[[components]]\n'
            'name = "agentbacklog"\n'
            'kind = "tool"\n'
            'command = ["agentbacklog", "--version"]\n'
            '\n'
            '[components.params]\n'
            'meta = "free-note"\n'
        )

    # HOME isolé : le render borne ses écritures sous HOME — l'écrire
    # dans le tempdir garde le vrai ~/.config/agentreviewer intact.
    env = dict(os.environ, AGENTCONTROL_HOME=home, HOME=home)
    daemon = subprocess.Popen([DAEMON], env=env, stdout=subprocess.DEVNULL,
                              stderr=subprocess.STDOUT)
    c = None
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

        item = c.result("component_info", name="agentreviewer")
        spec = item.get("spec", {})

        # --- Champs catalogue-owned rafraîchis -------------------------
        check("item.catalog = true", item.get("catalog") is True, str(item.get("catalog")))
        check("spec.catalog = agentreviewer",
              spec.get("catalog") == "agentreviewer", str(spec.get("catalog")))
        keys = [p.get("key") for p in spec.get("params_schema", [])]
        check("params_schema rafraîchi (clé « repos » présente)",
              "repos" in keys, str(keys))
        check("render restauré",
              isinstance(spec.get("render"), dict)
              and "agentreviewer/config.toml" in spec["render"].get("path", ""),
              str(spec.get("render")))
        hc = spec.get("healthcheck", {})
        check("healthcheck = sentinelle catalogue (grep owner/repo)",
              hc.get("kind") == "command"
              and any("owner/repo" in a for a in hc.get("argv", [])),
              str(hc))
        check("command catalogue",
              spec.get("command") == ["agent-reviewer", "--watch"],
              str(spec.get("command")))

        # --- Choix utilisateur préservés / hygiène ---------------------
        params = spec.get("params", {})
        check("param user « repo » conservé",
              params.get("repo") == "Wameuh/AgentControl", str(params))
        check("param user « repo_dir » conservé",
              params.get("repo_dir") == "/tmp/clone", str(params))
        check("clé zombie purgée", "zombie" not in params, str(params))
        # « params » RPC = valeurs effectives (défauts ∪ posées) : la
        # valeur « not-an-int » a été purgée → le défaut « 1 » s'affiche.
        check("valeur invalide purgée (max_parallel → défaut)",
              params.get("max_parallel") == "1", str(params))
        check("description utilisateur conservée",
              spec.get("description") == "Watcher custom descr",
              str(spec.get("description")))
        check("autostart utilisateur conservé",
              spec.get("autostart") is False, str(spec.get("autostart")))

        # --- Params libres (entrée sans schéma) préservés --------------
        bl = c.result("component_info", name="agentbacklog")
        check("params libres sans schéma préservés (agentbacklog)",
              bl.get("spec", {}).get("params", {}).get("meta") == "free-note",
              str(bl.get("spec", {}).get("params")))

        # --- Merge en mémoire : components.toml non réécrit au boot ----
        with open(components_toml) as f:
            txt = f.read()
        check("components.toml inchangé au boot (pas de réécriture)",
              "params_schema" not in txt, "params_schema écrit au boot")

        # --- La nouvelle clé est utilisable (le bug : « non déclaré ») -
        r = c.result("component_set_params", name="agentreviewer",
                     params={"repos": '[{"repo":"a/b","repo_dir":"/x"}]'})
        params = r.get("params", {})
        check("comp set « repos » accepté sur spec resynchronisée",
              "repos" in params, str(params))
        rendered = os.path.join(home, ".config/agentreviewer/config.toml")
        with open(rendered) as f:
            cfg = f.read()
        check("config rendue contient le nouveau param « repos »",
              'repos = [{' in cfg and '"a/b"' in cfg, cfg[:200])
    finally:
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
