"""Role keys keep the two seats of a promoted lead independent."""

from pathlib import Path
import sys
import unittest
from unittest.mock import patch

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    import org_state


class RosterRolesTests(unittest.TestCase):
    def test_a_lead_and_chief_with_the_same_name_keep_separate_reader_keys(self):
        roster = {"top": "/top.toml", "leads": {"same": {"config": "/lead.toml", "active": True}},
                  "chief_of_staff": {"name": "same", "session": {"pid": 2}}, "candidate": None}
        readers = org_state.readers(roster)
        self.assertEqual(set(readers), {"lead:same", "chief_of_staff:same"})
        self.assertEqual(readers["lead:same"]["config"], "/lead.toml")
        self.assertEqual(readers["chief_of_staff:same"]["config"], "/top.toml")
