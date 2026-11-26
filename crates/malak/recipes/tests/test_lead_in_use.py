"""Check when lead-up must refuse to take over a lead."""

import os
from pathlib import Path
import sys
import unittest
from unittest.mock import patch


RECIPES = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(RECIPES))
import board_data  # noqa: E402


def bound(pid, state="bound"):
    return {
        "state": state,
        "session": {"pid": pid, "program": "claude", "folder": "/work"},
        "history": [{"event": "issued", "at_ms": 1_000}],
    }


class LeadInUseTests(unittest.TestCase):
    def test_a_lead_bound_to_a_running_session_is_in_use(self):
        reason = board_data.lead_in_use({"claim": bound(os.getpid())})

        self.assertIn(f"pid {os.getpid()}", reason)
        self.assertIn("program claude", reason)
        self.assertIn("folder /work", reason)

    def test_a_lead_bound_to_a_gone_session_is_free(self):
        with patch.object(board_data, "session_alive", return_value=False):
            self.assertIsNone(board_data.lead_in_use({"claim": bound(1)}))

    def test_an_ended_claim_is_free(self):
        self.assertIsNone(board_data.lead_in_use({"claim": bound(os.getpid(), "ended")}))

    def test_an_unbound_lead_with_an_open_connection_is_in_use(self):
        self.assertIn("open connection", board_data.lead_in_use({"connections": 1}))

    def test_an_unbound_lead_without_connections_is_free(self):
        self.assertIsNone(board_data.lead_in_use({"connections": 0}))


if __name__ == "__main__":
    unittest.main()
