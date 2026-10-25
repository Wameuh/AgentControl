#!/usr/bin/env python3
"""PreToolUse hook: block merge commands — merges are the user's prerogative.

Exception: `gh pr merge <N>` is allowed when the PR targets
`next_release` AND the latest review-shaped comment (marker
`^[*#]+ ?(Review|Verdict)`, from the review bot or an owner/member/
collaborator) carries an approving verdict **in title position**
(`Verdict: APPROVED` / `Verdict: mergeable`) posted after the
server-side push of the current head sha (repo PushEvents).

Everything else stays blocked: git merge, pulls/<n>/merge,
repos/<r>/merges, mergePullRequest, enablePullRequestAutoMerge,
gh pr merge --admin / to main / without approval / with a divergent
-R or pull URL. Read-only plumbing (merge-base, merge-file,
--merges) and git pull are unaffected.

Tripwire, not a security boundary: lexical tricks (`gh 'pr' 'merge'`)
are not matched, and a review-shaped comment self-posted under the
local gh account is indistinguishable from a real review — the hook
prevents policy-drift merges, not a determined bypass.
"""
import json
import re
import shlex
import subprocess
import sys
from datetime import datetime, timedelta, timezone

HARD_BLOCK = [
    r"\bpulls/\d+/merge\b",                            # REST merge endpoint
    r"\brepos/\S+/merges\b",                           # REST "merge a branch" endpoint
    r"\b(?:mergePullRequest|enablePullRequestAutoMerge|autoMergePullRequest)\b",
    r"\bgit\s+(?:-\S+\s+\S+\s+|-\S+\s+)*merge\b(?!-)", # git [-C d] merge (not merge-base/-file)
]
GH_PR_MERGE = re.compile(r"\bgh\b[^;|&]*\bpr\s+merge\b")
REVIEW_MARKER = r"^[*#]+ ?(Review|Verdict)"
VERDICT_TITLE = re.compile(r"(?im)^\s*[*# ]*verdict\s*:\s*([a-z]+)\b")
REVIEW_BOTS = ("devin-ai-integration[bot]", "github-actions[bot]")
SKEW = timedelta(minutes=2)

# `gh pr merge` flags: booleans skipped, value-flags skip their value.
BOOL_FLAGS = {"--auto", "--disable-auto", "-d", "--delete-branch",
              "-m", "--merge", "-r", "--rebase", "-s", "--squash"}
VAL_FLAGS = {"-b", "--body", "--body-file", "--match-head-commit",
             "-t", "--subject", "-R", "--repo"}

REASON = (
    "Merge bloqué : réservé à l'utilisateur, sauf `gh pr merge <N>` vers "
    "`next_release` quand la dernière review auto est `Verdict: APPROVED` "
    "postérieure au push du head courant."
)


def run(cmd, timeout=4):
    try:
        return subprocess.run(cmd, capture_output=True, text=True,
                              timeout=timeout).stdout.strip()
    except Exception:
        return ""


def repo_nwo():
    url = run(["git", "remote", "get-url", "origin"])
    m = re.search(r"[:/]([^/:]+/[^/]+?)(?:\.git)?$", url)
    return m.group(1) if m else ""


def pr_target(seg, nwo):
    """Parse `gh pr merge` args → (pr_number, ok). Fail closed on
    --admin, unknown flags, a divergent -R/--repo or pull URL, and on
    non-verifiable targets (branch names)."""
    try:
        toks = shlex.split(seg)
    except ValueError:
        return "", False
    try:
        i = toks.index("merge")
    except ValueError:
        return "", False
    pos = None
    j = i + 1
    while j < len(toks):
        t = toks[j]
        if t == "--admin":
            return "", False
        # redirections shell : ">f"/"2>&1" en un token, "> f" en deux
        # — sans ça la cible de redirect passerait pour un positionnel.
        if re.fullmatch(r"\d*(>>?|<)", t):
            j += 2
            continue
        if re.match(r"\d*(>>?|<|>&|<&)", t) or t == "&":
            j += 1
            continue
        if t in BOOL_FLAGS:
            j += 1
            continue
        if t in VAL_FLAGS:
            if t in ("-R", "--repo") and j + 1 < len(toks) \
                    and toks[j + 1].lower() != nwo.lower():
                return "", False
            j += 2
            continue
        if t.startswith("--repo="):
            if t.split("=", 1)[1].lower() != nwo.lower():
                return "", False
            j += 1
            continue
        if t.startswith("-"):
            return "", False
        if pos is None:
            pos = t
        else:
            return "", False
        j += 1
    if not pos:
        return "", False
    m = re.search(r"github\.com/([^/]+/[^/]+)/pull/(\d+)", pos)
    if m:
        return (m.group(2), m.group(1).lower() == nwo.lower())
    if re.fullmatch(r"\d+", pos):
        return pos, True
    return "", False


def latest_verdict(n, nwo):
    """(created_at, verdict-token) of the newest review-shaped comment
    from a plausible reviewer; ('', '') if none/unverifiable."""
    bot_sel = " or ".join(f'.user.login == "{b}"' for b in REVIEW_BOTS)
    out = run(["gh", "api", "--paginate",
               f"repos/{nwo}/issues/{n}/comments?per_page=100",
               "--jq",
               rf'.[] | select(.body | test("{REVIEW_MARKER}"))'
               rf' | select({bot_sel} or .author_association == "OWNER"'
               ' or .author_association == "MEMBER"'
               ' or .author_association == "COLLABORATOR")'
               ' | .created_at + "\\u0001" + .body'], timeout=8)
    # rows are `created_at + \u0001 + body`; bodies keep their newlines,
    # so the last row starts at the last \u0001 (bodies can't contain it).
    idx = out.rfind("\u0001")
    if idx == -1:
        return "", ""
    start = out.rfind("\n", 0, idx)
    created_s = out[start + 1:idx]
    body = out[idx + 1:]
    m = VERDICT_TITLE.search(body)
    return created_s, (m.group(1).lower() if m else "")


def pushed_at(nwo, head, ref):
    """Server-side timestamp of the push that set `head` as a branch
    tip (repo PushEvents) — committer.date is author-controlled.
    The events feed can lag hours → fall back to the per-ref
    activity feed, which reports pushes near-instantly."""
    ts = run(["gh", "api", "--paginate",
              f"repos/{nwo}/events?per_page=100",
              "--jq",
              rf'[.[] | select(.type == "PushEvent"'
              rf' and .payload.head == "{head}")]'
              ' | .[0].created_at // ""'], timeout=8)
    if ts or not ref:
        return ts
    return run(["gh", "api", "--method", "GET", f"repos/{nwo}/activity",
                "-f", f"ref={ref}", "-f", "per_page=100",
                "--jq",
                rf'[.[] | select(.after == "{head}")]'
                ' | .[0].timestamp // ""'], timeout=8)


def approved_next_release_merge(n, nwo):
    pr = run(["gh", "pr", "view", n, "-R", nwo,
              "--json", "baseRefName,headRefOid,headRefName"])
    try:
        pr = json.loads(pr)
    except Exception:
        return False
    if pr.get("baseRefName") != "next_release":
        return False
    head = pr.get("headRefOid") or ""
    created_s, verdict = latest_verdict(n, nwo)
    if not created_s or verdict not in ("approved", "mergeable"):
        return False
    name = pr.get("headRefName") or ""
    pushed = pushed_at(nwo, head, f"refs/heads/{name}" if name else "")
    if not pushed:
        return False
    try:
        return datetime.fromisoformat(created_s.replace("Z", "+00:00")) \
            >= datetime.fromisoformat(pushed.replace("Z", "+00:00")) - SKEW
    except ValueError:
        return False


def main():
    try:
        data = json.load(sys.stdin)
    except Exception:
        return 0
    ti = data.get("tool_input") or {}
    cmd = " ".join(str(ti.get(k) or "")
                   for k in ("command", "text_input", "bytes_input"))
    nwo = None
    for seg in re.split(r"&&|\|\||[;|]", cmd):
        if any(re.search(p, seg) for p in HARD_BLOCK):
            print(json.dumps({"decision": "block", "reason": REASON}))
            return 0
        if GH_PR_MERGE.search(seg):
            if nwo is None:
                nwo = repo_nwo()
            n, ok = pr_target(seg, nwo) if nwo else ("", False)
            if not (ok and approved_next_release_merge(n, nwo)):
                print(json.dumps({"decision": "block", "reason": REASON}))
                return 0
    return 0


if __name__ == "__main__":
    sys.exit(main())
