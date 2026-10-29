#!/usr/bin/env python3
"""Génère la description de la release PR : notes de version lisibles par
des utilisateurs (sections Nouveautés / Corrections / Améliorations),
construites à partir des sujets de squash mergés sur next_release dans le
cycle courant.

Version affichée = prochaine release calculée comme release-bump
(version.yml) : borne = dernier commit `chore: bump version` dont le tag
vX.Y.Z existe, sinon dernier tag semver ; niveau = mot-clé parenthésé le
plus fort du range `(major)` > `(minor)`|`(sync)` > patch. Toute
divergence entre les deux implémentations doit être corrigée en les
gardant synchrones.

Le bloc généré est encadré de marqueurs `release-notes:start/end` : en
mode --merge-into, seule la zone entre marqueurs est remplacée — tout
texte hors marqueurs (annotation humaine) est préservé.

Usage :
  release_notes.py [--ref HEAD] [--date YYYY-MM-DD] [--out FILE]
                   [--merge-into BODY_FILE]
"""
import argparse
import datetime
import json
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

MARK_START = "<!-- release-notes:start -->"
MARK_END = "<!-- release-notes:end -->"

# type conventional -> section user-facing ; les autres types (docs,
# test, chore, ci, build, refactor, style, …) sont internes et exclus.
CATEGORIES = [
    ("feat", "Nouveautés"),
    ("fix", "Corrections"),
    ("perf", "Améliorations"),
]

SUBJECT = re.compile(r"^(\w+)(?:\([^)]*\))?!?\s*:\s*(.+?)\s*(?:\(#(\d+)\)\s*)?$")
SEMVER = re.compile(r"^v\d+\.\d+\.\d+$")
# Mots-clés de bump : info pipeline, jamais montrée à l'utilisateur.
KEYWORD = re.compile(r"\s*\((?:major|minor|sync|patch)\)", re.IGNORECASE)

# Bloc process interne, replié : comment la release se publie. Miroir du
# commentaire d'en-tête de auto-release-pr.yml.
PROCESS = """<details><summary>Processus de publication (interne)</summary>

Pour releaser : passer la PR **ready** → `release-bump` pousse le bump de
version (`dernier tag + niveau`, mots-clés `(minor)`/`(major)`/`(sync)` des
commits du cycle) → les 5 checks tournent sur le head bumpé et un tag
`v<NEXT>-rc.<k>` déclenche le build de la **prerelease** — installable depuis
l'app via l'auto-update en activant le canal rc dans les paramètres, pour
tester avant de merger → merge → `release-on-merge` tague `v<NEXT>`,
dispatche le build de release final et nettoie les rc du cycle.

</details>"""


def git(*args):
    return subprocess.run(["git", "-C", ROOT, *args], check=True,
                          capture_output=True, text=True).stdout


def semver_tags():
    """Tags vX.Y.Z (rc exclus) du plus récent au plus ancien."""
    out = git("tag", "-l", "v*", "--sort=-version:refname")
    return [t for t in out.splitlines() if SEMVER.match(t)]


def find_bound(ref):
    """Dernier bump DEJA RELEASE = commit `chore: bump version` dont le
    tag vX.Y.Z existe ; sinon dernier tag semver ; sinon None (racine)."""
    for c in git("rev-list", "--grep=^chore: bump version", ref).split():
        # Dernière occurrence X.Y.Z du sujet — comme `tail -1` de
        # version.yml, les deux implémentations restent synchrones.
        v = re.findall(r"\d+\.\d+\.\d+", git("log", "-1", "--format=%s", c))
        if v and f"v{v[-1]}" in semver_tags():
            return c
    tags = semver_tags()
    return tags[0] if tags else None


def bump_level(rev_range):
    msgs = git("log", "--format=%B", rev_range)
    if re.search(r"\(major\)", msgs, re.IGNORECASE):
        return "major"
    if re.search(r"\((?:minor|sync)\)", msgs, re.IGNORECASE):
        return "minor"
    return "patch"


def next_version(rev_range):
    """Dernier tag semver + niveau ; base package.json si aucun tag."""
    tags = semver_tags()
    if tags:
        base = tags[0][1:]
    else:
        pkg = json.load(open(os.path.join(ROOT, "client", "package.json")))
        base = pkg["version"].split("-")[0]
    ma, mi, pa = (int(x) for x in base.split("."))
    level = bump_level(rev_range)
    return {
        "major": f"{ma + 1}.0.0",
        "minor": f"{ma}.{mi + 1}.0",
        "patch": f"{ma}.{mi}.{pa + 1}",
    }[level]


def entries(rev_range):
    """Sujets du range (chronologique) -> {section: [(titre, pr)]}."""
    out = {name: [] for _, name in CATEGORIES}
    types = {t: name for t, name in CATEGORIES}
    subjects = git("log", "--format=%s", "--reverse", rev_range).splitlines()
    for s in subjects:
        m = SUBJECT.match(s)
        if not m or m.group(1) not in types:
            continue
        title = KEYWORD.sub("", m.group(2)).strip()
        if not title:
            continue
        title = title[0].upper() + title[1:]
        out[types[m.group(1)]].append((title, m.group(3)))
    return out


def render(version, date, ent):
    lines = [MARK_START,
             f"## Notes de version — v{version}",
             "",
             f"_Générées automatiquement — dernière mise à jour : {date}._",
             ""]
    written = False
    for _, name in CATEGORIES:
        if not ent[name]:
            continue
        written = True
        lines.append(f"### {name}")
        lines += [f"- {t}" + (f" (#{p})" if p else "") for t, p in ent[name]]
        lines.append("")
    if not written:
        lines += ["Cette version contient des améliorations internes et "
                  "de la maintenance.", ""]
    lines += [PROCESS, "", MARK_END, ""]
    return "\n".join(lines)


def merge_into(current, block):
    """Remplace la zone marquée de `current` par `block` ; si la body n'a
    pas de marqueurs (legacy ou vierge), le bloc seul fait foi."""
    i, j = current.find(MARK_START), current.find(MARK_END)
    if i == -1 or j == -1 or j < i:
        return block
    tail = current[j + len(MARK_END):]
    return current[:i] + block + (tail if tail.strip() else "")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ref", default="HEAD")
    ap.add_argument("--date", default=datetime.date.today().isoformat())
    ap.add_argument("--out", help="fichier de sortie (défaut: stdout)")
    ap.add_argument("--merge-into", metavar="FILE",
                    help="body existante : remplace la zone marquée, "
                         "préserve le contenu hors marqueurs")
    a = ap.parse_args()

    bound = find_bound(a.ref)
    rev_range = f"{bound}..{a.ref}" if bound else a.ref
    body = render(next_version(rev_range), a.date, entries(rev_range))
    if a.merge_into:
        body = merge_into(open(a.merge_into).read(), body)
    if a.out:
        open(a.out, "w").write(body)
    else:
        sys.stdout.write(body)


if __name__ == "__main__":
    main()
