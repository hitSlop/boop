import os
import runpy
import sqlite3
import tempfile
import unittest
from pathlib import Path


stamps = runpy.run_path(str(Path(__file__).resolve().parents[1] / "scripts/pool-build-stamps.py"))


class BuildStampTests(unittest.TestCase):
    def test_restore_only_inherited_inputs_once(self):
        with tempfile.TemporaryDirectory() as directory:
            folder = Path(directory)
            root = folder / "project"
            db = root / "xtool/.xtool-tmp/DerivedData/Intermediates.noindex/XCBuildData/build.db"
            db.parent.mkdir(parents=True)
            paths = [root / "source", root / "modified", root / "removed", folder / "unrelated"]
            precise = 1_740_000_000_123_456_789
            rounded = 1_740_000_000_000_000_000
            with sqlite3.connect(db) as conn:
                conn.execute("CREATE TABLE key_names (key STRING)")
                for path in paths:
                    path.write_text("a")
                    os.utime(path, ns=(precise, precise))
                    conn.execute("INSERT INTO key_names VALUES (?)", ("N" + str(path),))

            record = folder / "stamps.json.gz"
            self.assertEqual(stamps["capture"](root, record), 3)
            paths[1].write_text("new input")
            for path in paths:
                os.utime(path, ns=(rounded, rounded))
            paths[2].unlink()
            self.assertEqual(stamps["restore"](root, record), 1)
            self.assertEqual(paths[0].stat().st_mtime_ns, precise)
            self.assertEqual(paths[1].stat().st_mtime_ns, rounded)
            self.assertEqual(paths[3].stat().st_mtime_ns, rounded)
            self.assertEqual(stamps["restore"](root, record), 0)

    def test_restore_directory_only_if_membership_matches(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            db = root / "xtool/.xtool-tmp/DerivedData/Intermediates.noindex/XCBuildData/build.db"
            db.parent.mkdir(parents=True)
            original = 1_740_000_000_000_000_000
            unpacked = original + 30_000_000_000
            first, second = root / "a", root / "b"
            with sqlite3.connect(db) as conn:
                conn.execute("CREATE TABLE key_names (key STRING)")
                for path in (first, second):
                    path.mkdir()
                    os.utime(path, ns=(original, original))
                    conn.execute("INSERT INTO key_names VALUES (?)", ("N" + str(path),))
            record = root / "snapshot.json.gz"
            self.assertEqual(stamps["capture"](root, record), 2)
            (second / "new-file").touch()
            for path in (first, second):
                os.utime(path, ns=(unpacked, unpacked))
            self.assertEqual(stamps["restore"](root, record), 1)
            self.assertEqual(first.stat().st_mtime_ns, original)
            self.assertEqual(second.stat().st_mtime_ns, unpacked)
