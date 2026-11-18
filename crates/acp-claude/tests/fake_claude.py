#!/usr/bin/env python3
"""Faux `claude` pour les tests acp-claude : lit du NDJSON sur stdin,
répond aux control_request, et exécute les tours scriptés du fichier
scénario (JSON dans FAKE_SCENARIO).

Scénario :
{
  "init": {control initialize response},
  "turns": [[evt, ...], ...],        # un script par message user
  "on_interrupt": [evt, ...],       # émis sur control interrupt
}
Événements spéciaux : {"_sleep": s}, {"_perm_wait": rid}.
"""
import json
import os
import sys
import time

scenario = json.load(open(os.environ["FAKE_SCENARIO"]))
_pidfile = os.environ.get("FAKE_PIDFILE")
if _pidfile:
    open(_pidfile, "w").write(str(os.getpid()))
turns = list(scenario.get("turns", []))
default_turn = scenario.get("default_turn", [])
on_interrupt = scenario.get("on_interrupt", [])
pending_users = []


def emit(evt):
    if "_sleep" in evt:
        time.sleep(evt["_sleep"])
        return
    if "_perm_wait" in evt:
        return
    sys.stdout.write(json.dumps(evt) + "\n")
    sys.stdout.flush()


def ctrl_response(rid, response):
    sys.stdout.write(
        json.dumps(
            {
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": rid,
                    "response": response,
                },
            }
        )
        + "\n"
    )
    sys.stdout.flush()


def run_turn():
    script = turns.pop(0) if turns else default_turn
    for evt in script:
        if "_perm_wait" in evt:
            # Bloque jusqu'à la control_response du control_request
            # can_use_tool émis juste avant. Les messages user reçus
            # pendant l'attente sont mis en file (comme claude les
            # queue en queued_turns) — jamais perdus.
            target = evt["_perm_wait"]
            for line in sys.stdin:
                try:
                    m = json.loads(line)
                except Exception:
                    continue
                if m.get("type") == "control_response":
                    r = m.get("response", {})
                    if r.get("request_id") == target:
                        break
                elif m.get("type") == "user":
                    pending_users.append(line)
            continue
        emit(evt)


def input_lines():
    while True:
        if pending_users:
            yield pending_users.pop(0)
        else:
            l = sys.stdin.readline()
            if not l:
                return
            yield l


for line in input_lines():
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except Exception:
        continue
    t = msg.get("type")
    if t == "control_request":
        rid = msg.get("request_id")
        req = msg.get("request", {})
        sub = req.get("subtype")
        if sub == "initialize":
            if scenario.get("init_error"):
                sys.stdout.write(
                    json.dumps(
                        {
                            "type": "control_response",
                            "response": {
                                "subtype": "error",
                                "request_id": rid,
                                "error": "init failed",
                            },
                        }
                    )
                    + "\n"
                )
                sys.stdout.flush()
            else:
                ctrl_response(rid, scenario.get("init", {}))
        elif sub == "interrupt":
            for evt in on_interrupt:
                emit(evt)
            ctrl_response(rid, {})
        elif sub in (
            "set_permission_mode",
            "set_model",
            "set_max_thinking_tokens",
            "mcp_status",
        ):
            ctrl_response(rid, {})
        else:
            sys.stdout.write(
                json.dumps(
                    {
                        "type": "control_response",
                        "response": {
                            "subtype": "error",
                            "request_id": rid,
                            "error": f"unsupported: {sub}",
                        },
                    }
                )
                + "\n"
            )
            sys.stdout.flush()
    elif t == "user":
        run_turn()
    # control_response / autre : ignoré
