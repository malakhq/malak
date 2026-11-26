"""The durative adapter: a sandbox runner that wraps another agent command."""

from adapters import Adapter

ADAPTER = Adapter(
    key="durative",
    rank=8,
    delegation_runners={"durative": "durative"},
    start_examples={"durative": "durative exec ID -- codex exec ..."},
    skill_delegation="`durative exec`",
    lane_label="Sandbox",
    skill_no_hooks="The sandbox lane runs `durative`, so no native hooks are wired:",
    skill_lanes=(
        "  - Sandbox lane: `malak --board NAME recipe worker-start WORKER --brief FILE -- durative exec ID -- codex exec ...`",
    ),
)
