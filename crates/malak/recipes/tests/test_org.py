"""Role keys keep the two seats of a promoted lead independent."""

from pathlib import Path
import sys
import unittest
from unittest.mock import Mock, patch

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    import org_state
    import org_seats


class RosterRolesTests(unittest.TestCase):
    def test_a_lead_and_chief_with_the_same_name_keep_separate_reader_keys(self):
        roster = {"top": "/top.toml", "leads": {"same": {"config": "/lead.toml", "active": True}},
                  "chief_of_staff": {"name": "same", "session": {"pid": 2}}, "candidate": None}
        readers = org_state.readers(roster)
        self.assertEqual(set(readers), {"lead:same", "chief_of_staff:same"})
        self.assertEqual(readers["lead:same"]["config"], "/lead.toml")
        self.assertEqual(readers["chief_of_staff:same"]["config"], "/top.toml")


class ReaderSeatNamesTests(unittest.TestCase):
    def test_two_readers_with_colliding_short_hashes_receive_distinct_names(self):
        participants = []
        # Different full hashes with the same first 20 characters.
        hashes = {b"lead:first": "a" * 20 + "b" * 44,
                  b"lead:second": "a" * 20 + "c" * 44,
                  b"lead:second:1": "d" * 64}
        with patch.object(org_seats.board, "board_status", return_value={"participants": participants}), \
                patch.object(org_seats.hashlib, "sha256", side_effect=lambda text: Mock(hexdigest=lambda: hashes[text])):
            first = org_seats.available_name("/target.toml", "lead:first")
            participants.append({"name": first, "revoked": False})
            second = org_seats.available_name("/target.toml", "lead:second")
        self.assertNotEqual(first, second)
        self.assertEqual(first, "org-" + "a" * 20)
        self.assertEqual(second, "org-" + "d" * 20)
        for name in (first, second):
            org_seats.board.check_new_participant_name(name)
