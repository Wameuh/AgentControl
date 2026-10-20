#!/usr/bin/env python3
"""Bump la version patch (0.0.x) partout : tauri.conf.json, package.json,
et tous les Cargo.toml. Usage : python3 client/tools/bump_version.py"""
import json
import os
import re

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
FILES = [
    os.path.join(ROOT, "client", "src-tauri", "tauri.conf.json"),
    os.path.join(ROOT, "client", "package.json"),
]
CRATES = ["proto", "daemon", "acctl", "client-core"]
FILES += [os.path.join(ROOT, "crates", c, "Cargo.toml") for c in CRATES]
FILES.append(os.path.join(ROOT, "client", "src-tauri", "Cargo.toml"))


def bump(v):
    p = v.split(".")
    p[-1] = str(int(p[-1]) + 1)
    return ".".join(p)


new = None
for f in FILES:
    if f.endswith(".json"):
        c = json.load(open(f))
        if new is None:
            new = bump(c["version"])
        c["version"] = bump(c["version"])
        json.dump(c, open(f, "w"), indent=2)
        open(f, "a").write("\n")
    else:
        s = open(f).read()
        s2, n = re.subn(r'^version = "(\d+\.\d+\.\d+)"',
                        lambda m: f'version = "{bump(m.group(1))}"', s, count=1, flags=re.M)
        if n:
            open(f, "w").write(s2)
print("version ->", new)
