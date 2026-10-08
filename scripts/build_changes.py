#!/usr/bin/env python3
"""Build a balanced recent-changes index from exported history shards."""

from __future__ import annotations

import gzip
import json
import sys
from pathlib import Path

PER_KIND = 2_000


def main() -> int:
    root = Path(sys.argv[1] if len(sys.argv) > 1 else "web/data")
    history_dir = root / "history"
    reviews: list[dict] = []
    other: list[dict] = []

    for path in sorted(history_dir.glob("*.json.gz")):
        with gzip.open(path, "rt", encoding="utf-8") as f:
            shard = json.load(f)
        for guid, events in shard.items():
            for event in events:
                row = {k: v for k, v in event.items() if k != "d"}
                row["g"] = guid
                (reviews if row.get("e") == "review" else other).append(row)

    key = lambda x: x.get("at") or ""
    reviews.sort(key=key, reverse=True)
    other.sort(key=key, reverse=True)
    merged = reviews[:PER_KIND] + other[:PER_KIND]
    merged.sort(key=key, reverse=True)

    out = root / "changes.json.gz"
    with gzip.open(out, "wt", encoding="utf-8", compresslevel=9) as f:
        json.dump(merged, f, ensure_ascii=False, separators=(",", ":"))

    print(
        f"changes: {len(merged)} total "
        f"({min(len(reviews), PER_KIND)} review, {min(len(other), PER_KIND)} non-review)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
