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
turns = list(scenario.get("turns", []))
default_turn = scenario.get("default_turn", [])
on_interrupt = scenario.get("on_interrupt", [])
perm_wait_rid = None


def emit(evt):
    if "_sleep" in evt:
        time.sleep(evt["_sleep"])
        return
    if "_perm_wait" in evt:
        global perm_wait_rid
        perm_wait_rid = evt["_perm_wait"]
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
            # can_use_tool émis juste avant (lu sur stdin).
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
                    # message user pendant la permission : on le
                    # traite après (injecté en file simple — le test
                    # de race s'en charge via un second tour).
                    pass
            continue
        emit(evt)


for line in sys.stdin:
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
