#!/usr/bin/env python3
"""Export small, browser-friendly history/metadata files for GitHub Pages.

The main Rust exporter owns the current catalog/detail shards.  This script adds
version history without duplicating full historical JSON snapshots into the
site.  Each event contains a compact diff of useful fields and is sharded by
the first two GUID characters, matching the detail exporter.
"""

from __future__ import annotations

import argparse
import gzip
import json
import os
import sqlite3
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


def parse_json(raw: str | None) -> dict[str, Any]:
    if not raw:
        return {}
    try:
        value = json.loads(raw)
        return value if isinstance(value, dict) else {}
    except json.JSONDecodeError:
        return {}


def clip(value: Any, limit: int = 240) -> Any:
    if not isinstance(value, str):
        return value
    value = " ".join(value.split())
    return value if len(value) <= limit else value[: limit - 1] + "…"


def location(value: Any) -> str | None:
    if not isinstance(value, dict):
        return None
    lat, lon = value.get("latitude"), value.get("longitude")
    if isinstance(lat, (int, float)) and isinstance(lon, (int, float)):
        return f"{lat:.5f}, {lon:.5f}"
    return None


def adv_summary(raw: str | None) -> dict[str, Any]:
    v = parse_json(raw)
    stages = v.get("stageSummaries")
    return {
        "title": clip(v.get("title")),
        "type": v.get("adventureType"),
        "rating": v.get("ratingsAverage"),
        "ratings": v.get("ratingsTotalCount"),
        "reviews": v.get("reviewsTotalCount"),
        "completions": v.get("completionCount"),
        "recommended": v.get("recommendedCount"),
        "owner": v.get("ownerUsername"),
        "visibility": v.get("visibility"),
        "archived": v.get("isArchived"),
        "highlyRecommended": v.get("isHighlyRecommended"),
        "stages": len(stages) if isinstance(stages, list) else v.get("stagesTotalCount"),
        "location": location(v.get("location")),
        "description": clip(v.get("description")),
    }


def stage_summary(raw: str | None) -> dict[str, Any]:
    v = parse_json(raw)
    return {
        "title": clip(v.get("title")),
        "type": v.get("challengeType"),
        "question": clip(v.get("question")),
        "description": clip(v.get("description")),
        "radius": v.get("geofencingRadius"),
        "location": location(v.get("location")),
    }


def review_summary(raw: str | None) -> dict[str, Any]:
    v = parse_json(raw)
    return {
        "player": v.get("playerUsername"),
        "rating": v.get("rating"),
        "recommended": v.get("recommended"),
        "creator": v.get("isCreator"),
        "created": v.get("createdUtc"),
        "text": clip(v.get("reviewText")),
    }


def clean_summary(v: dict[str, Any]) -> dict[str, Any]:
    return {k: x for k, x in v.items() if x is not None and x != ""}


def diff(before: dict[str, Any], after: dict[str, Any]) -> dict[str, list[Any]]:
    out: dict[str, list[Any]] = {}
    for key in before.keys() | after.keys():
        a, b = before.get(key), after.get(key)
        if a != b:
            out[key] = [a, b]
    return out


def write_gz(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode()
    with gzip.open(path, "wb", compresslevel=6) as fh:
        fh.write(payload)


def scalar(db: sqlite3.Connection, sql: str, default: Any = 0) -> Any:
    try:
        row = db.execute(sql).fetchone()
        return default if row is None or row[0] is None else row[0]
    except sqlite3.Error:
        return default


def table_exists(db: sqlite3.Connection, name: str) -> bool:
    return bool(
        db.execute(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?", (name,)
        ).fetchone()
    )


def collect_events_for_prefix(db: sqlite3.Connection, prefix: str) -> dict[str, list[dict[str, Any]]]:
    pat = prefix + "*"
    by_guid: dict[str, list[dict[str, Any]]] = defaultdict(list)

    # Current snapshots let us compare the final archived version with live state.
    current_adv = {
        guid: (int(seq or 1), raw)
        for guid, seq, raw in db.execute(
            "SELECT guid, COALESCE(version_seq,1), raw_json FROM adventures "
            "WHERE guid GLOB ?",
            (pat,),
        )
    }
    adv_rows: dict[str, list[tuple[int, str, str, str | None]]] = defaultdict(list)
    if table_exists(db, "adventure_versions"):
        for guid, seq, at, change, raw in db.execute(
            "SELECT adventure_guid, version_seq, superseded_at, change, raw_json "
            "FROM adventure_versions WHERE adventure_guid GLOB ? "
            "ORDER BY adventure_guid, version_seq",
            (pat,),
        ):
            adv_rows[guid].append((int(seq), at, change, raw))

    for guid, rows in adv_rows.items():
        live = current_adv.get(guid)
        for i, (seq, at, change, raw) in enumerate(rows):
            before = clean_summary(adv_summary(raw))
            if i + 1 < len(rows):
                after = clean_summary(adv_summary(rows[i + 1][3]))
            elif live:
                after = clean_summary(adv_summary(live[1]))
            else:
                after = {}
            changes = diff(before, after)
            if change == "removed":
                changes.setdefault("status", ["active", "removed"])
            elif change == "restored":
                changes.setdefault("status", ["removed", "active"])
            by_guid[guid].append(
                {
                    "at": at,
                    "kind": "adventure",
                    "change": change,
                    "v": seq,
                    "diff": changes,
                }
            )

    current_stage: dict[tuple[str, int], tuple[int, str | None]] = {
        (guid, int(idx)): (int(seq or 1), raw)
        for guid, idx, seq, raw in db.execute(
            "SELECT adventure_guid, stage_index, COALESCE(version_seq,1), raw_json "
            "FROM stages WHERE adventure_guid GLOB ?",
            (pat,),
        )
    }
    stage_rows: dict[tuple[str, int], list[tuple[int, str, str, str | None]]] = defaultdict(list)
    if table_exists(db, "stage_versions"):
        for guid, idx, seq, at, change, raw in db.execute(
            "SELECT adventure_guid, stage_index, version_seq, superseded_at, change, raw_json "
            "FROM stage_versions WHERE adventure_guid GLOB ? "
            "ORDER BY adventure_guid, stage_index, version_seq",
            (pat,),
        ):
            stage_rows[(guid, int(idx))].append((int(seq), at, change, raw))

    for (guid, idx), rows in stage_rows.items():
        live = current_stage.get((guid, idx))
        for i, (seq, at, change, raw) in enumerate(rows):
            before = clean_summary(stage_summary(raw))
            if i + 1 < len(rows):
                after = clean_summary(stage_summary(rows[i + 1][3]))
            elif live:
                after = clean_summary(stage_summary(live[1]))
            else:
                after = {}
            changes = diff(before, after)
            if change in {"removed", "deleted"}:
                changes.setdefault("status", ["active", "removed"])
            elif change == "restored":
                changes.setdefault("status", ["removed", "active"])
            by_guid[guid].append(
                {
                    "at": at,
                    "kind": "stage",
                    "index": idx,
                    "change": change,
                    "v": seq,
                    "diff": changes,
                }
            )

    # Review history is keyed by review id. Join through the current review row to
    # recover the adventure GUID, then compare archived snapshots with current.
    if table_exists(db, "review_versions"):
        current_review = {
            int(rid): (guid, int(seq or 1), raw)
            for rid, guid, seq, raw in db.execute(
                "SELECT id, adventure_guid, COALESCE(version_seq,1), raw_json "
                "FROM reviews WHERE adventure_guid GLOB ?",
                (pat,),
            )
        }
        review_rows: dict[int, list[tuple[int, str, str, str | None]]] = defaultdict(list)
        for rid, seq, at, change, raw in db.execute(
            "SELECT rv.review_id, rv.version_seq, rv.superseded_at, rv.change, rv.raw_json "
            "FROM review_versions rv JOIN reviews r ON r.id=rv.review_id "
            "WHERE r.adventure_guid GLOB ? ORDER BY rv.review_id, rv.version_seq",
            (pat,),
        ):
            review_rows[int(rid)].append((int(seq), at, change, raw))

        for rid, rows in review_rows.items():
            live = current_review.get(rid)
            if not live:
                continue
            guid = live[0]
            for i, (seq, at, change, raw) in enumerate(rows):
                before = clean_summary(review_summary(raw))
                if i + 1 < len(rows):
                    after = clean_summary(review_summary(rows[i + 1][3]))
                else:
                    after = clean_summary(review_summary(live[2]))
                changes = diff(before, after)
                if change == "removed":
                    changes.setdefault("status", ["active", "removed"])
                elif change == "restored":
                    changes.setdefault("status", ["removed", "active"])
                by_guid[guid].append(
                    {
                        "at": at,
                        "kind": "review",
                        "id": rid,
                        "change": change,
                        "v": seq,
                        "diff": changes,
                    }
                )

    for events in by_guid.values():
        events.sort(key=lambda e: e.get("at") or "", reverse=True)
    return by_guid


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", required=True)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    out = Path(args.out)
    hist_dir = out / "history"
    hist_dir.mkdir(parents=True, exist_ok=True)
    for stale in hist_dir.glob("*.json.gz"):
        stale.unlink()

    db = sqlite3.connect(f"file:{Path(args.db).resolve()}?mode=ro", uri=True)
    db.execute("PRAGMA query_only=ON")
    db.execute("PRAGMA temp_store=MEMORY")

    recent: list[dict[str, Any]] = []
    history_events = 0
    history_labs = 0

    for i in range(256):
        prefix = f"{i:02x}"
        shard = collect_events_for_prefix(db, prefix)
        if not shard:
            continue
        write_gz(hist_dir / f"{prefix}.json.gz", shard)
        history_labs += len(shard)
        titles = dict(
            db.execute(
                "SELECT guid, title FROM adventures WHERE guid GLOB ?",
                (prefix + "*",),
            )
        )
        for guid, events in shard.items():
            title = titles.get(guid)
            history_events += len(events)
            for e in events:
                recent.append(
                    {
                        "g": guid,
                        "t": title,
                        "at": e.get("at"),
                        "kind": e.get("kind"),
                        "change": e.get("change"),
                        "index": e.get("index"),
                        "id": e.get("id"),
                    }
                )
        # Keep only the newest events as we go so a long-lived database does
        # not make the Pages build's memory use grow with all history.
        recent.sort(key=lambda e: e.get("at") or "", reverse=True)
        del recent[2000:]

    write_gz(hist_dir / "recent.json.gz", recent)

    queue = {}
    if table_exists(db, "queue"):
        queue = {
            str(status): int(count)
            for status, count in db.execute(
                "SELECT status, COUNT(*) FROM queue GROUP BY status"
            )
        }

    generated = datetime.now(timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")
    meta = {
        "generatedAt": generated,
        "source": {
            "pagesSha": os.environ.get("GITHUB_SHA", ""),
            "codeSha": os.environ.get("LABSWEEP_CODE_SHA", ""),
            "dbReleaseId": os.environ.get("LABSWEEP_DB_RELEASE_ID", ""),
            "dbPublishedAt": os.environ.get("LABSWEEP_DB_PUBLISHED_AT", ""),
            "runId": os.environ.get("GITHUB_RUN_ID", ""),
            "runNumber": os.environ.get("GITHUB_RUN_NUMBER", ""),
        },
        "counts": {
            "labs": scalar(db, "SELECT COUNT(*) FROM labs"),
            "adventures": scalar(db, "SELECT COUNT(*) FROM adventures"),
            "activeAdventures": scalar(
                db, "SELECT COUNT(*) FROM adventures WHERE COALESCE(status,'active')='active'"
            ),
            "removedAdventures": scalar(
                db, "SELECT COUNT(*) FROM adventures WHERE COALESCE(status,'active')='removed'"
            ),
            "stages": scalar(
                db, "SELECT COUNT(*) FROM stages WHERE COALESCE(status,'active')='active'"
            ),
            "reviews": scalar(
                db, "SELECT COUNT(*) FROM reviews WHERE COALESCE(status,'active')='active'"
            ),
            "crackedAnswers": scalar(db, "SELECT COUNT(*) FROM cracks")
            if table_exists(db, "cracks")
            else 0,
            "historyEvents": history_events,
            "historyLabs": history_labs,
            "adventureVersions": scalar(db, "SELECT COUNT(*) FROM adventure_versions")
            if table_exists(db, "adventure_versions")
            else 0,
            "stageVersions": scalar(db, "SELECT COUNT(*) FROM stage_versions")
            if table_exists(db, "stage_versions")
            else 0,
            "reviewVersions": scalar(db, "SELECT COUNT(*) FROM review_versions")
            if table_exists(db, "review_versions")
            else 0,
        },
        "queue": queue,
        "lastSeen": {
            "adventureFetch": scalar(db, "SELECT MAX(fetched_at) FROM adventures", None),
            "reviews": scalar(db, "SELECT MAX(reviews_checked_at) FROM adventures", None),
            "historyChange": max(
                [
                    str(scalar(db, "SELECT MAX(superseded_at) FROM adventure_versions", "") or ""),
                    str(scalar(db, "SELECT MAX(superseded_at) FROM stage_versions", "") or ""),
                    str(scalar(db, "SELECT MAX(superseded_at) FROM review_versions", "") or ""),
                ]
            )
            or None,
        },
    }
    (out / "meta.json").write_text(
        json.dumps(meta, ensure_ascii=False, separators=(",", ":")) + "\n",
        encoding="utf-8",
    )
    print(
        f"pages metadata: {history_events:,} history events across "
        f"{history_labs:,} labs; {len(recent[:2000]):,} recent events"
    )


if __name__ == "__main__":
    main()
