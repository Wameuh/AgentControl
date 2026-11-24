#!/usr/bin/env python3
"""E2E des scripts POSIX embarqués par ensure_daemon (ensure_daemon.rs) —
les DEUX consts sont EXTRAITES de la source Rust (pas de copie : le test
suit le code) puis exécutées dans un sandbox (HOME + PATH factices) :
aucun vrai démon, binaire ou superviseur n'est touché.

Architecture I1 (option a) : PROBE_SCRIPT ne fait que sonder (arch,
version, port) et répond `install:<arch>` — le download+signature vit
côté app ; INSTALL_SCRIPT reçoit l'artefact vérifié sur stdin
(`cat > acd.tgz`), le test le nourrit avec le tarball factice.

Doublons factices sur PATH : acctl (versions pilotées par fichiers d'état,
branche AGENTCONTROL_TCP simulant le listener du port), curl (probe du
port uniquement — plus aucun download distant), install (n'écrit que
sous $HOME), supervisorctl/systemctl/pkill/pgrep/nohup (journalisés,
jamais exécutés), docker (sortie pilotée par state/dockermode —
"hit" simule un conteneur/projet agentcontrol, sinon échec = hôte sans
docker joignable).

Matrice :
  ok / newer          → rien n'est installé (sortie anticipée)
  ver<tag, port libre → install → installed:<tag>
  ver<tag, port occupé PAR NOTRE DÉMON (même version via AGENTCONTROL_TCP)
    → install — c'est le cas corrigé : la garde busy rendait avant tout
    upgrade impossible (le démon vivant bloquait sa propre maj)
  ver<tag, port occupé par un service étranger / un AUTRE démon
    (version ≠) / un listener qui ne répond pas → busy, rien d'installé
  démon injoignable + port occupé → busy ; + port libre → install
  traces « managé ailleurs » (BUG C) → managed:<detail>, rien d'installé :
    ~/.agentcontrol présent, authorized_keys avec permitopen/agentcontrol,
    conteneur/projet docker, compose publiant le port

Usage : python3 client/tests/ensure-daemon-e2e.py
"""
import os, re, shutil, subprocess, sys, tarfile, tempfile, io

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
ENSURE_RS = os.path.join(ROOT, "client/src-tauri/src/commands/ensure_daemon.rs")
TAG = "9.9.9"
PORT = 7331


def extract_scripts():
    """Reconstitue les deux consts telles que le fichier les porte :
    supprime les continuations `\\`+newline+indent, dé-escape \\\" et \\\\,
    remplace {t}/{p}. La fin d'une const = `"` non échappé + `;`."""
    src = open(ENSURE_RS, encoding="utf-8").read()
    out = {}
    for name in ("PROBE_SCRIPT", "INSTALL_SCRIPT"):
        m = re.search(r'const %s: &str = "(.*?)(?<!\\)";' % name, src, re.S)
        assert m, f"const {name} introuvable"
        raw = re.sub(r"\\\n\s*", "", m.group(1))
        buf, i = [], 0
        while i < len(raw):
            if raw[i] == "\\" and i + 1 < len(raw) and raw[i + 1] in "\\\"":
                buf.append(raw[i + 1]); i += 2
            else:
                buf.append(raw[i]); i += 1
        out[name] = "".join(buf)
    probe = out["PROBE_SCRIPT"].replace("{t}", TAG).replace("{p}", str(PORT))
    return probe, out["INSTALL_SCRIPT"]


FAKE_ACCTL = """#!/bin/sh
# status simulé : sans AGENTCONTROL_TCP → réponse socket unix (state/ver,
# absent = démon injoignable) ; avec → réponse du listener sur le port.
S="$AC_STATE"
[ "$1" = status ] || { echo "fake acctl: $*" >&2; exit 2; }
if [ -n "$AGENTCONTROL_TCP" ]; then
  case "$(cat "$S/tcpmode" 2>/dev/null || echo refused)" in
    ours) v=$(cat "$S/ver" 2>/dev/null); [ -n "$v" ] && echo "{\\"version\\":\\"$v\\"}" || exit 1 ;;
    other) echo "{\\"version\\":\\"$(cat "$S/tcpver")\\"}" ;;
    http) echo "HTTP/1.0 501 nonsense" ;;
    hang) sleep 10 ;;
    *) exit 1 ;;
  esac
else
  v=$(cat "$S/ver" 2>/dev/null)
  [ -n "$v" ] && echo "{\\"version\\":\\"$v\\"}" || exit 1
fi
"""

FAKE_CURL = """#!/bin/sh
# probe du port uniquement : "-o /dev/null ... http://127.0.0.1:P" →
# state/portstate. Le download distant n'existe plus (I1 : l'artefact
# est poussé vérifié par l'app via stdin — voir run_scenario).
out=""; prev=""
for a in "$@"; do [ "$prev" = -o ] && out="$a"; prev="$a"; done
case "$out" in
  /dev/null) [ "$(cat "$AC_STATE/portstate" 2>/dev/null || echo free)" = free ] && exit 7 || exit 0 ;;
  *) echo "fake curl: download distant inattendu ($*)" >&2; exit 2 ;;
esac
"""

# install factice : journalisé + n'écrit QUE sous $HOME (la branche svc du
# script peut cibler un chemin supervisé réel — hors sandbox → refusé).
FAKE_INSTALL = """#!/bin/sh
last=""; for a in "$@"; do last="$a"; done
echo "install $*" >> "$AC_STATE/calls.log"
case "$last" in "$HOME"/*) exec @REAL_INSTALL@ "$@" ;; *) exit 0 ;; esac
"""

FAKE_SVC = """#!/bin/sh
echo "$(basename "$0") $*" >> "$AC_STATE/calls.log"
case "$(basename "$0")" in
  systemctl) [ "$1" = cat ] && exit 1; exit 0 ;;
  pgrep) exit 1 ;;
  *) exit 0 ;;
esac
"""

# docker factice : sans state/dockermode=hit, toute commande échoue
# (daemon absent / pas de conteneur) — la sonde doit dégrader proprement.
# "hit" simule `docker ps -a` ou `docker compose ls` listant un projet
# agentcontrol/devcontainer.
FAKE_DOCKER = """#!/bin/sh
[ "$(cat "$AC_STATE/dockermode" 2>/dev/null)" = hit ] || exit 1
case "$*" in
  "ps -a --format {{.Names}} {{.Image}}") echo "devcontainer-dev-1 agentcontrol-daemon" ;;
  "compose ls") echo "agentcontrol  running  2" ;;
esac
exit 0
"""

# acctl livré dans le tarball factice = le « nouveau » démon installé.
NEW_ACCTL = '#!/bin/sh\necho "{\\"version\\":\\"%s\\"}"\n' % TAG


def make_tgz(path):
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as t:
        for name, body, mode in [
            ("agentcontrol-daemon", "#!/bin/sh\nexit 0\n", 0o755),
            ("acctl", NEW_ACCTL, 0o755),
            ("packaging/agentcontrol-daemon.service", "[Unit]\nX=1\n", 0o644),
        ]:
            ti = tarfile.TarInfo(name)
            data = body.encode()
            ti.size, ti.mode = len(data), mode
            t.addfile(ti, io.BytesIO(data))
    open(path, "wb").write(buf.getvalue())


def write(path, body, mode=0o644):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    open(path, "w").write(body)
    os.chmod(path, mode)


def run_scenario(probe, install, *, ver=None, port="free", tcpmode=None,
                 tcpver=None, shell="sh", ak=None, agenthome=False,
                 bin_exists=False, dockermode=None, compose=False):
    tmp = tempfile.mkdtemp(prefix="ac-ensure-")
    home, fakes, state = (os.path.join(tmp, d) for d in ("home", "fakes", "state"))
    for d in (home, fakes, state):
        os.makedirs(d)
    os.makedirs(os.path.join(home, ".local/bin"))
    os.makedirs(os.path.join(home, ".supervisor/conf.d"))
    # Conf supervisord dans le faux HOME : la branche service du script la
    # trouve si l'hôte n'a pas de conf globale (cette machine : conf réelle
    # /etc/supervisor → svc non inscriptible → restart skippé ; ailleurs :
    # conf HOME → supervisorctl factice). Les deux fins couvrent le flux.
    write(os.path.join(home, ".supervisor/conf.d/agentcontrol-daemon.conf"),
          "[program:agentcontrol-daemon]\ncommand=%s/.local/bin/agentcontrol-daemon\n" % home)
    write(os.path.join(fakes, "acctl"), FAKE_ACCTL, 0o755)
    write(os.path.join(fakes, "curl"), FAKE_CURL, 0o755)
    write(os.path.join(fakes, "install"),
          FAKE_INSTALL.replace("@REAL_INSTALL@", shutil.which("install")), 0o755)
    for b in ("supervisorctl", "systemctl", "pkill", "pgrep", "nohup"):
        write(os.path.join(fakes, b), FAKE_SVC, 0o755)
    write(os.path.join(fakes, "docker"), FAKE_DOCKER, 0o755)
    # Traces « managé ailleurs » (BUG C) — chacune isolée par scénario.
    if ak is not None:
        write(os.path.join(home, ".ssh/authorized_keys"), ak)
    if agenthome:
        write(os.path.join(home, ".agentcontrol/config.toml"), "[x]\n")
    if bin_exists:
        # Binaire natif déjà posé (démon installé puis arrêté) —
        # ~/.agentcontrol seul n'est alors plus une trace « managé ».
        write(os.path.join(home, ".local/bin/agentcontrol-daemon"),
              "#!/bin/sh\nexit 0\n", 0o755)
    if compose:
        write(os.path.join(home, "dev/AgentControl/.devcontainer/compose.yml"),
              "services:\n  dev:\n    ports:\n      - \"127.0.0.1:%s:%s\"\n" % (PORT, PORT))
    if ver is not None:
        write(os.path.join(state, "ver"), ver)
    write(os.path.join(state, "portstate"), port)
    if tcpmode:
        write(os.path.join(state, "tcpmode"), tcpmode)
    if tcpver:
        write(os.path.join(state, "tcpver"), tcpver)
    if dockermode:
        write(os.path.join(state, "dockermode"), dockermode)
    # Artefact « téléchargé + vérifié » par l'app (phases download/
    # integrity vivent côté client en Rust) — le script d'install ne
    # fait que `cat > acd.tgz` : stdin = le tarball.
    tgz = os.path.join(state, "acd.tgz")
    make_tgz(tgz)
    env = {
        "HOME": home,
        "PATH": fakes + os.pathsep + os.defpath,
        "AC_STATE": state,
        "AGENTCONTROL_SOCK": home + "/.agentcontrol/daemon.sock",
    }
    # Phase 1 — sonde : `ok:*`/`newer:*`/`busy:` terminent ici,
    # `install:<arch>` continue.
    p = subprocess.run([shell, "-c", probe], env=env,
                       capture_output=True, text=True, timeout=90)
    out = p.stdout.strip().splitlines()
    last = out[-1] if out else ""
    err = p.stderr
    if last == "install:x86_64" or last == "install:aarch64":
        # Phase 3 — push stdin (phase 2 download+vérif vit en Rust) +
        # install : le tarball part tel quel, comme le ferait l'app.
        with open(tgz, "rb") as f:
            p = subprocess.run([shell, "-c", install], env=env, stdin=f,
                               capture_output=True, text=True, timeout=90)
        out = p.stdout.strip().splitlines()
        err = (err or "") + p.stderr
    calls = ""
    try:
        calls = open(os.path.join(state, "calls.log")).read()
    except OSError:
        pass
    # Preuve d'install : le binaire a été déposé dans le faux HOME.
    installed = os.path.exists(os.path.join(home, ".local/bin/agentcontrol-daemon"))
    return out, err, calls, installed


AK_PERMITOPEN = ('restrict,permitopen="127.0.0.1:%s" ssh-ed25519 AAAA'
                 ' agentcontrol-pair host 1700000000\n' % PORT)
AK_PLAIN = 'ssh-ed25519 AAAA utilisateur@laptop\n'


def main():
    probe, install = extract_scripts()
    assert "AGENTCONTROL_TCP" in probe, "sonde d'identité absente du script"
    assert "cat > acd.tgz" in install, "l'install doit recevoir l'artefact par stdin (I1)"
    assert "github.com" not in install and "curl" not in install, \
        "l'install ne doit plus télécharger (download+vérif côté app)"
    # {f} = flag « install anyway » confirmé par l'utilisateur.
    probe0, probe1 = probe.replace("{f}", "0"), probe.replace("{f}", "1")
    cases = [
        # (nom, kwargs, stdout attendu (dernière ligne), install attendu)
        ("à jour",               dict(ver=TAG), f"ok:{TAG}", False),
        ("démon > tag",          dict(ver="10.0.0"), "newer:10.0.0", False),
        ("ver<tag, port libre",  dict(ver="1.0.0"), f"installed:{TAG}", True),
        ("ver<tag, port = notre démon",
                                 dict(ver="1.0.0", port="up", tcpmode="ours"),
                                 f"installed:{TAG}", True),
        ("ver<tag, service étranger",
                                 dict(ver="1.0.0", port="up", tcpmode="http"),
                                 f"busy:127.0.0.1:{PORT}", False),
        ("ver<tag, autre démon (v≠)",
                                 dict(ver="1.0.0", port="up", tcpmode="other", tcpver="8.8.8"),
                                 f"busy:127.0.0.1:{PORT}", False),
        ("ver<tag, listener muet",
                                 dict(ver="1.0.0", port="up", tcpmode="hang"),
                                 f"busy:127.0.0.1:{PORT}", False),
        ("démon down, port occupé", dict(port="up"),
                                 f"busy:127.0.0.1:{PORT}", False),
        ("démon down, port libre", dict(), f"installed:{TAG}", True),
        # BUG C — traces « managé ailleurs » : managed:<detail>, rien
        # d'installé tant que l'utilisateur n'a pas confirmé. La garde
        # ne s'applique que si AUCUN démon ne répond (ver vide).
        ("~/.agentcontrol sans binaire", dict(agenthome=True),
                                 "managed:agentcontrol-home", False),
        ("démon natif arrêté (home+bin)", dict(agenthome=True, bin_exists=True),
                                 f"installed:{TAG}", True),
        ("démon natif répond + home", dict(ver="1.0.0", agenthome=True),
                                 f"installed:{TAG}", True),
        ("démon natif répond + authorized_keys",
                                 dict(ver="1.0.0", ak=AK_PERMITOPEN),
                                 f"installed:{TAG}", True),
        ("authorized_keys permitopen", dict(ak=AK_PERMITOPEN),
                                 "managed:authorized-keys", False),
        ("authorized_keys marqueur", dict(ak=AK_PERMITOPEN.replace(
            'permitopen="127.0.0.1:%s" ' % PORT, "")),
                                 "managed:authorized-keys", False),
        ("authorized_keys clé neutre", dict(ak=AK_PLAIN),
                                 f"installed:{TAG}", True),
        ("conteneur docker",      dict(dockermode="hit"),
                                 "managed:docker", False),
        ("compose file port",     dict(compose=True),
                                 "managed:compose-file", False),
        # « Install anyway » ({f}=1) : les gardes managé sont sautées.
        ("managed + install anyway", dict(agenthome=True, force=True),
                                 f"installed:{TAG}", True),
        ("docker absent/ko",      dict(dockermode=None),
                                 f"installed:{TAG}", True),
    ]
    shells = [s for s in ("sh", "bash") if shutil.which(s)]
    fails = 0
    for name, kw, want, installs in cases:
        kw = dict(kw)
        script = probe1 if kw.pop("force", False) else probe0
        for sh in shells:
            out, err, calls, installed = run_scenario(
                script, install, shell=sh, **kw)
            last = out[-1] if out else "(aucune sortie)"
            ok = last == want and installed == installs
            fails += not ok
            print("%s %-30s [%s] → %-26s install=%s" % (
                "PASS" if ok else "FAIL", name, sh, last, installed))
            if not ok:
                print("  attendu: %r install=%s | stderr: %s | calls: %s" % (
                    want, installs, err.strip()[:300], calls.strip()[:200]))
    print("=" * 64)
    print("échecs: %d" % fails)
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()
