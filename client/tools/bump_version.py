#!/usr/bin/env python3
"""Bump la version partout : tauri.conf.json, package.json, Cargo.toml.
Usage : python3 client/tools/bump_version.py [x.y.z[-rc.n] | major|minor|patch]
Sans argument : incrémente le patch. Politique : y = incompatibilité
client↔démon (déclenche l'auto-update du serveur), z = correctifs.
Le suffixe explicite -rc.n sert au build des prereleases (injection au
build, jamais commitée).

Mode --detect-level : lit les messages de commit du cycle sur stdin
(séparés par \x1e — `git log --format=%x1e%B`) et imprime le niveau
de bump — source de vérité de la règle mots-clés → niveau, partagée
par version.yml (release-bump) et release_notes.py :
  git log --format=%x1e%B v1.2.3..HEAD \
    | python3 client/tools/bump_version.py --detect-level

Mode --bound [rev] : imprime la borne de la plage de scan (sha de
commit ou nom de tag) — dernier bump déjà release (son tag vX.Y.Z
existe) ou dernier marqueur de reset/revert de bump, sinon dernier
tag semver. Partagée par version.yml (release-bump) et
bump-bound-test.sh :
  python3 client/tools/bump_version.py --bound HEAD"""
import json
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
FILES = [
    os.path.join(ROOT, "client", "src-tauri", "tauri.conf.json"),
    os.path.join(ROOT, "client", "package.json"),
]
CRATES = ["proto", "forgesync", "daemon", "acctl", "client-core", "acp-core", "acp-claude"]
FILES += [os.path.join(ROOT, "crates", c, "Cargo.toml") for c in CRATES]
FILES.append(os.path.join(ROOT, "client", "src-tauri", "Cargo.toml"))


def bump(v, level="patch"):
    p = v.split("-")[0].split(".")
    i = {"major": 0, "minor": 1, "patch": 2}[level]
    p[i] = str(int(p[i]) + 1)
    for j in range(i + 1, 3):
        p[j] = "0"
    return ".".join(p)


SQUASH_SUBJ_RE = re.compile(r"\(#\d+\)\s*$")


def _keyword_scope(msgs):
    """Texte où les mots-clés comptent : un commit = un enregistrement
    \x1e (format `git log --x1e... %B`). Par commit on ne retient que
    le sujet (1re ligne) et — pour les squash GitHub, sujets terminant
    par `(#N)` — les puces `* ...` du body qui reprennent les sujets
    des commits d'origine. Le texte libre d'un body ne compte pas :
    un corps citant '(major)' (ex. une note d'incident) ne doit pas
    déclencher un bump — incident #334 re-bumpé en 1.0.0 par #441."""
    keep = []
    for rec in msgs.split("\x1e"):
        lines = rec.splitlines()
        if not lines:
            continue
        keep.append(lines[0])
        if SQUASH_SUBJ_RE.search(lines[0]):
            keep.extend(l for l in lines[1:] if l.startswith("* "))
    return "\n".join(keep)


def detect_level(msgs):
    """Niveau de bump depuis les messages du cycle — voir _keyword_scope
    pour ce qui est retenu. Mot-clé parenthésé le plus fort :
    (major) > (sync) ; aucun → patch.
    (sync) = seul déclencheur de minor : incompatibilité protocole
    démon↔client — c'est le bump x.y qui déclenche l'auto-update.
    (minor) est déprécié : ignoré, traité comme patch (une feature sans
    casse protocole reste compatible → le client patché reste pair).
    Convention : jamais un mot nu — `a major bug` ou `sync retry` en
    prose ne comptent pas."""
    scope = _keyword_scope(msgs)
    level = ("major" if re.search(r"\(major\)", scope, re.IGNORECASE)
             else "minor" if re.search(r"\(sync\)", scope, re.IGNORECASE)
             else "patch")
    if re.search(r"\(minor\)", scope, re.IGNORECASE):
        print(f"note : (minor) déprécié — ignoré (niveau retenu : "
              f"{level} ; seul (sync) monte en minor)", file=sys.stderr)
    return level


SEMVER_RE = re.compile(r"\d+\.\d+\.\d+")
TAG_RE = re.compile(r"v\d+\.\d+\.\d+")


def _git(*args, check=True):
    return subprocess.run(["git", *args], check=check,
                          capture_output=True, text=True).stdout


def scan_bound(head="HEAD"):
    """Borne de la plage de scan des mots-clés de bump.

    Candidats (rev-list, plus récent d'abord) : sujets de bump
    (`chore: bump version`), reverts de bump et commits `(reset` —
    un revert/reset marque une borne de cycle même sans tag : un bump
    annulé ne doit pas laisser la plage remonter jusqu'au tag
    précédent (le `(major)` du cycle annulé serait ré-appliqué —
    incident #435/#436). Pour chaque candidat on retient X.Y.Z du
    sujet et on l'accepte si le tag vX.Y.Z existe (bump déjà release)
    ou si le sujet porte `(reset` / un revert de bump. Le bump poussé
    par ce job (tag pas encore créé) est sauté → la plage couvre le
    cycle entier même sur re-run. Aucun candidat retenu → dernier
    tag semver (post-reset : nr = tip main, plus de commit bump)."""
    candidates = _git(
        "rev-list",
        "--grep=^chore: bump version",
        "--grep=revert bump",
        "--grep=(reset",
        head).split()
    for c in candidates:
        subj = _git("log", "-1", "--format=%s", c).strip()
        vers = SEMVER_RE.findall(subj)
        if not vers:
            continue
        tagged = subprocess.run(
            ["git", "rev-parse", "-q", "--verify",
             f"refs/tags/v{vers[-1]}"],
            capture_output=True).returncode == 0
        if (tagged or "(reset" in subj
                or re.search(r"revert.*bump", subj, re.IGNORECASE)):
            return c
    for t in _git("tag", "-l", "v*", "--sort=-version:refname").split():
        if TAG_RE.fullmatch(t):
            return t
    return ""


def main():
    # Usage: bump_version.py [x.y.z | major|minor|patch]  (défaut: patch)
    if "--detect-level" in sys.argv[1:]:
        if len(sys.argv) != 2:
            raise SystemExit("--detect-level ne prend pas d'autre argument")
        print(detect_level(sys.stdin.read()))
        return
    args = sys.argv[1:]
    if "--bound" in args:
        rest = [a for a in args if a != "--bound"]
        if len(rest) > 1:
            raise SystemExit("--bound prend au plus une révision")
        print(scan_bound(rest[0] if rest else "HEAD"))
        return
    explicit = None
    level = "patch"
    for a in sys.argv[1:]:
        if a in ("major", "minor", "patch"):
            level = a
        elif re.match(r"^\d+\.\d+\.\d+(-rc\.\d+)?$", a):
            explicit = a
        else:
            raise SystemExit(f"argument inconnu: {a} (attendu x.y.z ou major|minor|patch)")

    new = None
    for f in FILES:
        if f.endswith(".json"):
            c = json.load(open(f))
            if new is None:
                new = explicit or bump(c["version"], level)
            c["version"] = explicit or bump(c["version"], level)
            json.dump(c, open(f, "w"), indent=2)
            open(f, "a").write("\n")
        else:
            s = open(f).read()
            s2, n = re.subn(r'^version = "(\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)"',
                            lambda m: f'version = "{explicit or bump(m.group(1), level)}"', s, count=1, flags=re.M)
            if n:
                open(f, "w").write(s2)
    print("version ->", new)


if __name__ == "__main__":
    main()
