#!/usr/bin/env python3
"""Bump la version partout : tauri.conf.json, package.json, Cargo.toml.
Usage : python3 client/tools/bump_version.py [x.y.z[-rc.n] | major|minor|patch]
Sans argument : incrémente le patch. Politique : y = incompatibilité
client↔démon (déclenche l'auto-update du serveur), z = correctifs.
Le suffixe explicite -rc.n sert au build des prereleases (injection au
build, jamais commitée).

Mode --detect-level : lit les messages de commit du cycle sur stdin et
imprime le niveau de bump — source de vérité de la règle mots-clés →
niveau, partagée par version.yml (release-bump) et release_notes.py :
  git log --format=%B v1.2.3..HEAD \
    | python3 client/tools/bump_version.py --detect-level"""
import json
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
FILES = [
    os.path.join(ROOT, "client", "src-tauri", "tauri.conf.json"),
    os.path.join(ROOT, "client", "package.json"),
]
CRATES = ["proto", "forgesync", "daemon", "acctl", "client-core"]
FILES += [os.path.join(ROOT, "crates", c, "Cargo.toml") for c in CRATES]
FILES.append(os.path.join(ROOT, "client", "src-tauri", "Cargo.toml"))


def bump(v, level="patch"):
    p = v.split("-")[0].split(".")
    i = {"major": 0, "minor": 1, "patch": 2}[level]
    p[i] = str(int(p[i]) + 1)
    for j in range(i + 1, 3):
        p[j] = "0"
    return ".".join(p)


def detect_level(msgs):
    """Niveau de bump depuis les messages du cycle (sujets + bodies —
    le squash reprend les sujets en body). Mot-clé parenthésé le plus
    fort : (major) > (sync) ; aucun → patch.
    (sync) = seul déclencheur de minor : incompatibilité protocole
    démon↔client — c'est le bump x.y qui déclenche l'auto-update.
    (minor) est déprécié : ignoré, traité comme patch (une feature sans
    casse protocole reste compatible → le client patché reste pair).
    Convention : jamais un mot nu — `a major bug` ou `sync retry` en
    prose ne comptent pas."""
    level = ("major" if re.search(r"\(major\)", msgs, re.IGNORECASE)
             else "minor" if re.search(r"\(sync\)", msgs, re.IGNORECASE)
             else "patch")
    if re.search(r"\(minor\)", msgs, re.IGNORECASE):
        print(f"note : (minor) déprécié — ignoré (niveau retenu : "
              f"{level} ; seul (sync) monte en minor)", file=sys.stderr)
    return level


def main():
    # Usage: bump_version.py [x.y.z | major|minor|patch]  (défaut: patch)
    if "--detect-level" in sys.argv[1:]:
        if len(sys.argv) != 2:
            raise SystemExit("--detect-level ne prend pas d'autre argument")
        print(detect_level(sys.stdin.read()))
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
