#!/usr/bin/env python3
# A fake `claude` for OpenAGC's RemoteTrigger tests. It records how it was
# called and answers as `claude -p --output-format json` would after the
# model called RemoteTrigger.
import json, os, re, sys

here = os.path.dirname(os.path.abspath(__file__))
args = sys.argv[1:]
with open(os.path.join(here, "calls.jsonl"), "a") as f:
    f.write(json.dumps({"args": args, "api_key": os.environ.get("ANTHROPIC_API_KEY"),
                        "nested": os.environ.get("CLAUDECODE")}) + "\n")
prompt = args[args.index("-p") + 1]
call = json.loads(prompt[prompt.index("{"):])
action = call["action"]

def reply(result, is_error=False):
    print(json.dumps({"type": "result", "subtype": "success", "is_error": is_error, "result": result}))
    sys.exit(0)

mode = open(os.path.join(here, "mode")).read().strip() if os.path.exists(os.path.join(here, "mode")) else "ok"
if mode == "logged_out":
    reply("Invalid API key · Please run /login", is_error=True)
if action == "list":
    reply("```json\n" + json.dumps({"data": [{"id": "trig_old", "job_config": {"ccr": {"environment_id": "env_42",
        "session_context": {"model": "claude-opus-5"}}}, "mcp_connections": [
        {"connector_uuid": "conn-1", "name": "Gmail", "url": "https://gmailmcp.googleapis.com/mcp/v1"}]}]}) + "\n```")
if action == "create":
    body = call["body"]
    with open(os.path.join(here, "created.json"), "w") as f:
        json.dump(body, f)
    reply("Created it: " + json.dumps({"id": "trig_01NEW", "next_run_at": "2026-09-24T10:44:00Z"}))
if action == "update":
    with open(os.path.join(here, "updated.json"), "w") as f:
        json.dump(call, f)
    reply(json.dumps({"id": call["trigger_id"]}))
if action == "run":
    reply(json.dumps({"session_id": "session_run_1"}))
if action == "list_runs":
    reply(json.dumps({"data": [{"session_id": "session_run_1", "status": "completed",
        "started_at": "2026-09-24T09:44:00Z", "ended_at": "2026-09-24T09:46:10Z"}]}))
if action == "get_run_log":
    reply(json.dumps({"log": "Sorted 4 threads: 1-Daily 1, 2-Weekly-Newsletters 3."}))
reply(json.dumps({"error": "unknown action"}))
