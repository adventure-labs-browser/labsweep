#!/usr/bin/env python3
import gzip
import json
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


class PagesExportTest(unittest.TestCase):
    def test_history_diff_and_meta(self):
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            db_path = root / "test.db"
            out = root / "out"
            db = sqlite3.connect(db_path)
            db.executescript(
                """
                CREATE TABLE queue(status TEXT);
                CREATE TABLE labs(guid TEXT PRIMARY KEY, raw_json TEXT);
                CREATE TABLE adventures(
                  guid TEXT PRIMARY KEY,title TEXT,raw_json TEXT,fetched_at TEXT,
                  status TEXT,version_seq INTEGER,reviews_checked_at TEXT);
                CREATE TABLE stages(
                  adventure_guid TEXT,stage_index INTEGER,raw_json TEXT,status TEXT,
                  version_seq INTEGER,PRIMARY KEY(adventure_guid,stage_index));
                CREATE TABLE reviews(
                  id INTEGER PRIMARY KEY,adventure_guid TEXT,raw_json TEXT,status TEXT,
                  version_seq INTEGER);
                CREATE INDEX idx_reviews_adv ON reviews(adventure_guid);
                CREATE TABLE adventure_versions(
                  adventure_guid TEXT,version_seq INTEGER,superseded_at TEXT,change TEXT,
                  status TEXT,http_status INTEGER,raw_json TEXT,
                  PRIMARY KEY(adventure_guid,version_seq));
                CREATE TABLE stage_versions(
                  adventure_guid TEXT,stage_index INTEGER,version_seq INTEGER,
                  superseded_at TEXT,change TEXT,raw_json TEXT,
                  PRIMARY KEY(adventure_guid,stage_index,version_seq));
                CREATE TABLE review_versions(
                  review_id INTEGER,version_seq INTEGER,superseded_at TEXT,change TEXT,
                  raw_json TEXT,PRIMARY KEY(review_id,version_seq));
                CREATE TABLE cracks(adventure_guid TEXT,stage_index INTEGER,hash TEXT);
                """
            )

            guid = "ab000000-0000-0000-0000-000000000001"
            old = {
                "adventureGuid": guid,
                "title": "Old title",
                "ratingsAverage": 4.0,
                "stageSummaries": [{"title": "Old stage", "question": "Old Q"}],
                "location": {"latitude": 1.0, "longitude": 2.0},
            }
            new = {
                "adventureGuid": guid,
                "title": "New title",
                "ratingsAverage": 4.5,
                "stageSummaries": [{"title": "New stage", "question": "New Q"}],
                "location": {"latitude": 1.0, "longitude": 2.0},
            }
            old_stage = {"title": "Old stage", "question": "Old Q", "challengeType": "Question"}
            new_stage = {"title": "New stage", "question": "New Q", "challengeType": "Question"}
            old_review = {"playerUsername": "tester", "rating": 4, "reviewText": "old review"}
            new_review = {"playerUsername": "tester", "rating": 5, "reviewText": "new review"}

            db.execute("INSERT INTO queue VALUES ('done')")
            db.execute("INSERT INTO labs VALUES (?,?)", (guid, json.dumps(new)))
            db.execute(
                "INSERT INTO adventures VALUES (?,?,?,?,?,?,?)",
                (guid, "New title", json.dumps(new), "2026-10-07 20:00:00", "active", 2, "2026-10-07 20:01:00"),
            )
            db.execute("INSERT INTO stages VALUES (?,?,?,?,?)", (guid, 0, json.dumps(new_stage), "active", 2))
            db.execute("INSERT INTO reviews VALUES (?,?,?,?,?)", (1, guid, json.dumps(new_review), "active", 2))
            db.execute(
                "INSERT INTO adventure_versions VALUES (?,?,?,?,?,?,?)",
                (guid, 1, "2026-10-07 19:00:00", "updated", "active", 200, json.dumps(old)),
            )
            db.execute(
                "INSERT INTO stage_versions VALUES (?,?,?,?,?,?)",
                (guid, 0, 1, "2026-10-07 19:00:01", "updated", json.dumps(old_stage)),
            )
            db.execute(
                "INSERT INTO review_versions VALUES (?,?,?,?,?)",
                (1, 1, "2026-10-07 19:00:02", "updated", json.dumps(old_review)),
            )
            db.execute("INSERT INTO cracks VALUES (?,?,?)", (guid, 0, "x"))
            db.commit()
            db.close()

            subprocess.run(
                [
                    sys.executable,
                    str(Path(__file__).with_name("export_pages_meta.py")),
                    "--db", str(db_path),
                    "--out", str(out),
                ],
                check=True,
            )

            meta = json.loads((out / "meta.json").read_text())
            self.assertEqual(meta["counts"]["historyEvents"], 3)
            self.assertEqual(meta["counts"]["historyLabs"], 1)

            with gzip.open(out / "history" / "ab.json.gz", "rt") as fh:
                history = json.load(fh)[guid]
            self.assertEqual(len(history), 3)
            adv = next(e for e in history if e["kind"] == "adventure")
            self.assertEqual(adv["diff"]["title"], ["Old title", "New title"])
            stage = next(e for e in history if e["kind"] == "stage")
            self.assertEqual(stage["diff"]["question"], ["Old Q", "New Q"])
            review = next(e for e in history if e["kind"] == "review")
            self.assertEqual(review["diff"]["rating"], [4, 5])


if __name__ == "__main__":
    unittest.main()
