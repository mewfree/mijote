#!/usr/bin/env python3
"""Scripted ACP agent for the README demo (see demo.tape). Not a real agent:
it replies to every prompt with canned, paced output so the recording is
deterministic and uses no subscription."""
import json
import sys
import time

NAME = sys.argv[1] if len(sys.argv) > 1 else "Claude Code"


def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def update(sid, **u):
    send({"jsonrpc": "2.0", "method": "session/update",
          "params": {"sessionId": sid, "update": u}})


def text(sid, s, kind="agent_message_chunk", delay=0.04):
    for word in s.split(" "):
        update(sid, sessionUpdate=kind, content={"type": "text", "text": word + " "})
        time.sleep(delay)


def survey(sid):
    text(sid, "Scanning the repo layout.", "agent_thought_chunk")
    time.sleep(0.4)
    update(sid, sessionUpdate="tool_call", toolCallId="s1", title="List src/",
           kind="read", status="pending")
    time.sleep(0.5)
    update(sid, sessionUpdate="tool_call_update", toolCallId="s1", status="completed")
    text(sid, "Small crate with one module. I want to run the tests before summarizing.\n\n")
    send({"jsonrpc": "2.0", "id": 901, "method": "session/request_permission",
          "params": {"sessionId": sid,
                     "toolCall": {"toolCallId": "s2", "title": "Run cargo test"},
                     "options": [
                         {"optionId": "allow", "name": "Allow once", "kind": "allow_once"},
                         {"optionId": "reject", "name": "Reject", "kind": "reject_once"}]}})
    sys.stdin.readline()
    update(sid, sessionUpdate="tool_call", toolCallId="s2", title="Run cargo test",
           kind="execute", status="pending")
    time.sleep(0.8)
    update(sid, sessionUpdate="tool_call_update", toolCallId="s2", status="completed")
    text(sid, "All tests pass. `client.rs` exposes a single `fetch` function and "
              "nothing else depends on it yet.\n")


def turn(sid, prompt):
    if "summarize" in json.dumps(prompt):
        return survey(sid)
    text(sid, "Looking at how the retry logic is wired.", "agent_thought_chunk")
    time.sleep(0.4)
    update(sid, sessionUpdate="tool_call", toolCallId="t1", title="Read src/client.rs",
           kind="read", status="pending")
    time.sleep(0.5)
    update(sid, sessionUpdate="tool_call_update", toolCallId="t1", status="completed")
    text(sid, "Requests fail on the first timeout. I'll add exponential backoff.\n\n")
    send({"jsonrpc": "2.0", "id": 900, "method": "session/request_permission",
          "params": {"sessionId": sid,
                     "toolCall": {"toolCallId": "t2", "title": "Edit src/client.rs"},
                     "options": [
                         {"optionId": "allow", "name": "Allow once", "kind": "allow_once"},
                         {"optionId": "reject", "name": "Reject", "kind": "reject_once"}]}})
    answer = json.loads(sys.stdin.readline())
    if answer.get("result", {}).get("outcome", {}).get("optionId") != "allow":
        text(sid, "Understood, leaving the file untouched.")
        return
    update(sid, sessionUpdate="tool_call", toolCallId="t2", title="Edit src/client.rs",
           kind="edit", status="pending")
    time.sleep(0.6)
    update(sid, sessionUpdate="tool_call_update", toolCallId="t2", status="completed")
    text(sid, "Done. `fetch` now retries **3 times** with backoff:\n\n"
              "```rust\nfor attempt in 0..3 {\n    sleep(base << attempt);\n}\n```\n\n"
              "- delays: 100ms, 200ms, 400ms\n- gives up after the last attempt\n")


def main():
    for line in sys.stdin:
        req = json.loads(line)
        method, rid, params = req.get("method"), req.get("id"), req.get("params", {})
        if method == "initialize":
            send({"jsonrpc": "2.0", "id": rid, "result": {
                "protocolVersion": 1, "agentInfo": {"name": NAME}, "authMethods": []}})
        elif method == "session/new":
            send({"jsonrpc": "2.0", "id": rid, "result": {"sessionId": "demo", "models": {
                "currentModelId": "m", "availableModels": [{"modelId": "m", "name": NAME}]}}})
        elif method == "session/prompt":
            turn(params.get("sessionId", "demo"), params)
            send({"jsonrpc": "2.0", "id": rid, "result": {"stopReason": "end_turn"}})
        elif rid is not None:
            send({"jsonrpc": "2.0", "id": rid, "result": {}})


if __name__ == "__main__":
    main()
