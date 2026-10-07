# LabSweep

Worldwide Adventure Labs discovery, detail, review, answer-recovery, and history pipeline.

## Web viewer

The repository includes a static MapLibre viewer in `web/`. GitHub Pages rebuilds it from the latest `db-latest` database snapshot after every successful daily refresh, so the site tracks the rolling dataset without committing generated data to git.

Viewer features include:

- searchable worldwide catalog with virtualized results
- clustered map, heatmap, and all-points modes
- filtering and sorting by type, rating, stages, reviews, recommendation, fetch state, and archive state
- full Adventure Lab details, stages, recovered answers, review samples, and images
- per-adventure version history and a global recent-changes feed
- shareable deep links to individual labs
- dataset freshness/history counters
- browser geolocation jump and CSV export of the current filtered result set

Generated Pages data lives under `web/data/` only during the Pages build. It is intentionally gitignored. The exporter writes a compact catalog, lazy detail shards, lazy history shards, recent changes, and dataset metadata.

### Update flow

1. `daily.yml` refreshes the rolling SQLite database and publishes `db-latest`.
2. The `pages` workflow runs automatically when `daily` completes successfully.
3. It restores `db-latest`, runs `labsweep export`, packages `web/`, and deploys GitHub Pages.
4. Viewer-code changes on `main` also trigger a Pages rebuild against the latest database.

The viewer is static and has no credentials or server-side component. Authentication material is stripped before database publication and is not part of the exported dataset.
