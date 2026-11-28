"""Owner-door seat operations, always bound to the reader's lead session."""

import json
import hashlib
from pathlib import Path

import board_data as board
import org_state
from recipe_board import board_name, board_settings

READ_CHANNELS = ("rail", "briefs", "calls", "questions", "lead", "relay/*", "to/*")


def session_of(reader):
    if "session" in reader:
        session = reader["session"]
    else:
        status = board.board_status(reader["config"])
        session = status.get("lead_session")
    if not session:
        raise board.RecipeError(f"reader has no recorded lead session: {reader['config']}")
    current = board.malak("recipe", "__session-process", str(session["pid"])).stdout
    actual = json.loads(current) if current else {}
    if any(actual.get(key) != session.get(key) for key in ("pid", "start_time")):
        raise board.RecipeError("reader's recorded lead session has ended; run lead-up again")
    return session


def revoke_seat(seat):
    status = board.board_status(seat["config"])
    person = next((p for p in status["participants"] if p["id"] == seat["id"]), None)
    if person and not person["revoked"]:
        board.malak("--config", seat["config"], "revoke", seat["id"])


def issue(top, roster, name):
    reader = org_state.readers(roster).get(name)
    if reader is None:
        raise board.RecipeError(f"{name!r} is not an active reader on the org roster")
    session = session_of(reader)
    old = roster["seats"].setdefault(name, [])
    # Always revoke before replacing so stale sessions cannot retain a seat.
    for seat in old:
        revoke_seat(seat)
    old.clear()
    org_state.save(top, roster)
    targets = [(lead_name, lead["config"], False) for lead_name, lead in roster["leads"].items()
               if lead["active"] and Path(lead["config"]) != Path(reader["config"])]
    if Path(reader["config"]) != Path(roster["top"]):
        targets.append((board_name(Path(roster["top"])), roster["top"], True))
    for target_name, config, is_top in targets:
        key = hashlib.sha256(f"{name}:{config}".encode()).hexdigest()
        path = org_state.folder(top) / f"{key}.handover"
        # guest is the one registration path. Old revoked names can be enrolled anew.
        path.unlink(missing_ok=True)
        result = board.malak("--config", config, "recipe", "guest", "org-" + hashlib.sha256(name.encode()).hexdigest()[:24],
                             "--out", path, "--session", str(session["pid"]), "--json")
        guest = json.loads(result.stdout)
        seat = {"board": target_name, "config": str(config), "id": guest["id"],
                "handover": str(path), "top": is_top}
        # Persist each issued identity before granting, so interrupted runs can revoke it.
        old.append(seat)
        org_state.save(top, roster)
        if is_top:
            grants = ["lead:send", "questions:send", "top/leads:send,read,get,ack",
                      "cos/ramp:read,get,ack", f"{guest['inbox']}:read,get,ack",
                      "rail:read,get", "briefs:read,get", "calls:read,get"]
        else:
            grants = [f"{channel}:read,get" for channel in READ_CHANNELS]
        board.malak("--config", config, "grant", guest["id"], *grants)
        if is_top:
            board.malak("--config", reader["config"], "recipe", "guest", "--register-home", path)
    return old


def reseat_for_board(config):
    found = org_state.find(config)
    if found is None:
        return
    top, _ = found
    with org_state.locked(top) as roster:
        if Path(roster["top"]) == Path(config) and roster.get("chief_of_staff"):
            session = board.board_status(config).get("lead_session")
            if session:
                roster["chief_of_staff"]["session"] = session
                org_state.save(top, roster)
        names = [name for name, reader in org_state.readers(roster).items()
                 if Path(reader["config"]) == Path(config) and not name.startswith("candidate:")]
        for name in names:
            issue(top, roster, name)
