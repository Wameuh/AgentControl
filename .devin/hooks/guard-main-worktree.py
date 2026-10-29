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
  merge, bisect, update-index, sparse-checkout, read-tree,
  checkout-index, filter-branch, checkout/switch to anything but
  next_release, restore --source, symbolic-ref writes. Blocked anywhere
  in the repo (refs and config are shared across worktrees): config
  writes outside external scope (--global/--file…), remote mutations,
  branch -f/-d/-m and update-ref touching next_release, HEAD/@ (in
  main), any checked-out branch or its remote-tracking ref, fetch/pull
  refspec dst writes (`src:dst`) and local-path pushes. `-c`/
  `--config-env`/`GIT_CONFIG_*`/`export`ed alias injection is blocked
  too — it would rename the whole verb table away.
  Allowed: read-only git, fetch/pull/reset (documented resync path),
  restore and `checkout -- <paths>` (revert local edits toward clean),
  `checkout|switch next_release`, worktree add/remove/lock, config
  reads and --unset removals, external-scope config writes, remote
  pushes of non-protected branches.
  The effective repo dir is tracked per command segment (exec `workdir`,
  cd/pushd/popd with OLDPWD, `git -C`, --git-dir/--work-tree, GIT_DIR/
  GIT_WORK_TREE env) and compared against `git worktree list` roots.
  Command lines are split on `&&`/`||`/`;`/`|`/`&`/newlines/parens,
  heredoc bodies are stripped first (quote-aware, `<<<` excluded),
  `$(...)`/backtick substitutions and `bash|sh -c`/`eval` payloads are
  scanned recursively, short flags are unbundled, and shell keywords
  (then/do/if/…) plus wrappers (sudo/env/nice/…) are transparent.
- mcp__* mutation-named tools (write/edit/create/delete/rename/move/…):
  block absolute-path arguments inside the main worktree.

Stop: refuse to stop while the main worktree carries tracked
modifications, sits on a branch other than next_release, or is ahead
of origin/next_release — backstop for shell-side edits and ref moves
the exec scan cannot see. SessionStart warns about the same state.

Tripwire, not a security boundary (same stance as block-merge.py):
each shell's persistent cwd is not fully observable, plain shell
writes (`>`, `sed -i`, `rm`), `xargs`/`find -exec`-style indirection
and untracked files are out of scope — the Stop check catches the
tracked dirt and ahead-state they leave behind. Deliberate
over-blocks (safe direction): update-ref also scans the ref *value*
positional, remote update/prune/set-head are refused repo-wide, and
checked-out branches are protected from any worktree.
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
WRAPPERS = {"sudo", "doas", "command", "builtin", "env", "nice", "time",
            "nohup", "setsid", "stdbuf"}
WRAPPER_VAL = {"sudo": {"-u", "-g", "-h", "-p", "-C", "-T", "-D", "-r",
                      "-t"},
               "doas": {"-u"}, "nice": {"-n"}, "time": {"-p"},
               "stdbuf": {"-i", "-o", "-e"}}
TRANSPARENT = {"then", "do", "else", "elif", "if", "while", "until",
               "for", "fi", "done", "esac", "case", "in", "select",
               "coproc", "{", "}", "!", "time"}
SHELLS = {"bash", "sh", "dash", "zsh", "ksh"}  # `eval` handled separately
# git global options that consume a separate value token
VAL_OPTS = {"-C", "-c", "--git-dir", "--work-tree", "--namespace",
            "--exec-path", "--config-env"}
# config flags that read instead of writing / point outside the repo
CFG_READ = {"--get", "-l", "--list", "--get-all", "--get-regexp",
            "--get-color", "--get-urlmatch"}
CFG_WRITE = {"--add", "--replace-all"}
CFG_REMOVE = {"--unset", "--unset-all", "--remove-section",
              "--rename-section"}
CFG_SCOPE_EXT = {"--global", "--system", "--file", "--blob", "-f"}
CFG_VAL = {"-f", "--file", "--blob", "--default", "--type", "--value",
           "--fixed-value"}
# env vars that inject or repoint git config (alias bypass vector)
GIT_CONFIG_ENV = re.compile(
    r"GIT_CONFIG_(PARAMETERS|GLOBAL|SYSTEM|KEY_\d+|VALUE_\d+|COUNT)$")
# env vars / config values git executes — rescan their contents
EXEC_ENV = {"GIT_SSH_COMMAND", "GIT_SSH", "GIT_EXTERNAL_DIFF",
            "GIT_EDITOR", "GIT_SEQUENCE_EDITOR", "GIT_PAGER",
            "GIT_ASKPASS", "GIT_SSH_ASKPASS", "GIT_PROXY_COMMAND"}
EXEC_OPTS = {"--upload-pack", "--exec"}

FLAT_BLOCK = {
    "commit", "add", "stage", "rm", "mv", "clean", "cherry-pick",
    "revert", "am", "apply", "rebase", "merge", "bisect",
    "update-index", "read-tree", "checkout-index", "filter-branch",
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


def worktree_info():
    """(roots, checked-out short branch names) — main worktree first."""
    out = run(["git", "-C", REPO, "worktree", "list", "--porcelain"])
    roots, branches = [], set()
    for l in out.splitlines():
        if l.startswith("worktree "):
            roots.append(l[9:])
        elif l.startswith("branch "):
            branches.add(l[7:].removeprefix("refs/heads/"))
    return roots, branches


def worktree_roots():
    return worktree_info()[0]


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


def locate(cwd, cdir, gitdir, roots):
    """(in_main, in_repo) for a git invocation. With --git-dir the index
    and HEAD live in that gitdir regardless of any work-tree override,
    so the main gitdir means main unconditionally. Unknown cwd
    (`popd`/`cd -` into the void) fails closed as main."""
    main = real(roots[0])
    if gitdir:
        g = real(os.path.join(cwd or "/", gitdir))
        mg = os.path.join(main, ".git")
        return g == mg, g == mg or g.startswith(mg + os.sep)
    if cwd is None and cdir is None:
        return True, True
    d = os.path.join(cwd or "/", cdir) if cdir else cwd
    tl = toplevel(d)
    return tl == main, tl in {real(r) for r in roots}


def strip_heredocs(cmd):
    """Drop heredoc bodies so their literal lines aren't scanned as
    commands. Quote-aware (`<<` inside '…'/"…" is not an operator) and
    `<<<` here-strings are not heredocs. `<<-` tolerates leading tabs
    on the delimiter line."""
    out, pending, quote = [], [], None
    for line in cmd.split("\n"):
        if pending:
            want, dash = pending[0]
            if (line.lstrip("\t") if dash else line) == want:
                pending.pop(0)
            continue
        i, n = 0, len(line)
        while i < n:
            c = line[i]
            if quote:
                if c == quote:
                    quote = None
                elif c == "\\" and quote == '"':
                    i += 1
            elif c in "'\"":
                quote = c
            elif c == "\\":
                i += 1
            elif c == "<":
                if line[i:i + 3] == "<<<":  # here-string, not a heredoc
                    i += 3
                    continue
                m = re.match(
                    r"<<(-?)\s*(?:\\(\w+)|(['\"])([^'\"]*?)\3"
                    r"|([^\s;&|<>()'\"]+))", line[i:])
                if m:
                    pending.append(
                        (m.group(2) or m.group(4) or m.group(5),
                         bool(m.group(1))))
                    i += m.end() - 1
            i += 1
        out.append(line)
    return "\n".join(out)


def preamble(toks, env0=None):
    """Skip leading env assignments, shell keywords and wrappers →
    (env, index of the real command)."""
    env = dict(env0 or {})
    i, n = 0, len(toks)
    while i < n:
        t = toks[i]
        b = os.path.basename(t)
        if t in TRANSPARENT:
            i += 1
            continue
        if re.fullmatch(r"[A-Za-z_]\w*=.*", t):
            env[t.split("=", 1)[0]] = t.split("=", 1)[1]
            i += 1
            continue
        if b in WRAPPERS:
            i += 1
            vf = WRAPPER_VAL.get(b, ())
            while i < n:
                tt = toks[i]
                if tt in vf:
                    i += 2
                    continue
                if tt.startswith("-") and tt not in ("-", "--"):
                    i += 1
                    continue
                if re.fullmatch(r"[A-Za-z_]\w*=.*", tt):
                    env[tt.split("=", 1)[0]] = tt.split("=", 1)[1]
                    i += 1
                    continue
                break
            continue
        break
    return env, i


def git_invocation(toks, env0=None):
    """Locate a `git` invocation in the tokens → (verb, args, cdir,
    gitdir, configs, env) or None."""
    env, i = preamble(toks, env0)
    n = len(toks)
    if i >= n or os.path.basename(toks[i]) != "git":
        return None
    i += 1
    cdir = env.get("GIT_WORK_TREE")
    gitdir = env.get("GIT_DIR")
    configs = []
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
                elif t in ("-c", "--config-env"):
                    configs.append(toks[i + 1])
                i += 2
            else:
                i += 1
            continue
        m = re.match(r"--(git-dir|work-tree|config-env)=(.*)", t)
        if m:
            if m.group(1) == "git-dir":
                gitdir = m.group(2)
            elif m.group(1) == "work-tree":
                cdir = m.group(2)
            else:
                configs.append(m.group(2))
        i += 1
    if i >= n:
        return None
    return toks[i], toks[i + 1:], cdir, gitdir, configs, env


def positionals(args, val_flags=()):
    """Non-flag args before an eventual `--` (bare `-` counts);
    `val_flags` also drop their value token."""
    before = args[:args.index("--")] if "--" in args else args
    out, skip = [], False
    for a in before:
        if skip:
            skip = False
            continue
        if a in val_flags:
            skip = True
            continue
        if a == "-" or not a.startswith("-"):
            out.append(a)
    return out


def flags_of(args):
    """Flag tokens with single-dash bundles unbundled (`-rd` →
    `-r` + `-d`)."""
    out = set()
    for a in args:
        if a == "-" or not a.startswith("-"):
            continue
        if re.fullmatch(r"-[^-\s]{2,}", a):
            out.update("-" + c for c in a[1:])
        else:
            out.add(a)
    return out


def ref_tail(p):
    """Branch-ish tail of a ref: strip refs/, refs/heads/,
    refs/remotes/<r>/ and a leading '+'."""
    return re.sub(r"^refs/(heads/|remotes/[^/]+/)?", "",
                  p.lstrip("+"))


def prot_hit(p, names):
    """True iff positional `p` names a protected branch — as a full
    name (`feat/x`), a ref tail (`refs/remotes/o/next_release`) or a
    bare remote-tracked form (`origin/next_release`)."""
    t = ref_tail(p)
    return t in names or t.split("/", 1)[-1] in names


def verb_ok(verb, args):
    """Whether `git <verb> <args>` leaves the main worktree untouched."""
    if verb in FLAT_BLOCK:
        return False
    flags = flags_of(args)
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
                       or (a.startswith("-") and not a.startswith("--")
                           and "s" in a[1:]) for a in args)
    if verb == "stash":
        return bool(pos) and pos[0] in ("list", "show")
    if verb == "sparse-checkout":
        return bool(pos) and pos[0] == "list"
    if verb == "submodule":
        return not pos or pos[0] not in ("update", "add", "deinit")
    return True


def repo_verb_ok(verb, args, in_main, checked, cwd=None):
    """Whether `git <verb> <args>` is safe anywhere in the repo — refs,
    remotes and config are shared across all worktrees. `checked` holds
    the short names of branches checked out in some worktree."""
    flags = flags_of(args)
    protected = {PROTECTED_BRANCH} | checked
    names = protected | ({"HEAD", "@"} if in_main else set())
    if verb == "config":
        pos = positionals(args, CFG_VAL)
        name = (pos[0] if pos else "").lower()
        if flags & CFG_REMOVE or flags & CFG_READ:
            return True
        if len(pos) <= 1 and not (flags & CFG_WRITE):
            return True  # `config name` read form
        if name.startswith("alias."):
            return False
        # write form: only external scope (--global/--file…) is allowed;
        # --worktree is per-worktree — fine in linked ones only.
        ext = CFG_SCOPE_EXT | ({"--worktree"} if not in_main else set())
        return bool(flags & ext)
    if verb == "remote":
        pos = positionals(args)
        # bare `remote`/`-v` list; `show`/`get-url` read — rest mutates
        # the shared config
        return not pos or pos[0] in ("show", "get-url")
    if verb in ("fetch", "pull"):
        pos = positionals(args)
        # refspec dst writes (`+src:dst`) can forge remote-tracking refs
        # — the ahead-check baseline lives under refs/remotes/origin/
        if any(":" in s for s in pos[1:]) or any(
                a.startswith("--refmap") and ":" in a for a in args):
            return False
        return True
    if verb == "push":
        pos = positionals(args)
        # `push <local dir> src:dst` writes LOCAL refs → protect every
        # checked-out branch; remote-side pushes only care about
        # next_release (direct pushes to it are forbidden anyway).
        if not pos or len(pos) == 1:
            return not in_main  # bare push pushes the current branch
        local = pos[0] in (".", "..") or os.path.isdir(
            os.path.join(cwd or "/", pos[0]))
        names = names if local else {PROTECTED_BRANCH}
        return not any(
            prot_hit(s.split(":", 1)[-1], names) for s in pos[1:])
    if verb == "branch":
        pos = positionals(args, {"-u", "--set-upstream-to"})
        danger = flags & {"-f", "--force", "-m", "-M", "--move",
                          "-d", "-D", "--delete"}
        if not danger:
            return True
        # `branch -m <new>` with no source renames the branch checked out
        # where the command runs — protected only in the main worktree
        if flags & {"-m", "-M", "--move"} and len(pos) <= 1 and in_main:
            return False
        return not any(prot_hit(p, names) for p in pos)
    if verb == "update-ref":
        if "--stdin" in flags:
            return False  # ref list arrives on stdin — can't inspect
        pos = positionals(args, {"-m"})
        return not any(prot_hit(p, names) for p in pos)
    if verb == "symbolic-ref":
        pos = positionals(args)
        if len(pos) >= 2 or flags & {"-d", "--delete"}:
            return not prot_hit(pos[0], names)
        return True
    if verb == "tag":
        # read/delete forms pass; creates prot_hit the tag name — a tag
        # `origin/next_release` shadows the remote-tracking baseline
        # (refs/tags/ resolves before refs/remotes/ in gitrevisions)
        if flags & {"-l", "--list", "-n", "--show", "-v", "--verify",
                    "-d", "--delete"}:
            return True
        pos = positionals(
            args, {"-m", "-F", "-u", "--file", "--local-user",
                   "--cleanup", "--sort"})
        return not (pos and prot_hit(pos[0], names))
    if verb == "worktree":
        pos = positionals(args)
        # `worktree add <path> <committish>` — a second checkout of a
        # protected branch is refused
        if pos and pos[0] == "add":
            return not any(prot_hit(p, names) for p in pos[2:])
        return True
    if verb in ("fast-import", "receive-pack"):
        return False  # stdin ref-write channel — no read form
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
                r"(?:^\+\+\+ b/|^--- a/|^\*\*\* \w+ File: "
                r"|^\*\*\* Move to: )(.+)$", body, re.M):
            if in_main_worktree(os.path.join(base, m.group(1).strip())):
                return block()


def inner_script(toks, env0):
    """For `bash|sh -c <script>` / `eval <script>` invocations, return
    (inner command string, env parsed from the outer preamble) so
    GIT_DIR-style exports still apply inside, else None."""
    env, i = preamble(toks, env0)
    if i >= len(toks):
        return None
    b = os.path.basename(toks[i])
    if b == "eval":
        s = " ".join(toks[i + 1:])
        return (s, env) if s else None
    if b in SHELLS:
        for j in range(i + 1, len(toks) - 1):
            t = toks[j]
            if t == "-c" or (t.startswith("-") and not t.startswith("--")
                             and "c" in t[1:]):
                return toks[j + 1], env
            if not t.startswith("-"):
                break  # positional script file — not inline
    return None


def scan_segments(cmd, cwd, roots, checked, env0=None, depth=0):
    """Scan a command string; returns "block" on a policy hit."""
    cmd = strip_heredocs(cmd)
    segs = re.split(r"&&|\|\||[;|&\n()]", cmd)
    subs = re.findall(r"\$\((.*?)\)|`([^`]*)`", cmd, re.S)
    segs += [s for pair in subs for s in pair if s]
    env0 = dict(env0 or {})
    old = None
    for seg in segs:
        try:
            toks = shlex.split(seg)
        except ValueError:
            continue
        if not toks:
            continue
        if toks[0] in ("export", "readonly", "declare", "local",
                       "typeset"):
            # exported assignments persist for later segments —
            # exporting a GIT_CONFIG_* name is enough to block (the
            # value may have been set by a bare assignment earlier)
            if toks[0] == "export" or "-x" in flags_of(toks[1:]):
                for t in toks[1:]:
                    m = re.fullmatch(r"([A-Za-z_]\w*)=(.*)", t)
                    if m:
                        env0[m.group(1)] = m.group(2)
                    elif re.fullmatch(r"[A-Za-z_]\w*", t) \
                            and (GIT_CONFIG_ENV.match(t)):
                        return "block"
            continue
        if re.fullmatch(r"cd|pushd|popd", toks[0]):
            tgt = next((t for t in toks[1:]
                        if not t.startswith("-") or t == "-"), None)
            if toks[0] == "popd" or tgt == "-" \
                    or (toks[0] == "pushd" and tgt is None):
                cwd, old = old, cwd  # OLDPWD swap; unknown → None
            else:
                newd = tgt or "~"
                old, cwd = cwd, os.path.normpath(
                    os.path.join(cwd or os.sep, newd))
            continue
        if depth < 3:
            inner = inner_script(toks, env0)
            if inner is not None:
                script, env1 = inner
                if scan_segments(script, cwd, roots, checked, env1,
                                 depth + 1) == "block":
                    return "block"
                continue
        inv = git_invocation(toks, env0)
        if not inv:
            continue
        verb, args, cdir, gitdir, configs, env = inv
        in_main, in_repo = locate(cwd, cdir, gitdir, roots)
        if not in_repo:
            continue
        if any(GIT_CONFIG_ENV.match(k) for k in env):
            return "block"
        if any(c.split("=", 1)[0].lower().startswith("alias.")
               for c in configs):
            return "block"
        if verb in ("config", "remote", "branch", "update-ref",
                    "fetch", "pull", "push", "symbolic-ref", "tag",
                    "worktree", "fast-import", "receive-pack"):
            if not repo_verb_ok(verb, args, in_main, checked, cwd):
                return "block"
        elif in_main and not verb_ok(verb, args):
            return "block"
        if depth < 3:
            # exec-capable config/env values spawn unseen inner git —
            # rescan them like `bash -c` payloads
            vals = [v for k, v in env.items() if k in EXEC_ENV and v]
            vals += [c.split("=", 1)[1] for c in configs if "=" in c]
            for j, a in enumerate(args):
                if a in EXEC_OPTS and j + 1 < len(args):
                    vals.append(args[j + 1])
                elif a.split("=", 1)[0] in EXEC_OPTS and "=" in a:
                    vals.append(a.split("=", 1)[1])
            for v in vals:
                if scan_segments(v, cwd, roots, checked, env,
                                 depth + 1) == "block":
                    return "block"


def check_exec(ti, cwd):
    cmd = " ".join(str(ti.get(k) or "")
                   for k in ("command", "text_input", "bytes_input"))
    roots, checked = worktree_info()
    if not roots:
        return
    if scan_segments(cmd, cwd, roots, checked) == "block":
        block()


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
    """(branch, dirty tracked entries, ahead-of-origin count)."""
    roots = worktree_roots()
    if not roots:
        return "", [], 0
    out = run(["git", "-C", roots[0], "status", "--porcelain=v1", "-b"])
    branch, dirty = "", []
    for l in out.splitlines():
        if l.startswith("## "):
            m = re.match(r"(\S+?)(?:\.\.\.|$)", l[3:])
            branch = m.group(1) if m else ""
        elif not l.startswith("??"):
            dirty.append(l)
    ahead = 0
    if branch == PROTECTED_BRANCH:
        cnt = run(["git", "-C", roots[0], "rev-list", "--count",
                   PROTECTED_BRANCH, "^origin/" + PROTECTED_BRANCH])
        ahead = int(cnt) if cnt.isdigit() else 0
    return branch, dirty, ahead


def state_reason(branch, dirty, ahead):
    bits = []
    if branch != PROTECTED_BRANCH:
        bits.append(f"branche `{branch or 'HEAD détaché'}` ≠ "
                    f"`{PROTECTED_BRANCH}`")
    if dirty:
        shown = ", ".join(x[3:].strip() for x in dirty[:5])
        bits.append(f"{len(dirty)} fichier(s) tracké(s) modifié(s) "
                    f"({shown}{', …' if len(dirty) > 5 else ''})")
    if ahead:
        bits.append(f"{ahead} commit(s) locaux ahead "
                    f"d'origin/{PROTECTED_BRANCH}")
    return "Worktree principal souillé : " + "; ".join(bits) + (
        ". Restaurer l'état propre (git switch " + PROTECTED_BRANCH +
        ", git restore/git reset --hard, ou finir depuis un worktree).")


def check_state(block_stop):
    branch, dirty, ahead = main_state()
    if branch == PROTECTED_BRANCH and not dirty and not ahead:
        return
    reason = state_reason(branch, dirty, ahead)
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
