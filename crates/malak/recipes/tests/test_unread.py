"""A worker's unacknowledged inbox messages, as `recipe workers` shows them."""

from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from worker_queue import unread_words

WORKER = "w1"


def status(floor, head, acknowledged=None):
    readers = {} if acknowledged is None else {WORKER: acknowledged}
    return {"channels": [{"name": f"to/{WORKER}", "floor": floor, "head": head, "readers": readers}]}


def lead(inbox, position):
    return "lead"


class UnreadWordsTests(unittest.TestCase):
    def test_an_inbox_that_was_never_written_has_nothing_unread(self):
        self.assertIsNone(unread_words(status(1, 1), WORKER, lead))

    def test_a_board_without_the_inbox_has_nothing_unread(self):
        self.assertIsNone(unread_words({"channels": []}, WORKER, lead))

    def test_messages_never_acknowledged_are_counted_from_the_floor(self):
        self.assertEqual(unread_words(status(1, 3), WORKER, lead), "2 unread (lead #1, lead #2)")

    def test_acknowledged_messages_are_not_counted(self):
        self.assertEqual(unread_words(status(1, 4, acknowledged=2), WORKER, lead), "1 unread (lead #3)")

    def test_everything_acknowledged_leaves_nothing_unread(self):
        self.assertIsNone(unread_words(status(1, 3, acknowledged=2), WORKER, lead))

    def test_trimmed_messages_are_not_counted(self):
        self.assertEqual(unread_words(status(3, 4, acknowledged=1), WORKER, lead), "1 unread (lead #3)")

    def test_a_missing_sender_leaves_only_the_position(self):
        self.assertEqual(unread_words(status(1, 2), WORKER, lambda inbox, position: None), "1 unread (#1)")

    def test_a_long_list_names_the_first_few_and_counts_the_rest(self):
        self.assertEqual(
            unread_words(status(1, 8), WORKER, lead),
            "7 unread (lead #1, lead #2, lead #3, lead #4, lead #5, and 2 more)",
        )

    def test_a_board_that_cannot_report_status_has_nothing_unread(self):
        self.assertIsNone(unread_words(None, WORKER, lead))


if __name__ == "__main__":
    unittest.main()
