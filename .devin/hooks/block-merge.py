#!/usr/bin/env python3
"""PreToolUse hook: block merge commands — merges are the user's prerogative.

Blocks: gh pr merge, git merge, gh api / curl to pull/branch merge endpoints,
GraphQL mergePullRequest. Does NOT block read-only git plumbing
(merge-base, merge-file, --merges filters) or git pull.
"""
import json
import re
import sys

PATTERNS = [
    r"\bgh\b[^;|&]*\bpr\s+merge\b",                    # gh [-R r] pr merge
    r"\bpulls/\d+/merge\b",                            # REST merge endpoint (gh api, curl)
    r"\brepos/\S+/merges\b",                           # REST "merge a branch" endpoint
    r"\bmergePullRequest\b",                           # GraphQL mutation
    r"\bgit\s+(?:-\S+\s+\S+\s+|-\S+\s+)*merge\b(?!-)", # git [-C d] merge (not merge-base/-file)
]

REASON = (
    "Merge bloqué : les merges de PR/branches sont réservés à l'utilisateur. "
    "L'agent prépare la branche et la PR, l'utilisateur merge manuellement."
)


def main():
    try:
        data = json.load(sys.stdin)
    except Exception:
        return 0
    ti = data.get("tool_input") or {}
    cmd = " ".join(str(ti.get(k) or "") for k in ("command", "text_input", "bytes_input"))
    if any(re.search(p, seg) for p in PATTERNS for seg in re.split(r"&&|\|\||[;|]", cmd)):
        print(json.dumps({"decision": "block", "reason": REASON}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
