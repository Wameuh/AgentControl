#!/usr/bin/env python3
"""PreToolUse/Stop/SessionStart hook: keep the MAIN worktree pristine.

The primary checkout must stay on `next_release` with no local changes —
all work happens in linked worktrees (`git worktree add`, convention
`~/worktrees/ac-*`).

PreToolUse:
- write/edit/apply_patch/notebook_edit: block file paths that resolve
  inside the main worktree (linked worktrees nested under it excluded).
- exec/write_to_process: block git commands that would mutate the main
  worktree's files, index or HEAD attachment — commit, add, rm, mv,
  clean, stash (mutating forms), cherry-pick, revert, am, apply, rebase,
  merge, bisect, update-index, update-ref/symbolic-ref writes,
  read-tree, checkout-index, filter-branch, checkout/switch to anything
  but next_release, restore --source, branch -f/-d/-m touching
  next_release, submodule update/add/deinit.
  Allowed: read-only git, fetch/pull/reset (documented resync path),
  restore and `checkout -- <paths>` (revert local edits toward clean),
  `checkout|switch next_release`, worktree add/remove/lock.
  The effective repo dir is tracked per command segment (exec `workdir`,
  cd/pushd, `git -C`, --git-dir/--work-tree, GIT_DIR/GIT_WORK_TREE env)
  and compared against `git worktree list` roots.
- mcp__* mutation-named tools (write/edit/create/delete/rename/move/…):
  block absolute-path arguments inside the main worktree.

Stop: refuse to stop while the main worktree carries tracked
modifications or sits on a branch other than next_release — backstop
for shell-side edits the exec scan cannot see. SessionStart warns
about the same state.

Tripwire, not a security boundary (same stance as block-merge.py):
each shell's persistent cwd is not observable, so `cd`-then-mutate
split across two tool calls escapes the exec scan, and plain shell
writes (`>`, `sed -i`, `rm`) are not parsed — the Stop check catches
the tracked dirt they leave behind.
"""
import json
import os
import re
import shlex
import subprocess
import sys

REPO = os.path.dirname(os.path.dirname(os.path.dirname(
    os.path.realpath(__file__))))
PROTECTED_BRANCH = "next_release"

PATH_KEYS = ("file_path", "notebook_path", "path", "target_file")
MCP_MUTATE = re.compile(
    r"write|edit|creat|delet|rename|move|put|upload|patch|remove")
WRAPPERS = {"sudo", "doas", "command", "builtin", "env", "nice", "time"}
# git global options that consume a separate value token
VAL_OPTS = {"-C", "-c", "--git-dir", "--work-tree", "--namespace",
            "--exec-path"}

FLAT_BLOCK = {
    "commit", "add", "stage", "rm", "mv", "clean", "cherry-pick",
    "revert", "am", "apply", "rebase", "merge", "bisect",
    "update-index", "update-ref", "read-tree", "checkout-index",
    "filter-branch",
}

REASON = (
    "Worktree principal verrouillé : aucun changement local — il reste "
    "propre sur `next_release`. Travailler dans un worktree lié "
    "(`git worktree add ~/worktrees/ac-<feat> -b <br> origin/next_release`). "
    "Restent permis ici : lecture, fetch/pull/reset, restore/checkout -- "
    "(retour au propre), `checkout|switch next_release`."
)


def run(cmd, timeout=5):
    try:
        return subprocess.run(cmd, capture_output=True, text=True,
                              timeout=timeout).stdout.strip()
    except Exception:
        return ""


def real(p):
    return os.path.realpath(os.path.expanduser(p))


def worktree_roots():
    out = run(["git", "-C", REPO, "worktree", "list", "--porcelain"])
    return [l[9:] for l in out.splitlines() if l.startswith("worktree ")]


def under(p, root):
    try:
        return os.path.commonpath([p, root]) == root
    except ValueError:
        return False


def in_main_worktree(path, roots=None):
    """True iff `path` resolves inside the main worktree but outside any
    linked worktree nested under it."""
    if not path:
        return False
    roots = roots if roots is not None else worktree_roots()
    if not roots:
        return False
    rp, main = real(path), real(roots[0])
    if not under(rp, main):
        return False
    return not any(under(rp, real(r)) for r in roots[1:])


def toplevel(d):
    """Absolute worktree root containing dir `d`, or ''."""
    return run(["git", "-C", d, "rev-parse", "--path-format=absolute",
                "--show-toplevel"])


def targets_main(cwd, cdir, gitdir, roots):
    """True iff a git invocation resolves to the main worktree."""
    main = real(roots[0])
    if gitdir:
        if real(os.path.join(cwd, gitdir)) != os.path.join(main, ".git"):
            return False
        # --work-tree elsewhere wins over the main gitdir
        return not cdir or toplevel(os.path.join(cwd, cdir)) == main
    d = os.path.join(cwd, cdir) if cdir else cwd
    return toplevel(d) == main


def git_invocation(toks):
    """Locate a `git` invocation in the tokens (past env assignments and
    wrappers like sudo) → (verb, args, cdir, gitdir) or None."""
    env = {}
    i, n = 0, len(toks)
    while i < n and re.fullmatch(r"[A-Za-z_]\w*=.*", toks[i]):
        k, v = toks[i].split("=", 1)
        env[k] = v
        i += 1
    hops = 0
    while i < n and os.path.basename(toks[i]) in WRAPPERS \
            and hops < 6:
        i += 1
        hops += 1
        while i < n and toks[i].startswith("-") and hops < 6:
            i += 1  # wrapper flags (sudo -u …)
    if i >= n or os.path.basename(toks[i]) != "git":
        return None
    i += 1
    cdir = env.get("GIT_WORK_TREE")
    gitdir = env.get("GIT_DIR")
    while i < n:
        t = toks[i]
        if t == "--" or not t.startswith("-") or t == "-":
            break
        if t in VAL_OPTS:
            if i + 1 < n:
                if t in ("-C", "--work-tree"):
                    cdir = toks[i + 1]
                elif t == "--git-dir":
                    gitdir = toks[i + 1]
                i += 2
            else:
                i += 1
            continue
        m = re.match(r"--(git-dir|work-tree)=(.*)", t)
        if m:
            if m.group(1) == "git-dir":
                gitdir = m.group(2)
            else:
                cdir = m.group(2)
        i += 1
    if i >= n:
        return None
    return toks[i], toks[i + 1:], cdir, gitdir


def positionals(args):
    """Non-flag args before an eventual `--` (bare `-` counts)."""
    before = args[:args.index("--")] if "--" in args else args
    return [a for a in before if a == "-" or not a.startswith("-")]


def verb_ok(verb, args):
    """Whether `git <verb> <args>` leaves the main worktree untouched."""
    if verb in FLAT_BLOCK:
        return False
    flags = {a for a in args if a.startswith("-") and a != "-"}
    pos = positionals(args)
    if verb == "checkout":
        if flags & {"-b", "-B", "--detach", "-d", "--orphan", "-t",
                    "--track"}:
            return False
        return not pos or pos == [PROTECTED_BRANCH] or pos[0] == "."
    if verb == "switch":
        if flags & {"-c", "-C", "--create", "--detach", "-d", "--orphan"}:
            return False
        return pos == [PROTECTED_BRANCH]
    if verb == "restore":
        return not any(a == "--source" or a.startswith("--source=")
                       or a == "-s" for a in args)
    if verb == "stash":
        return bool(pos) and pos[0] in ("list", "show")
    if verb == "branch":
        danger = flags & {"-f", "--force", "-m", "-M", "--move",
                          "-d", "-D", "--delete"}
        if not danger:
            return True
        if flags & {"-m", "-M", "--move"} and len(pos) <= 1:
            return False  # renames the current (protected) branch
        return PROTECTED_BRANCH not in pos
    if verb == "symbolic-ref":
        return not (len(pos) >= 2 or flags & {"-d", "--delete"})
    if verb == "submodule":
        return not pos or pos[0] not in ("update", "add", "deinit")
    return True


def block():
    print(json.dumps({"decision": "block", "reason": REASON}))


def check_file_tool(ti):
    for k in PATH_KEYS:
        if in_main_worktree(str(ti.get(k) or "")):
            return block()
    if ti.get("patch") or ti.get("input"):  # apply_patch-style bodies
        body = f"{ti.get('patch') or ''}\n{ti.get('input') or ''}"
        base = os.environ.get("DEVIN_PROJECT_DIR", REPO)
        for m in re.finditer(
                r"(?:^\+\+\+ b/|^\*\*\* \w+ File: |^--- a/)(.+)$",
                body, re.M):
            if in_main_worktree(os.path.join(base, m.group(1).strip())):
                return block()


def check_exec(ti, cwd):
    cmd = " ".join(str(ti.get(k) or "")
                   for k in ("command", "text_input", "bytes_input"))
    roots = worktree_roots()
    if not roots:
        return
    for seg in re.split(r"&&|\|\||[;|]", cmd):
        try:
            toks = shlex.split(seg)
        except ValueError:
            continue
        if not toks:
            continue
        m = re.fullmatch(r"(?:cd|pushd)", toks[0])
        if m and len(toks) > 1 and not toks[1].startswith("-"):
            cwd = os.path.normpath(os.path.join(cwd, toks[1]))
            continue
        inv = git_invocation(toks)
        if not inv:
            continue
        verb, args, cdir, gitdir = inv
        if not targets_main(cwd, cdir, gitdir, roots):
            continue
        if not verb_ok(verb, args):
            return block()


def check_mcp(ti):
    def paths(v):
        if isinstance(v, str) and re.match(r"^(~|/)", v):
            yield v
        elif isinstance(v, dict):
            for x in v.values():
                yield from paths(x)
        elif isinstance(v, list):
            for x in v:
                yield from paths(x)
    for p in paths(ti):
        if in_main_worktree(p):
            return block()


def main_state():
    """(branch, dirty tracked entries) of the main worktree."""
    roots = worktree_roots()
    if not roots:
        return "", []
    out = run(["git", "-C", roots[0], "status", "--porcelain=v1", "-b"])
    branch, dirty = "", []
    for l in out.splitlines():
        if l.startswith("## "):
            m = re.match(r"(\S+?)(?:\.\.\.|$)", l[3:])
            branch = m.group(1) if m else ""
        elif not l.startswith("??"):
            dirty.append(l)
    return branch, dirty


def state_reason(branch, dirty):
    bits = []
    if branch != PROTECTED_BRANCH:
        bits.append(f"branche `{branch or 'HEAD détaché'}` ≠ "
                    f"`{PROTECTED_BRANCH}`")
    if dirty:
        shown = ", ".join(x[3:].strip() for x in dirty[:5])
        bits.append(f"{len(dirty)} fichier(s) tracké(s) modifié(s) "
                    f"({shown}{', …' if len(dirty) > 5 else ''})")
    return "Worktree principal souillé : " + "; ".join(bits) + (
        ". Restaurer l'état propre (git switch " + PROTECTED_BRANCH +
        ", git restore/git reset --hard, ou finir depuis un worktree).")


def check_state(block_stop):
    branch, dirty = main_state()
    if branch == PROTECTED_BRANCH and not dirty:
        return
    reason = state_reason(branch, dirty)
    if block_stop:
        print(json.dumps({"decision": "block", "reason": reason}))
    else:
        print(json.dumps({"hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": reason}}))


def main():
    try:
        data = json.load(sys.stdin)
    except Exception:
        return 0
    ev = data.get("hook_event_name")
    if ev == "Stop":
        if not data.get("stop_hook_active"):
            check_state(block_stop=True)
        return 0
    if ev == "SessionStart":
        check_state(block_stop=False)
        return 0
    ti = data.get("tool_input") or {}
    name = data.get("tool_name") or ""
    cwd = ti.get("workdir") or os.environ.get("DEVIN_PROJECT_DIR") or REPO
    if name in ("exec", "write_to_process"):
        check_exec(ti, cwd)
    elif name.startswith("mcp__"):
        if MCP_MUTATE.search(name):
            check_mcp(ti)
    else:  # write / edit / apply_patch / notebook_edit
        check_file_tool(ti)
    return 0


if __name__ == "__main__":
    sys.exit(main())
