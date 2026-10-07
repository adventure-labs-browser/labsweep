# labsweep

Worldwide Adventure Labs discovery, detail/review refresh, version-history and static viewer pipeline.

## Viewer

The GitHub Pages site is built from the rolling `db-latest` release after each successful daily refresh:

**https://adventure-labs-browser.github.io/advdata/**

The viewer is entirely static. `labsweep export` writes the current catalog and lazy detail shards; `scripts/export_pages_meta.py` writes compact version-history shards and freshness metadata. The Pages workflow downloads the latest published database, rebuilds the export, and deploys it.

Viewer features include the worldwide MapLibre map (clusters, heatmap, or all points), search/filter/sort, per-lab stages/reviews/answers, version-history diffs, a global recent-changes feed, permalinks, local favorites, distance sorting, CSV export, and JSON download.

The generated `web/data/` directory is intentionally not committed.
