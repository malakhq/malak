"""The size budgets of the board-only builds are chosen for the platform."""

from importlib.machinery import SourceFileLoader
from importlib.util import module_from_spec, spec_from_loader
from pathlib import Path
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "board-size"
spec = spec_from_loader("board_size", SourceFileLoader("board_size", str(SCRIPT)))
board_size = module_from_spec(spec)
spec.loader.exec_module(board_size)

# Bytes the Linux CI run 45706507764 measured for the memory store, the Markdown
# store and the served board.
LINUX_MEASURED = {"": 637_584, "markdown": 829_088, board_size.SERVER_FEATURES: 1_312_336}


class BudgetTests(unittest.TestCase):
    def test_linux_budgets_hold_the_measured_sizes(self):
        for features, size in LINUX_MEASURED.items():
            self.assertGreater(board_size.budgets("linux")[features], size, features)

    def test_linux_budgets_stay_within_twelve_percent_of_the_measured_sizes(self):
        for features, size in LINUX_MEASURED.items():
            self.assertLess(board_size.budgets("linux")[features], size * 1.12, features)

    def test_macos_budgets_are_unchanged(self):
        self.assertEqual(
            board_size.budgets("darwin"),
            {"": 640_000, "markdown": 760_000, board_size.SERVER_FEATURES: 1_130_000},
        )

    def test_each_platform_budgets_the_same_builds(self):
        self.assertEqual(board_size.budgets("linux").keys(), board_size.budgets("darwin").keys())

    def test_an_unknown_platform_has_no_budget(self):
        with self.assertRaises(SystemExit):
            board_size.budgets("plan9")


if __name__ == "__main__":
    unittest.main()
