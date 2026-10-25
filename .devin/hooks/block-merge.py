#!/usr/bin/env python3
"""PreToolUse hook: block merge commands — merges are the user's prerogative.

Exception: an agent may `gh pr merge <N>` when the PR targets
`next_release` AND the latest auto-review comment (marker
`^[*#]+ ?(Review|Verdict)`, e.g. `## Verdict: APPROVED`) approves —
`APPROVED`/`mergeable` in the body — and post-dates the current head
commit (~2 min skew). Everything else stays blocked: git merge,
pulls/<n>/merge, repos/<r>/merges, mergePullRequest, gh pr merge to
main or without approval. Does NOT block read-only plumbing
(merge-base, merge-file, --merges) or git pull.
"""
import json
import re
import subprocess
import sys
from datetime import datetime, timedelta, timezone

HARD_BLOCK = [
    r"\bpulls/\d+/merge\b",                            # REST merge endpoint
    r"\brepos/\S+/merges\b",                           # REST "merge a branch" endpoint
    r"\bmergePullRequest\b",                           # GraphQL mutation
    r"\bgit\s+(?:-\S+\s+\S+\s+|-\S+\s+)*merge\b(?!-)", # git [-C d] merge (not merge-base/-file)
]
GH_PR_MERGE = re.compile(r"\bgh\b[^;|&]*\bpr\s+merge\b")
REVIEW_MARKER = r"^[*#]+ ?(Review|Verdict)"
SKEW = timedelta(minutes=2)

REASON = (
    "Merge bloqué : réservé à l'utilisateur, sauf `gh pr merge <N>` vers "
    "`next_release` quand la dernière review auto est APPROVED sur le head "
    "courant (fraîche : post-commit)."
)


def run(cmd, timeout=3):
    try:
        return subprocess.run(cmd, capture_output=True, text=True,
                              timeout=timeout).stdout.strip()
    except Exception:
        return ""


def repo_nwo():
    url = run(["git", "remote", "get-url", "origin"])
    m = re.search(r"[:/]([^/:]+/[^/]+?)(?:\.git)?$", url)
    return m.group(1) if m else ""


def pr_number(seg):
    m = re.search(r"pull/(\d+)", seg) \
        or re.search(r"\bpr\s+merge\s+(?:--?\S+(?:\s+\S+)?\s+)*?(\d+)\b", seg)
    return m.group(1) if m else ""


def approved_next_release_merge(n, nwo):
    out = run(["gh", "pr", "view", n, "--json", "baseRefName,headRefOid"])
    try:
        pr = json.loads(out)
    except Exception:
        return False
    if pr.get("baseRefName") != "next_release":
        return False
    rows = run(["gh", "api", "--paginate",
                f"repos/{nwo}/issues/{n}/comments?per_page=100",
                "--jq",
                rf'.[] | select(.body | test("{REVIEW_MARKER}"))'
                ' | .created_at + "\u0001" + (.body | gsub("\n"; " "))'])
    last = rows.splitlines()[-1] if rows else ""
    if not last or "\u0001" not in last:
        return False
    created_s, body = last.split("\u0001", 1)
    # verdict en position de titre : `## Verdict: APPROVED`, ou
    # `verdict mergeable` dans les ~40 premiers caractères du marqueur —
    # la prose en body ne suffit pas (REJECTED/CONDITIONAL -> bloqué).
    if not re.search(
            r"(?i)verdict[: ]+(approved|mergeable)"
            r"|^[*# ]*(review|verdict).{0,40}\b(approved|mergeable)\b",
            body):
        return False
    # fraîcheur : la review doit post-dater le commit du head courant
    head = pr.get("headRefOid") or ""
    hdate = run(["gh", "api", f"repos/{nwo}/commits/{head}",
                 "--jq", ".commit.committer.date"], timeout=2)
    if not hdate:
        return True  # head introuvable -> verdict suffit
    try:
        created = datetime.fromisoformat(created_s.replace("Z", "+00:00"))
        committed = datetime.fromisoformat(hdate.replace("Z", "+00:00"))
    except ValueError:
        return False
    return created >= committed - SKEW


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
            n = pr_number(seg)
            if nwo is None:
                nwo = repo_nwo()
            if not (n and nwo and approved_next_release_merge(n, nwo)):
                print(json.dumps({"decision": "block", "reason": REASON}))
                return 0
    return 0


if __name__ == "__main__":
    sys.exit(main())
