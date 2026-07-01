import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from phonelink import atomicio


class AtomicWriteTests(unittest.TestCase):
    def setUp(self):
        self._dir = tempfile.TemporaryDirectory()
        self.root = Path(self._dir.name)

    def tearDown(self):
        self._dir.cleanup()

    def test_writes_content_and_creates_parents(self):
        target = self.root / "a" / "b" / "settings.json"
        atomicio.atomic_write_text(target, "hello")
        self.assertEqual(target.read_text(), "hello")

    def test_json_round_trip(self):
        target = self.root / "c.json"
        atomicio.atomic_write_json(target, {"x": [1, 2], "y": "é"})
        import json
        self.assertEqual(json.loads(target.read_text()), {"x": [1, 2], "y": "é"})

    def test_overwrite_is_atomic_and_leaves_no_temp_files(self):
        target = self.root / "d.txt"
        atomicio.atomic_write_text(target, "v1")
        atomicio.atomic_write_text(target, "v2")
        self.assertEqual(target.read_text(), "v2")
        # No leftover .tmp scratch files in the directory.
        self.assertEqual(list(self.root.glob("*.tmp")), [])
        self.assertEqual([p.name for p in self.root.iterdir()], ["d.txt"])

    def test_failed_write_preserves_old_file_and_cleans_temp(self):
        target = self.root / "e.txt"
        atomicio.atomic_write_text(target, "original")
        # Simulate a crash after the temp file is written but before replace.
        with mock.patch("os.replace", side_effect=OSError("boom")):
            with self.assertRaises(OSError):
                atomicio.atomic_write_text(target, "corrupt")
        # Old content is intact; no temp debris left behind.
        self.assertEqual(target.read_text(), "original")
        self.assertEqual(
            [p.name for p in self.root.iterdir() if p.name.endswith(".tmp")], []
        )


if __name__ == "__main__":
    unittest.main()
