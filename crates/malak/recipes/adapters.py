"""The registry of agent-tool adapters.

A recipe is a documented pattern of the four board verbs and names no agent tool
or model. Everything that belongs to one tool lives in that tool's adapter, a
file named `adapter_<tool>.py` that defines `ADAPTER`: its payload fields, reply
formats, hook registration, worker-start wiring, question tools and delegation
tools. A new tool is a new adapter file, never a change to a recipe.
"""

from dataclasses import dataclass, field
import importlib.util
from pathlib import Path
import sys
from typing import Callable, Optional


@dataclass(frozen=True)
class Adapter:
    """What one agent tool needs from the recipes. Unset fields mean "not applicable"."""

    key: str
    # Words shown to people, such as "Claude Code". None hides the tool from listings.
    display: Optional[str] = None
    # The command names that run the tool.
    programs: tuple = ()
    # Order of the tool among others in listings; lower comes first.
    rank: int = 0
    # Environment variables that carry the tool's session ID, and their precedence.
    session_env: tuple = ()
    session_priority: int = 0

    # Payload fields: does this payload come from the tool? The tool whose
    # `default` is true is assumed when nothing else matches.
    matches: Callable = lambda payload: False
    default: bool = False
    # Hook events that carry additional context, and the reply for each event:
    # `reply(event, text)` returns the JSON object to print, or None.
    context_events: tuple = ()
    reply: Callable = lambda event, value: None

    # User-level install: the settings folder (variable, default name), the settings
    # file that holds hook registrations, the folder of skills and the project file
    # that holds agent instructions.
    config_env: Optional[str] = None
    config_dir: Optional[str] = None
    settings_file: Optional[str] = None
    instruction_file: Optional[str] = None
    # Text for the person after the hooks are registered, given the event names.
    trust_steps: Optional[Callable] = None
    # Where the tool keeps its MCP servers: `mcp_file(home)` is the file, and
    # `mcp_codec` reads and writes its format (see agent_settings).
    mcp_file: Optional[Callable] = None
    mcp_codec: object = None

    # The `malak hook --tool` format that delivers messages to this tool. Unset
    # means the tool's own key; a tool with no format of its own uses "generic".
    delivery_tool: Optional[str] = None
    # worker-start wiring: `hook_arguments(events, env)` returns the command-line
    # arguments that register the hooks for one session, and `session_trust`
    # (optional) returns the extra arguments that approve them.
    hook_arguments: Optional[Callable] = None
    session_trust: Optional[Callable] = None
    # Notifying already-open sessions after an install or upgrade (optional module).
    notice: object = None

    # Tool names in hook payloads: the ones that mark activity in a session, the
    # ones that record plans, and the ones that start a delegate. The delegation
    # tables map a tool name or command name to the kind of start it is.
    activity_tools: frozenset = frozenset()
    shell_tools: frozenset = frozenset()
    plan_tools: frozenset = frozenset()
    delegation_tools: dict = field(default_factory=dict)
    delegation_runners: dict = field(default_factory=dict)
    # The command a lead might type for each kind of start, shown as an example.
    start_examples: dict = field(default_factory=dict)

    # Question tools: tool name to a parser with `questions(payload)` and
    # `answers(payload)`; and the prompt wrapper of asynchronous replies (optional).
    question_tools: dict = field(default_factory=dict)
    # `async_replies(prompt)` returns the (call, index) pairs an asynchronous
    # question tool delivered in a prompt; `async_tool` names that tool.
    async_tool: Optional[str] = None
    async_replies: Optional[Callable] = None
    # Rewrite a payload that arrives in the tool's own event vocabulary.
    translate: Optional[Callable] = None

    # Text shown to people: how the lead stays awake, and how it runs a watcher.
    loop_hint: Optional[str] = None
    background_hint: Optional[str] = None
    background_use: Optional[str] = None
    # The lines of the skill that name the tool's own features.
    skill_demo: Optional[str] = None
    skill_loop: Optional[str] = None
    skill_background: Optional[str] = None
    skill_lanes: tuple = ()
    # How the skill names the tool's own way of starting work outside the board, the
    # label of its worker-start lane, and why a lane without native hooks has none.
    skill_delegation: Optional[str] = None
    lane_label: Optional[str] = None
    skill_no_hooks: Optional[str] = None

    # The demo: `demo_command(sockets, sandbox, lead, trust)` returns the command.
    demo_command: Optional[Callable] = None
    demo_trust: bool = False


_loaded = {}


def load():
    """Import every adapter file beside this one and return the adapters by key."""
    folder = Path(__file__).resolve().parent
    if folder in _loaded:
        return _loaded[folder]
    found = {}
    for path in sorted(folder.glob("adapter_*.py")):
        name = path.stem
        module = sys.modules.get(name)
        if module is None:
            spec = importlib.util.spec_from_file_location(name, path)
            module = importlib.util.module_from_spec(spec)
            sys.modules[name] = module
            spec.loader.exec_module(module)
        adapter = getattr(module, "ADAPTER", None)
        if adapter is not None:
            found[adapter.key] = adapter
    _loaded[folder] = found
    return found


def all_adapters():
    return sorted(load().values(), key=lambda adapter: (adapter.rank, adapter.key))


def get(key):
    return load()[key]


def agents():
    """The adapters of tools people run, in listing order."""
    return [adapter for adapter in all_adapters() if adapter.display]


def join_words(words, joiner="or"):
    words = list(words)
    if len(words) <= 2:
        return f" {joiner} ".join(words)
    return f"{', '.join(words[:-1])} {joiner} {words[-1]}"


def programs():
    """The command names of the tools, in listing order."""
    return tuple(program for adapter in agents() for program in adapter.programs)


def demo_tools():
    return tuple(adapter.key for adapter in agents() if adapter.demo_command)


def session_env_names():
    ordered = sorted(agents(), key=lambda adapter: (adapter.session_priority, adapter.key))
    return tuple(name for adapter in ordered for name in adapter.session_env)


def union(attribute):
    result = frozenset()
    for adapter in all_adapters():
        result |= frozenset(getattr(adapter, attribute))
    return result


def union_of(collect):
    result = frozenset()
    for adapter in all_adapters():
        result |= frozenset(collect(adapter))
    return result


def merged(attribute):
    result = {}
    for adapter in all_adapters():
        result.update(getattr(adapter, attribute))
    return result


def tool_for(payload, named=None):
    """Name the tool a payload came from.

    A payload that matches an adapter belongs to it, unless the installed command
    named its tool. The adapter marked `default` is the answer when nothing else
    matches (an older install that did not name its tool).
    """
    adapters = all_adapters()
    for adapter in adapters:
        if adapter.key == "generic" and adapter.matches(payload):
            return adapter.key
    if named in load():
        return named
    for adapter in adapters:
        if adapter.matches(payload):
            return adapter.key
    return next(adapter.key for adapter in adapters if adapter.default)


def program_for(command_name):
    """The adapter that runs under this command name, or None."""
    return next((adapter for adapter in agents() if command_name in adapter.programs), None)


def native_adapter(program):
    """The adapter whose worker-start wiring applies to this command name, or None."""
    adapter = program_for(program)
    return adapter if adapter is not None and adapter.hook_arguments else None


def hook_tool(program):
    """The `malak hook --tool` value for a command name that gets native hooks, or None."""
    adapter = native_adapter(program)
    return None if adapter is None else adapter.delivery_tool or adapter.key


def names(joiner="or"):
    """"Claude Code or Codex": the tools' names for a sentence."""
    return join_words((adapter.display for adapter in agents()), joiner)


def render_skill(text):
    """Fill the skill's `{{...}}` lines from the adapters' own text."""
    adapters = agents()
    everyone = all_adapters()
    blocks = {
        "demo": join_words(a.skill_demo for a in adapters if a.skill_demo),
        "loop": " ".join(a.skill_loop for a in adapters if a.skill_loop),
        "background": join_words(a.skill_background for a in adapters if a.skill_background),
        "delegation": join_words((a.skill_delegation for a in everyone if a.skill_delegation), "and"),
        "lanes": "\n".join(line for a in everyone for line in a.skill_lanes),
        "native_lanes": join_words((a.lane_label for a in everyone if a.hook_arguments and a.lane_label), "and"),
        "no_hook_lanes": " ".join(a.skill_no_hooks for a in everyone if a.skill_no_hooks),
        "names": names(),
    }
    for key, value in blocks.items():
        text = text.replace("{{" + key + "}}", value)
    return text


def session_export():
    """The variable the CLI reads for an agent session ID that a recipe passes on."""
    return session_env_names()[0]


def lead_loop_lines():
    """The lines that tell a lead how each tool keeps it awake."""
    return [a.loop_hint for a in agents() if a.loop_hint]


def lead_background_line():
    """The line that tells a lead how each tool runs the lead watcher in the background."""
    background = join_words(a.background_hint for a in agents() if a.background_hint)
    return f"in {background}, and run it again after each output."


if __name__ == "__main__":
    # Text for the shell recipes: `python3 adapters.py demo-tools` and the like.
    command = sys.argv[1] if sys.argv[1:] else ""
    if command == "demo-tools":
        print("|".join(demo_tools()))
    elif command == "lead-loop":
        print("\n".join(lead_loop_lines()))
    elif command == "lead-background":
        print(lead_background_line())
    else:
        sys.exit("usage: adapters.py demo-tools | lead-loop | lead-background")
