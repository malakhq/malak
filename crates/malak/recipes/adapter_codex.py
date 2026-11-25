"""The Codex adapter."""

import json
import os
import select
import subprocess
import time

from adapters import Adapter
import adapter_codex_notice as notice

CONTEXT_EVENTS = ("SessionStart", "UserPromptSubmit", "SubagentStart", "PreToolUse", "PostToolUse")
RECOMMENDED = " (Recommended)"
ASYNC_TOOL = "request_user_input_async"
ASYNC_REPLY_TAG = "send_user_message_question_reply"
EVENTS = (
    "SessionStart", "UserPromptSubmit", "SubagentStart", "SubagentStop",
    "PreToolUse", "PostToolUse", "Stop", "SessionEnd",
)


def reply(event, value):
    """Codex reads context as `additionalContext` and a Stop reply as it is."""
    if event in CONTEXT_EVENTS:
        return {"hookSpecificOutput": {"hookEventName": event, "additionalContext": value}}
    if event == "Stop":
        return value
    return None


def trust_steps(events):
    return (
        "In Codex CLI, open /hooks and press Enter on each of "
        f"{', '.join(events)}. Select the Malak command from hooks.json, review it, then press t "
        "to trust it. Press Esc and repeat for the other events. "
        "In Codex desktop, open Settings > Hooks to review and trust the Malak commands."
    )


def hook_arguments(events, env):
    return codex_hook_arguments(events)


def codex_hook_arguments(events):
    flags = []
    for event, command in events.items():
        settings = "timeout=3" if event == "SessionEnd" else "additionalContextLimit=0"
        flags.extend((
            "-c", f"hooks.{event}=[{{hooks=[{{type=\"command\","
                  f"command={json.dumps(command)},{settings}}}]}}]",
        ))
    return flags


def send_request(server, request):
    server.stdin.write((json.dumps(request) + "\n").encode())


def receive_response(server, identifier, pending, stopping):
    deadline = time.monotonic() + 20
    while not stopping() and (left := deadline - time.monotonic()) > 0:
        end = pending.find(b"\n")
        if end < 0:
            if select.select([server.stdout], [], [], min(left, 0.05))[0]:
                chunk = os.read(server.stdout.fileno(), 65536)
                if not chunk:
                    break
                pending.extend(chunk)
            continue
        reply = json.loads(pending[:end])
        del pending[:end + 1]
        if reply.get("id") == identifier and "result" in reply:
            return reply["result"]
        if reply.get("id") == identifier:
            message = reply.get("error", {}).get("message", "unknown error")
            raise OSError(f"Codex could not list the session hooks: {message}")
    raise OSError("Codex did not return the session hook definitions")


def session_trust(workspace, definitions, *, env, stopping):
    # Inspect the session definitions through Codex. The resulting overrides
    # approve only these hashes for this process and leave user hooks alone.
    with subprocess.Popen(
        ["codex", "app-server", "--stdio", *codex_hook_arguments(definitions)],
        cwd=workspace, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, bufsize=0,
    ) as server:
        pending = bytearray()
        try:
            send_request(server, {"id": 0, "method": "initialize", "params": {
                "clientInfo": {"name": "malak", "version": "0.1.2"},
                "capabilities": {"experimentalApi": True},
            }})
            receive_response(server, 0, pending, stopping)
            send_request(server, {"method": "initialized"})
            send_request(server, {"id": 1, "method": "hooks/list",
                                  "params": {"cwds": [str(workspace)]}})
            result = receive_response(server, 1, pending, stopping)
        finally:
            if server.poll() is None:
                server.terminate()
                try:
                    server.wait(timeout=1)
                except subprocess.TimeoutExpired:
                    server.kill()

    expected = {event[0].lower() + event[1:]: command for event, command in definitions.items()}
    settings = []
    hooks = result["data"][0]["hooks"]
    for hook in hooks:
        event = hook["eventName"]
        if (hook["source"] == "sessionFlags" and hook["handlerType"] == "command"
                and hook["enabled"] and expected.get(event) == hook["command"]):
            key, value = json.dumps(hook["key"]), json.dumps(hook["currentHash"])
            settings.append(f"{key}={{trusted_hash={value}}}")
            del expected[event]
    if expected:
        raise OSError("Codex did not expose all four enabled Malak hooks for session approval")
    return ["-c", f"hooks.state={{{','.join(settings)}}}"]




class RequestUserInput:
    """The question tool: an `id` identifies each question."""

    @staticmethod
    def normalize(question):
        return question

    @staticmethod
    def key(question, text):
        return question.get("id", text)


class RequestUserInputAsync(RequestUserInput):
    """The asynchronous question tool: options are plain labels, the first recommended."""

    @staticmethod
    def normalize(question):
        labels = question.get("options") or []
        if not isinstance(labels, list) or not all(isinstance(label, str) for label in labels):
            return None
        return {
            "question": question.get("title"),
            "options": [{"label": label} for label in labels],
            "recommendation": labels[0].removesuffix(RECOMMENDED) if labels else "",
        }


def async_replies(prompt):
    """The (tool, call, index) of each asynchronous question a prompt answers."""
    if not isinstance(prompt, str):
        return []
    wrapped = prompt.strip()
    opening, closing = f"<{ASYNC_REPLY_TAG}>", f"</{ASYNC_REPLY_TAG}>"
    if not wrapped.startswith(opening) or not wrapped.endswith(closing):
        return []
    try:
        replies = json.loads(wrapped[len(opening):-len(closing)])
    except ValueError:
        return []
    if isinstance(replies, dict):
        replies = [replies]
    if not isinstance(replies, list):
        return []
    matched = []
    for item in replies:
        if not isinstance(item, dict) or not isinstance(answer := item.get("answer"), str) or not answer.strip():
            continue
        try:
            tool, call, index = json.loads(item["questionItemId"])
        except (KeyError, TypeError, ValueError):
            continue
        if tool == ASYNC_TOOL and isinstance(call, str) and type(index) is int and index >= 0:
            matched.append((tool, call, str(index)))
    return matched


def demo_command(sockets, sandbox, lead, trust):
    command = ["codex", "-a", "never", "exec", "--ephemeral", "--skip-git-repo-check",
               "--color", "never", "-c", "allow_login_shell=false", *trust]
    if sandbox == "off":
        return [*command, "--sandbox", "danger-full-access", "--"]
    doors = ("l", "p") if lead else ("p",)
    allowed = ",".join(f'{json.dumps(str(sockets / door / "s"))}="allow"' for door in doors)
    return [*command, "-c", 'default_permissions="malak-demo"',
            "-c", 'permissions.malak-demo.filesystem={":root"="read",'
                  '":workspace_roots"="write",":tmpdir"="write"}',
            "-c", 'permissions.malak-demo.network={enabled=true,mode="limited"}',
            "-c", "features.network_proxy.enabled=true",
            "-c", f"permissions.malak-demo.network.unix_sockets={{{allowed}}}", "--"]


ADAPTER = Adapter(
    key="codex",
    display="Codex",
    programs=("codex",),
    rank=1,
    session_env=("CODEX_SESSION_ID", "CODEX_THREAD_ID"),
    session_priority=0,
    matches=lambda payload: "turn_id" in payload,
    context_events=CONTEXT_EVENTS,
    reply=reply,
    config_env="CODEX_HOME",
    config_dir=".codex",
    settings_file="hooks.json",
    instruction_file="AGENTS.md",
    trust_steps=trust_steps,
    hook_arguments=hook_arguments,
    session_trust=session_trust,
    notice=notice,
    shell_tools=frozenset({"Bash"}),
    activity_tools=frozenset({"spawn_agent"}),
    plan_tools=frozenset({"update_plan"}),
    delegation_runners={"codex": "codex"},
    start_examples={"codex": "codex exec ..."},
    question_tools={"request_user_input": RequestUserInput, ASYNC_TOOL: RequestUserInputAsync},
    async_tool=ASYNC_TOOL,
    async_replies=async_replies,
    loop_hint="In the Codex App use an automation that runs the same command.",
    background_hint="a background command (Codex)",
    background_use="a background command in Codex",
    skill_demo='`malak recipe demo codex "GOAL"`',
    skill_loop="In the Codex App use an automation that runs the same command.",
    skill_background="Codex's background command session",
    skill_delegation="`codex exec`",
    lane_label="Codex",
    skill_lanes=(
        "  - Codex lane: `malak --board NAME recipe worker-start WORKER --brief FILE -- codex exec ...`",
    ),
    demo_command=demo_command,
    demo_trust=True,
)
