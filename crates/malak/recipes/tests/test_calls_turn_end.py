"""Large call batches keep the existing turn-end text budget."""
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from lead_turn_end import BUDGET, handover_text, signature, turn_end


class CallsBudgetTests(unittest.TestCase):
    def test_many_call_titles_report_omissions_within_the_byte_budget(self):
        calls = [{"channel": "calls", "position": position, "creation": "c",
                  "sender": "lead", "kind": "note", "text": "🦀" * 1000}
                 for position in range(1000)]
        text = handover_text("fixture", "malak --board fixture", [], [], [], 0, 0, calls)
        self.assertLessEqual(len(text.encode()), BUDGET)
        self.assertIn("notifications omitted", text)
        self.assertIn("calls --all", text)

    def test_old_notes_do_not_repeat_after_red_work_closes_but_new_notes_arrive(self):
        note = {"type": "note", "channel": "calls", "position": 1,
                "creation": "c", "sender": "lead", "kind": "note", "text": "First decision"}
        last = signature("fixture", [], [], [], [note, {"type": "call", "level": "red"}])
        with patch("lead_turn_end.pending_lead", return_value=[]), \
             patch("lead_turn_end.open_questions", return_value=([], 0)), \
             patch("lead_turn_end.available", side_effect=[[], [note], [], [note, {**note, "position": 2}]]), \
             patch("lead_turn_end.board_selector", return_value="--board fixture"):
            reply, last = turn_end("fixture", None, None, {}, last, False, 0)
            self.assertIsNone(reply)
            reply, _ = turn_end("fixture", None, None, {}, last, False, 0)
            self.assertEqual(reply["decision"], "block")
