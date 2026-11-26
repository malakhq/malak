"""Check the public API listing: its renderers, and the crate against docs/public-api.txt."""

from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import public_api

SELF_REF = {"borrowed_ref": {"lifetime": None, "is_mutable": False, "type": {"generic": "Self"}}}
TEXT = {"resolved_path": {"path": "Result", "id": 1, "args": {"angle_bracketed": {
    "args": [{"type": {"tuple": []}}, {"type": {"resolved_path": {"path": "Error", "id": 2, "args": None}}}],
    "constraints": [],
}}}}


class RenderTests(unittest.TestCase):
    def function(self, inputs, output):
        return {
            "sig": {"inputs": inputs, "output": output},
            "generics": {"params": [], "where_predicates": []},
            "header": {"is_const": False, "is_unsafe": False, "is_async": False},
        }

    def test_a_method_shows_its_receiver_and_result(self):
        function = self.function([["self", SELF_REF], ["count", {"primitive": "u64"}]], TEXT)
        self.assertEqual(
            public_api.render_function("Handle::ack", function),
            "fn Handle::ack(&self, count: u64) -> Result<(), Error>",
        )

    def test_a_by_value_receiver_shows_as_self(self):
        function = self.function([["self", {"generic": "Self"}]], None)
        self.assertEqual(public_api.render_function("Kind::as_str", function), "fn Kind::as_str(self)")

    def test_a_hidden_item_is_not_listed(self):
        self.assertTrue(public_api.hidden({"visibility": "public", "attrs": ["#[doc(hidden)]"]}))
        self.assertFalse(public_api.hidden({"visibility": "public", "attrs": []}))


class SnapshotTests(unittest.TestCase):
    def test_the_public_api_matches_its_snapshot(self):
        self.assertEqual(public_api.main([]), 0)


if __name__ == "__main__":
    unittest.main()
