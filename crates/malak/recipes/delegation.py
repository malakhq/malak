"""Spot a lead starting a worker outside the board, and name the `worker-start` form."""

import os
import shlex

import adapters

# The tools that start a delegate and the commands whose `exec` starts one come
# from the adapters, with the kind of start each one is.
AGENT_TOOLS = adapters.merged("delegation_tools")
RUNNERS = adapters.merged("delegation_runners")
SHELL_TOOLS = adapters.union("shell_tools")
SHELLS = {"bash", "sh", "zsh", "dash"}
# Words that run the next command rather than being it.
WRAPPERS = {"env", "nohup", "time", "command", "exec", "sudo", "nice"}
# A runner's subcommand sits within its first few words, after any options.
SUBCOMMAND_WORDS = 6

def segments(command):
    """Split a shell command line into the word lists of its simple commands."""
    for line in command.splitlines():
        lexer = shlex.shlex(line, posix=True, punctuation_chars=True)
        lexer.whitespace_split = True
        words, current = [], []
        try:
            for word in lexer:
                if set(word) <= set(";&|()"):
                    words.append(current)
                    current = []
                else:
                    current.append(word)
        except ValueError:
            continue
        words.append(current)
        yield from (segment for segment in words if segment)


def runner(words):
    """Return the kind of start if these words run `<runner> exec`, else None."""
    while words and ("=" in words[0].partition("/")[0] and not words[0].startswith("-")
                     or os.path.basename(words[0]) in WRAPPERS):
        words = words[1:]
    if not words:
        return None
    program = os.path.basename(words[0])
    if program in SHELLS and "-c" in words[1:]:
        inner = words[words.index("-c") + 1:][:1]
        return next((found for text in inner for found in map(runner, segments(text)) if found), None)
    if program in RUNNERS and "exec" in words[1:SUBCOMMAND_WORDS]:
        return RUNNERS[program]
    return None


def starts_worker(payload):
    """Which outside launch this PreToolUse payload is, as its adapter names it, or None."""
    if payload.get("hook_event_name") != "PreToolUse" or payload.get("agent_id"):
        # A subagent shares its parent's session but is not the lead.
        return None
    tool = payload.get("tool_name")
    if tool in AGENT_TOOLS:
        return AGENT_TOOLS[tool]
    tool_input = payload.get("tool_input")
    command = tool_input.get("command") if isinstance(tool_input, dict) else None
    if tool not in SHELL_TOOLS or not isinstance(command, str):
        return None
    return next((found for words in segments(command) if (found := runner(words))), None)


PREFIX = "Starting a worker?"


def reminder(kind, command):
    example = adapters.merged("start_examples")[kind]
    return (
        f"{PREFIX} Run it on the board: `{command} recipe worker-start NAME "
        f"--brief FILE -- <your agent command>` (e.g. {example}). Its exit report then "
        "reaches lead, and you can steer it through its inbox."
    )
