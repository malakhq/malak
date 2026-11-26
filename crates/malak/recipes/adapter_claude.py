"""The Claude Code adapter."""

import json
import os
from pathlib import Path

from adapters import Adapter
from agent_settings import JsonServers

CONTEXT_EVENTS = ("SessionStart", "UserPromptSubmit", "SubagentStart", "PreToolUse", "PostToolUse")


def reply(event, value):
    """Claude Code reads context as `additionalContext` and a Stop reply as it is."""
    if event in CONTEXT_EVENTS:
        return {"hookSpecificOutput": {"hookEventName": event, "additionalContext": value}}
    if event == "Stop":
        return value
    return None


def hook_arguments(events, env):
    settings = {"hooks": {
        event: [{"hooks": [{"type": "command", "command": command}]}]
        for event, command in events.items()
    }}
    return ["--settings", json.dumps(settings)]


class AskUserQuestion:
    """The question tool: the question text identifies each question."""

    @staticmethod
    def normalize(question):
        return question

    @staticmethod
    def key(question, text):
        return text


def mcp_file(home):
    """Claude Code keeps its user-level MCP servers in .claude.json, beside its config folder."""
    configured = os.environ.get("CLAUDE_CONFIG_DIR")
    return (Path(configured).expanduser().absolute() if configured else home) / ".claude.json"


def demo_command(sockets, sandbox, lead, trust):
    if sandbox == "off":
        return ["claude", "-p", "--no-session-persistence", "--dangerously-skip-permissions", "--"]
    return ["claude", "-p", "--no-session-persistence", "--tools", "Bash,Write,Read",
            "--allowedTools", "Bash(malak *)", "Write", "Read", "--"]


ADAPTER = Adapter(
    key="claude",
    display="Claude Code",
    programs=("claude",),
    rank=0,
    session_env=("CLAUDE_CODE_SESSION_ID",),
    session_priority=1,
    default=True,
    context_events=CONTEXT_EVENTS,
    reply=reply,
    config_env="CLAUDE_CONFIG_DIR",
    config_dir=".claude",
    settings_file="settings.json",
    instruction_file="CLAUDE.md",
    hook_arguments=hook_arguments,
    mcp_file=mcp_file,
    mcp_codec=JsonServers("mcpServers"),
    shell_tools=frozenset({"Bash"}),
    activity_tools=frozenset({"Agent", "Task", "Monitor"}),
    plan_tools=frozenset({"TaskCreate", "TaskUpdate", "TodoWrite"}),
    delegation_tools={"Agent": "agent", "Task": "agent"},
    start_examples={"agent": "claude --model claude-sonnet-5-5 ..."},
    question_tools={"AskUserQuestion": AskUserQuestion},
    loop_hint="In Claude Code use /loop (dynamic) with a tick of 2 to 5 minutes.",
    background_hint="background Bash (Claude Code)",
    background_use="background Bash in Claude Code",
    skill_demo='`malak recipe demo claude "GOAL"`',
    skill_loop="In Claude Code use `/loop` (dynamic) with a tick of 2 to 5 minutes.",
    skill_background="Claude Code's background Bash (leave Monitor for other work)",
    skill_delegation="Claude Code's Agent or Task tool",
    lane_label="Sonnet",
    skill_lanes=(
        "  - Sonnet lane: `malak --board NAME recipe worker-start WORKER --brief FILE -- claude --model claude-sonnet-5-5 ...`",
    ),
    demo_command=demo_command,
)
