/* labsweep static viewer — current catalog + lazy detail/history shards.
   Everything is static so GitHub Pages can serve the full dataset without a
   backend. Data is rebuilt from db-latest after every successful daily run. */
"use strict";

const esc = s => String(s ?? "").replace(/[&<>"']/g,
  c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

const san = h => {
  if (typeof DOMPurify === "undefined") return esc(h);
  const out = DOMPurify.sanitize(h ?? "", { USE_PROFILES: { html: true } });
  const t = document.createElement("div");
  t.innerHTML = out;
  t.querySelectorAll("a").forEach(a => { a.target = "_blank"; a.rel = "noopener"; });
  return t.innerHTML;
};

async function fetchJson(url, fallback = null) {
  try {
    const r = await fetch(url, { cache: "no-cache" });
    if (!r.ok) throw new Error(`${url}: ${r.status}`);
    return await r.json();
  } catch (e) {
    if (fallback !== null) return fallback;
    throw e;
  }
}

async function fetchGz(url, fallback = null) {
  try {
    const r = await fetch(url, { cache: "no-cache" });
    if (!r.ok) throw new Error(`${url}: ${r.status}`);
    if (!("DecompressionStream" in window))
      throw new Error("this browser does not support gzip DecompressionStream");
    const ds = r.body.pipeThrough(new DecompressionStream("gzip"));
    return new Response(ds).json();
  } catch (e) {
    if (fallback !== null) return fallback;
    throw e;
  }
}

const $ = id => document.getElementById(id);
const setLoad = (pct, msg) => {
  $("loadbar").firstElementChild.style.width = pct + "%";
  if (msg) $("loadmsg").textContent = msg;
};
const fmt = n => Number(n || 0).toLocaleString();
const dateOnly = s => (s || "").slice(0, 10);
const when = s => {
  if (!s) return "unknown";
  const d = new Date(s.endsWith?.("Z") ? s : s.replace(" ", "T") + "Z");
  if (Number.isNaN(d.getTime())) return s;
  return new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" }).format(d);
};
const age = s => {
  if (!s) return "unknown";
  const d = new Date(s);
  if (Number.isNaN(d.getTime())) return "unknown";
  let sec = Math.max(0, Math.round((Date.now() - d.getTime()) / 1000));
  if (sec < 60) return `${sec}s ago`;
  let min = Math.round(sec / 60); if (min < 60) return `${min}m ago`;
  let hr = Math.round(min / 60); if (hr < 48) return `${hr}h ago`;
  return `${Math.round(hr / 24)}d ago`;
};

let CAT = [];
let META = {};
let RECENT = [];
let filtered = [];
const shards = new Map();
const historyShards = new Map();
const favorites = new Set(JSON.parse(localStorage.getItem("labsweep:favorites") || "[]"));
const state = { q: "", sort: "ra", sel: null, userLoc: null, drawer: null };

function saveFavorites() {
  localStorage.setItem("labsweep:favorites", JSON.stringify([...favorites]));
}
function isFav(g) { return favorites.has(g); }
function toggleFav(g) {
  if (favorites.has(g)) favorites.delete(g); else favorites.add(g);
  saveFavorites();
  updateFavButton();
  renderList();
  if ($("ffav").checked) applyFilters();
}
function updateFavButton() {
  const b = $("dfav");
  if (!b || !state.sel) return;
  const on = isFav(state.sel);
  b.textContent = on ? "★" : "☆";
  b.title = on ? "remove favorite" : "save favorite";
  b.classList.toggle("on", on);
}

async function boot() {
  setLoad(4, "fetching metadata");
  const [meta, cat] = await Promise.all([
    fetchJson("data/meta.json", {}),
    fetchGz("data/catalog.json.gz"),
  ]);
  META = meta || {};
  CAT = cat;
  setLoad(35, `parsing ${CAT.length.toLocaleString()} labs`);
  await new Promise(r => setTimeout(r));

  RECENT = await fetchGz("data/history/recent.json.gz", []);
  setLoad(48, "building map");
  initMap();
  bindUi();
  applyFilters();
  renderStats();
  renderFreshness();

  const params = new URLSearchParams(location.search);
  const initial = params.get("lab");
  if (initial) {
    const x = CAT.find(v => v.g === initial);
    if (x) setTimeout(() => openDetail(x.g, x.la, x.lo, false), 100);
  }

  setLoad(100, "ready");
  $("load").style.display = "none";
}

function renderStats() {
  const nFetched = CAT.filter(x => x.f).length;
  const nRev = CAT.reduce((a, x) => a + (x.vc || 0), 0);
  const h = META.counts?.historyEvents || 0;
  $("stats").innerHTML =
    `${fmt(CAT.length)} labs · ${fmt(nFetched)} full · ${fmt(nRev)} reviews` +
    (h ? ` · ${fmt(h)} changes` : "");
}

function renderFreshness() {
  const el = $("fresh");
  const at = META.generatedAt;
  if (!at) {
    el.textContent = "snapshot";
    el.title = "build metadata unavailable";
    return;
  }
  el.textContent = `updated ${age(at)}`;
  el.title =
    `Site export: ${when(at)}\n` +
    `Database release: ${when(META.source?.dbPublishedAt)}\n` +
    `Latest adventure fetch: ${when(META.lastSeen?.adventureFetch)}\n` +
    `Latest history change: ${when(META.lastSeen?.historyChange)}`;
  const hours = (Date.now() - new Date(at).getTime()) / 3600000;
  el.classList.toggle("stale", hours > 36);
}

function passFilters(x) {
  if (state.q) {
    const q = state.q;
    if (!((x.t || "").toLowerCase().includes(q) ||
          (x.o || "").toLowerCase().includes(q) ||
          (x.g || "").toLowerCase().includes(q))) return false;
  }
  if ($("ftype").value && x.ty !== $("ftype").value) return false;
  if (+$("fminr").value && !(x.ra >= +$("fminr").value)) return false;
  if (+$("fminst").value && !(x.sc >= +$("fminst").value)) return false;
  if (+$("fminrv").value && !(x.vc >= +$("fminrv").value)) return false;
  if ($("fhr").checked && !x.hr) return false;
  if ($("ffetch").checked && !x.f) return false;
  if ($("factive").checked && x.arch) return false;
  if ($("ffav").checked && !isFav(x.g)) return false;
  return true;
}

function applyFilters() {
  filtered = CAT.filter(passFilters);
  const k = state.sort;
  if (k === "dist") {
    filtered.sort((a, b) => (a._dist ?? Infinity) - (b._dist ?? Infinity));
  } else if (k === "p") {
    filtered.sort((a, b) => String(b.p || "").localeCompare(String(a.p || "")));
  } else {
    filtered.sort((a, b) => (b[k] ?? -Infinity) - (a[k] ?? -Infinity));
  }
  $("count").textContent = `${fmt(filtered.length)} shown`;
  renderList();
  pushMapData();
  setFlatFilter();
}

function resetFilters() {
  $("search").value = "";
  state.q = "";
  $("ftype").value = "";
  $("fminr").value = "0";
  $("fminst").value = "0";
  $("fminrv").value = "0";
  $("fhr").checked = false;
  $("ffetch").checked = false;
  $("factive").checked = true;
  $("ffav").checked = false;
  applyFilters();
}

let deb;
function bindUi() {
  $("search").addEventListener("input", e => {
    state.q = e.target.value.trim().toLowerCase();
    clearTimeout(deb); deb = setTimeout(applyFilters, 120);
  });
  for (const id of ["ftype", "fminr", "fminst", "fminrv", "fhr", "ffetch", "factive", "ffav"])
    $(id).addEventListener("change", applyFilters);

  document.querySelectorAll(".sortbtn").forEach(b => b.addEventListener("click", () => {
    if (b.disabled) return;
    document.querySelectorAll(".sortbtn").forEach(x => x.classList.remove("on"));
    b.classList.add("on");
    state.sort = b.dataset.k;
    applyFilters();
  }));
  document.querySelector('.sortbtn[data-k="ra"]').classList.add("on");

  $("dclose").onclick = closeDrawer;
  $("dfav").onclick = () => state.sel && toggleFav(state.sel);
  $("dshare").onclick = shareSelected;
  $("recent").onclick = showRecentChanges;
  $("near").onclick = locateMe;
  $("exportcsv").onclick = exportCsv;
  $("reset").onclick = resetFilters;
  $("list").addEventListener("scroll", renderList);
  window.addEventListener("resize", renderList);
  document.querySelectorAll("#mapmode button").forEach(b =>
    b.onclick = () => setMode(b.dataset.m));

  document.addEventListener("keydown", e => {
    if (e.key === "/" && document.activeElement !== $("search")) {
      e.preventDefault(); $("search").focus();
    } else if (e.key === "Escape") {
      closeDrawer();
    }
  });
  window.addEventListener("popstate", () => {
    const g = new URLSearchParams(location.search).get("lab");
    if (!g) return closeDrawer(false);
    const x = CAT.find(v => v.g === g);
    if (x) openDetail(x.g, x.la, x.lo, false);
  });
}

const ROWH = 52;
function renderList() {
  const list = $("list"), spacer = $("spacer");
  const st = list.scrollTop, vh = list.clientHeight;
  const i0 = Math.max(0, Math.floor(st / ROWH) - 4);
  const i1 = Math.min(filtered.length, Math.ceil((st + vh) / ROWH) + 4);
  spacer.style.height = filtered.length * ROWH + "px";
  list.querySelectorAll(".row").forEach(n => n.remove());
  const frag = document.createDocumentFragment();

  for (let i = i0; i < i1; i++) {
    const x = filtered[i];
    const d = document.createElement("div");
    d.className = "row" + (x.f ? "" : " unf");
    d.style.top = i * ROWH + "px";
    const rating = x.ra ? `<b>${x.ra.toFixed(1)}★</b>` : "";
    const revs = x.vc ? ` · ${x.vc} rev` : "";
    const hr = x.hr ? ' · <span class="hr">★rec</span>' : "";
    const ac = x.ac ? ` · <span class="ac">${x.ac}✓</span>` : "";
    const dist = state.userLoc && Number.isFinite(x._dist) ? ` · ${formatDistance(x._dist)}` : "";
    d.innerHTML =
      `<div class="t"><span class="favmark">${isFav(x.g) ? "★" : ""}</span>${esc(x.t || "(untitled)")}</div>` +
      `<div class="s">${rating}${x.rc ? ` (${x.rc})` : ""} · ${esc(x.ty || "?")} · ` +
      `${x.sc ?? "?"} stg${revs}${hr}${ac}${dist} · ${esc(x.o || "")}</div>`;
    d.onclick = () => openDetail(x.g, x.la, x.lo);
    frag.appendChild(d);
  }
  list.appendChild(frag);
}

function haversine(a, b, c, d) {
  const R = 6371;
  const p = Math.PI / 180;
  const dlat = (c - a) * p, dlon = (d - b) * p;
  const q = Math.sin(dlat / 2) ** 2 +
    Math.cos(a * p) * Math.cos(c * p) * Math.sin(dlon / 2) ** 2;
  return R * 2 * Math.atan2(Math.sqrt(q), Math.sqrt(1 - q));
}
function formatDistance(km) {
  return km < 10 ? `${km.toFixed(1)} km` : `${Math.round(km)} km`;
}
function locateMe() {
  const b = $("near");
  if (!navigator.geolocation) {
    b.textContent = "location unavailable";
    return;
  }
  b.disabled = true; b.textContent = "locating…";
  navigator.geolocation.getCurrentPosition(pos => {
    state.userLoc = [pos.coords.latitude, pos.coords.longitude];
    for (const x of CAT) {
      if (x.la != null && x.lo != null)
        x._dist = haversine(state.userLoc[0], state.userLoc[1], x.la, x.lo);
    }
    const sort = document.querySelector('.sortbtn[data-k="dist"]');
    sort.disabled = false;
    b.disabled = false; b.textContent = "near me ✓";
    sort.click();
    map?.flyTo({ center: [state.userLoc[1], state.userLoc[0]], zoom: Math.max(map.getZoom(), 7) });
  }, err => {
    b.disabled = false; b.textContent = "near me";
    alert("Could not get location: " + err.message);
  }, { enableHighAccuracy: false, timeout: 10000 });
}

function exportCsv() {
  const cols = ["guid", "title", "owner", "type", "rating", "ratings", "reviews", "stages",
    "completions", "published", "latitude", "longitude", "archived", "fetched"];
  const quote = v => '"' + String(v ?? "").replaceAll('"', '""') + '"';
  const lines = [cols.join(",")];
  for (const x of filtered) lines.push([
    x.g, x.t, x.o, x.ty, x.ra, x.rc, x.vc, x.sc, x.cc, x.p, x.la, x.lo, !!x.arch, !!x.f
  ].map(quote).join(","));
  const blob = new Blob([lines.join("\n") + "\n"], { type: "text/csv" });
  const a = document.createElement("a");
  a.href = URL.createObjectURL(blob);
  a.download = `labsweep-${new Date().toISOString().slice(0, 10)}.csv`;
  a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 1000);
}

let map;
function colorExpr() {
  return ["case",
    ["!", ["get", "f"]], "#4a5568",
    ["!", ["has", "ra"]], "#58a6ff",
    ["interpolate", ["linear"], ["get", "ra"],
      1, "#f85149", 3, "#d29922", 4.5, "#3fb950", 5, "#2ea043"]];
}
function initMap() {
  map = new maplibregl.Map({
    container: "map",
    style: {
      version: 8,
      sources: {
        osm: { type: "raster", tileSize: 256,
          tiles: ["https://tile.openstreetmap.org/{z}/{x}/{y}.png"],
          attribution: "© OpenStreetMap contributors" }
      },
      layers: [{ id: "osm", type: "raster", source: "osm" }]
    },
    center: [10, 30], zoom: 1.6
  });
  map.addControl(new maplibregl.NavigationControl({ showCompass: false }), "bottom-right");
  map.on("load", () => {
    map.addSource("labs", {
      type: "geojson", data: { type: "FeatureCollection", features: [] },
      cluster: true, clusterMaxZoom: 11, clusterRadius: 45
    });
    map.addLayer({ id: "clust", type: "circle", source: "labs",
      filter: ["has", "point_count"],
      paint: { "circle-color": "#1c3a5e", "circle-radius":
        ["step", ["get", "point_count"], 12, 100, 16, 1000, 22, 10000, 30],
        "circle-stroke-width": 1.5, "circle-stroke-color": "#58a6ff" } });
    map.addLayer({ id: "clust-n", type: "symbol", source: "labs",
      filter: ["has", "point_count"],
      layout: { "text-field": "{point_count_abbreviated}", "text-size": 11 },
      paint: { "text-color": "#e6edf3" } });
    map.addLayer({ id: "pts", type: "circle", source: "labs",
      filter: ["!", ["has", "point_count"]],
      paint: { "circle-color": colorExpr(), "circle-radius":
        ["interpolate", ["linear"], ["zoom"], 4, 3, 10, 5],
        "circle-stroke-width": .8, "circle-stroke-color": "#0d1117" } });

    map.on("click", "clust", e => {
      const f = map.queryRenderedFeatures(e.point, { layers: ["clust"] })[0];
      map.getSource("labs").getClusterExpansionZoom(
        f.properties.cluster_id, (err, z) =>
          !err && map.easeTo({ center: f.geometry.coordinates, zoom: z + .5 }));
    });
    map.on("click", "pts", e => {
      const p = e.features[0].properties;
      openDetail(p.g, ...e.features[0].geometry.coordinates.slice().reverse());
    });
    for (const l of ["clust", "pts"]) {
      map.on("mouseenter", l, () => map.getCanvas().style.cursor = "pointer");
      map.on("mouseleave", l, () => map.getCanvas().style.cursor = "");
    }

    map.addSource("labs-flat", {
      type: "geojson", data: { type: "FeatureCollection", features: [] },
      maxzoom: 12, buffer: 512
    });
    map.addLayer({ id: "heat", type: "heatmap", source: "labs-flat",
      layout: { visibility: "none" },
      paint: {
        "heatmap-intensity": ["interpolate", ["linear"], ["zoom"], 0, 1, 10, 4],
        "heatmap-radius": ["interpolate", ["linear"], ["zoom"], 0, 2, 5, 6, 10, 14, 13, 24],
        "heatmap-color": ["interpolate", ["linear"], ["heatmap-density"],
          0, "rgba(28,58,94,0)", .15, "rgba(28,58,94,.55)", .35, "#58a6ff",
          .55, "#3fb950", .75, "#d29922", 1, "#f85149"],
        "heatmap-opacity": .85 } });
    map.addLayer({ id: "pts-all", type: "circle", source: "labs-flat",
      layout: { visibility: "none" },
      paint: { "circle-color": colorExpr(),
        "circle-radius": ["interpolate", ["linear"], ["zoom"], 1, 1.3, 4, 2.2, 8, 3.5, 12, 5.5],
        "circle-opacity": .8, "circle-stroke-width": 0 } });
    map.on("click", "pts-all", e => {
      const f = e.features[0];
      openDetail(f.properties.g, ...f.geometry.coordinates.slice().reverse());
    });
    map.on("mouseenter", "pts-all", () => map.getCanvas().style.cursor = "pointer");
    map.on("mouseleave", "pts-all", () => map.getCanvas().style.cursor = "");

    pushMapData();
    setTimeout(loadFlat, 60);
  });
}

let flatReady = false;
function loadFlat() {
  const feats = [];
  for (const x of CAT) {
    if (x.la == null) continue;
    const p = { g: x.g, f: !!x.f, ty: x.ty || "", hr: !!x.hr, arch: !!x.arch,
      tl: (x.t || "").toLowerCase(), ol: (x.o || "").toLowerCase() };
    if (x.ra != null) p.ra = x.ra;
    if (x.sc != null) p.sc = x.sc;
    if (x.vc != null) p.vc = x.vc;
    feats.push({ type: "Feature",
      geometry: { type: "Point", coordinates: [x.lo, x.la] }, properties: p });
  }
  map.getSource("labs-flat").setData({ type: "FeatureCollection", features: feats });
  flatReady = true;
  setFlatFilter();
}

function mapFilterExpr() {
  const all = ["all"];
  if (state.q) {
    const q = ["literal", state.q];
    all.push(["any",
      ["in", q, ["coalesce", ["get", "tl"], ""]],
      ["in", q, ["coalesce", ["get", "ol"], ""]],
      ["in", q, ["coalesce", ["get", "g"], ""]]]);
  }
  const ty = $("ftype").value;   if (ty) all.push(["==", ["get", "ty"], ty]);
  const mr = +$("fminr").value;  if (mr) all.push([">=", ["coalesce", ["get", "ra"], -1], mr]);
  const ms = +$("fminst").value; if (ms) all.push([">=", ["coalesce", ["get", "sc"], -1], ms]);
  const mv = +$("fminrv").value; if (mv) all.push([">=", ["coalesce", ["get", "vc"], -1], mv]);
  if ($("fhr").checked) all.push(["==", ["coalesce", ["get", "hr"], false], true]);
  if ($("ffetch").checked) all.push(["==", ["coalesce", ["get", "f"], false], true]);
  if ($("factive").checked) all.push(["!=", ["coalesce", ["get", "arch"], false], true]);
  if ($("ffav").checked) {
    const favs = [...favorites];
    all.push(favs.length ? ["in", ["get", "g"], ["literal", favs]] : ["==", 1, 0]);
  }
  return all;
}
function setFlatFilter() {
  if (!map || !flatReady) return;
  const f = mapFilterExpr();
  map.setFilter("heat", f);
  map.setFilter("pts-all", f);
}
function setMode(m) {
  document.querySelectorAll("#mapmode button").forEach(b =>
    b.classList.toggle("on", b.dataset.m === m));
  if (!map || !map.getLayer("heat")) return;
  const v = (id, on) => map.setLayoutProperty(id, "visibility", on ? "visible" : "none");
  v("clust", m === "cluster"); v("clust-n", m === "cluster"); v("pts", m === "cluster");
  v("heat", m === "heat"); v("pts-all", m === "all");
}
function pushMapData() {
  if (!map || !map.getSource("labs")) return;
  const feats = [];
  for (const x of filtered) {
    if (x.la == null) continue;
    feats.push({ type: "Feature",
      geometry: { type: "Point", coordinates: [x.lo, x.la] },
      properties: { g: x.g, ra: x.ra ?? 0, f: !!x.f } });
  }
  map.getSource("labs").setData({ type: "FeatureCollection", features: feats });
}

async function getDetail(g) {
  const key = g.slice(0, 2).toLowerCase();
  if (!shards.has(key))
    shards.set(key, await fetchGz(`data/detail/${key}.json.gz`, {}));
  return shards.get(key)[g] || null;
}
async function getHistory(g) {
  const key = g.slice(0, 2).toLowerCase();
  if (!historyShards.has(key))
    historyShards.set(key, await fetchGz(`data/history/${key}.json.gz`, {}));
  return historyShards.get(key)[g] || [];
}

function setLabUrl(g, push = true) {
  const u = new URL(location.href);
  if (g) u.searchParams.set("lab", g); else u.searchParams.delete("lab");
  history[push ? "pushState" : "replaceState"]({}, "", u);
}

async function openDetail(g, la, lo, push = true) {
  state.sel = g;
  state.drawer = "detail";
  $("drawer").classList.add("open");
  $("dhead-actions").style.display = "flex";
  $("dtitle").textContent = "loading…";
  $("dbody").innerHTML = "";
  updateFavButton();
  if (push) setLabUrl(g, true);
  if (la != null && map) map.flyTo({
    center: [lo, la], zoom: Math.max(map.getZoom(), 10),
    padding: { right: $("drawer").offsetWidth + 40, left: 40, top: 40, bottom: 40 },
  });

  const d = await getDetail(g);
  const cat = CAT.find(x => x.g === g) || {};
  if (state.sel !== g) return;
  if (!d) {
    $("dtitle").textContent = cat.t || g;
    $("dbody").innerHTML = `<div class="dsec">Discovery data only — detail not fetched yet.<br>
      <span class="chip">${esc(g)}</span></div>`;
    await appendHistory(g);
    return;
  }
  renderDetail(d, g);
  await appendHistory(g);
}

function closeDrawer(updateUrl = true) {
  $("drawer").classList.remove("open");
  state.drawer = null;
  state.sel = null;
  if (updateUrl) setLabUrl(null, false);
}

function shareSelected() {
  if (!state.sel) return;
  const u = new URL(location.href);
  u.searchParams.set("lab", state.sel);
  navigator.clipboard?.writeText(u.toString()).then(() => {
    const b = $("dshare"), old = b.textContent;
    b.textContent = "copied";
    setTimeout(() => b.textContent = old, 1000);
  }).catch(() => prompt("Permalink", u.toString()));
}

function kv(k, v) {
  return v == null || v === "" ? "" : `<div><span class="k">${k}</span>${esc(v)}</div>`;
}
const stars = n => n ? "★".repeat(Math.round(n)) + "☆".repeat(5 - Math.round(n)) : "—";

function renderDetail(d, guid) {
  $("dtitle").textContent = d.title || "(untitled)";
  const st = d.stageSummaries || [];
  const rv = d.reviews || [];
  const img = d.keyImageUrl ? `<img id="dimg" src="${esc(d.keyImageUrl)}" loading="lazy">` : "";
  const themes = (d.adventureThemes || []).map(t =>
    `<span class="chip">${esc(t.name || t.title || t)}</span>`).join("");
  $("dbody").innerHTML = img + `
  <div class="dsec"><div class="kv">
    ${kv("type", d.adventureType)}
    ${kv("rating", d.ratingsAverage ? d.ratingsAverage.toFixed(2) + "★ (" + (d.ratingsTotalCount || 0) + ")" : null)}
    ${kv("reviews", d.reviewsTotalCount)}
    ${kv("stages", st.length || d.stagesTotalCount)}
    ${kv("median time", d.medianTimeToComplete ? d.medianTimeToComplete + " min" : null)}
    ${kv("completions", d.completionCount)}
    ${kv("recommended", d.recommendedCount)}
    ${kv("owner", d.ownerUsername)}
    ${kv("published", dateOnly(d.publishedUtc))}
    ${kv("visibility", d.visibility)}
    ${kv("coords", d.location ? d.location.latitude.toFixed(5) + ", " + d.location.longitude.toFixed(5) : null)}
    ${kv("guid", d.adventureGuid || guid)}
  </div>${themes}
  <div class="detail-actions">
    <button onclick="copyGuid('${esc(guid)}')">copy guid</button>
    <button onclick="downloadJson('${esc(guid)}')">download json</button>
  </div></div>
  ${d.description ? `<div class="dsec"><h3>description</h3><div id="ddesc">${san(d.description)}</div></div>` : ""}
  <div class="dsec"><h3>stages (${st.length})</h3>${st.map((s, i) => renderStage(s, i, d.answers?.[i])).join("") || "<i>none</i>"}</div>
  <div class="dsec"><h3>reviews (${rv.length})</h3>${rv.map(renderReview).join("") || "<i>none</i>"}</div>
  <div class="dsec" id="historysec"><h3>history</h3><div class="muted">loading…</div></div>`;
}

window.copyGuid = async g => {
  try { await navigator.clipboard.writeText(g); } catch { prompt("GUID", g); }
};
window.downloadJson = async g => {
  const d = await getDetail(g);
  if (!d) return;
  const blob = new Blob([JSON.stringify(d, null, 2) + "\n"], { type: "application/json" });
  const a = document.createElement("a");
  a.href = URL.createObjectURL(blob);
  a.download = `${g}.json`;
  a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 1000);
};

function renderStage(s, i, answers) {
  const L = s.location;
  const hashes =
    (s.findCodeHashBase16v2 ? `<br>find <code>${esc(s.findCodeHashBase16v2)}</code>` : "") +
    ((s.answerCodeHashesBase16v2 || []).map(h => `<br>ans <code>${esc(h)}</code>`).join(""));
  const ans = (answers || []).map(a =>
    `<div class="ans">A: <b>${esc(a.d || a.a)}</b> <span class="chip">${esc(a.m)}</span></div>`
  ).join("");
  return `<div class="stage">
    <div class="st">${i + 1}. ${esc(s.title || "(untitled)")}
      <span class="chip">${esc(s.challengeType || "?")}</span></div>
    ${ans}
    ${s.question ? `<div class="sq"><span class="q">Q:</span> ${san(s.question)}</div>` : ""}
    ${s.description ? `<div class="sq sd">${san(s.description)}</div>` : ""}
    <div class="sm">${L ? `📍 ${L.latitude.toFixed(5)}, ${L.longitude.toFixed(5)}` : ""}
      ${s.geofencingRadius ? ` · r${s.geofencingRadius}m` : ""}${hashes}</div>
  </div>`;
}

function renderReview(r) {
  const imgs = (r.images || []).map(im =>
    `<a href="${esc(im.url || im.imageUrl || "#")}" target="_blank" rel="noopener"><img src="${esc(im.url || im.imageUrl)}" loading="lazy"></a>`).join("");
  const tags = (r.playerTags || []).map(t => `<span class="chip">${esc(t.name || t)}</span>`).join("");
  return `<div class="rev">
    <div class="rh"><span class="stars">${stars(r.rating)}</span>
      <b>${esc(r.playerUsername || "?")}</b>
      · ${dateOnly(r.createdUtc)}
      ${r.isCreator ? " · <span class='chip'>creator</span>" : ""}</div>
    ${r.reviewText ? `<div class="rt">${esc(r.reviewText)}</div>` : ""}
    ${imgs}${tags}</div>`;
}

function diffValue(v) {
  if (v == null || v === "") return "∅";
  if (typeof v === "boolean") return v ? "true" : "false";
  return String(v);
}
function renderHistoryEvent(e) {
  const diffEntries = Object.entries(e.diff || {});
  const changes = diffEntries.slice(0, 8).map(([k, pair]) =>
    `<div class="histdiff"><b>${esc(k)}</b><span>${esc(diffValue(pair[0]))}</span><i>→</i><span>${esc(diffValue(pair[1]))}</span></div>`
  ).join("");
  const more = diffEntries.length > 8 ? `<div class="muted">+${diffEntries.length - 8} more fields</div>` : "";
  const target = e.kind === "stage" ? `stage ${Number(e.index) + 1}` :
    e.kind === "review" ? `review #${e.id}` : "adventure";
  return `<div class="hist">
    <div class="histhead"><b>${esc(e.change || "changed")}</b> · ${esc(target)}
      <span>v${esc(e.v)} · ${esc(when(e.at))}</span></div>
    ${changes || '<div class="muted">status/history event; content snapshot unchanged</div>'}${more}
  </div>`;
}
async function appendHistory(g) {
  const sec = $("historysec") || (() => {
    const x = document.createElement("div");
    x.id = "historysec"; x.className = "dsec";
    x.innerHTML = "<h3>history</h3>";
    $("dbody").appendChild(x);
    return x;
  })();
  const events = await getHistory(g);
  if (state.sel !== g) return;
  sec.innerHTML = `<h3>history (${events.length})</h3>` +
    (events.length ? events.map(renderHistoryEvent).join("") :
      '<div class="muted">No recorded changes yet. The current row is version 1 or predates version tracking.</div>');
}

function showRecentChanges() {
  state.drawer = "recent";
  state.sel = null;
  $("drawer").classList.add("open");
  $("dhead-actions").style.display = "none";
  $("dtitle").textContent = "recent changes";
  setLabUrl(null, false);
  const c = META.counts || {};
  const summary = `<div class="dsec"><div class="kv">
    ${kv("history events", fmt(c.historyEvents))}
    ${kv("labs changed", fmt(c.historyLabs))}
    ${kv("adventure versions", fmt(c.adventureVersions))}
    ${kv("stage versions", fmt(c.stageVersions))}
    ${kv("review versions", fmt(c.reviewVersions))}
    ${kv("latest change", when(META.lastSeen?.historyChange))}
  </div></div>`;
  const rows = RECENT.map(e => {
    const target = e.kind === "stage" ? `stage ${Number(e.index) + 1}` :
      e.kind === "review" ? "review" : "adventure";
    return `<button class="change-row" data-guid="${esc(e.g)}">
      <span><b>${esc(e.t || e.g)}</b><small>${esc(e.change)} · ${esc(target)}</small></span>
      <time>${esc(when(e.at))}</time>
    </button>`;
  }).join("");
  $("dbody").innerHTML = summary + `<div class="dsec"><h3>latest ${RECENT.length} events</h3>
    <div class="change-list">${rows || '<div class="muted">No version history has been recorded yet.</div>'}</div></div>`;
  $("dbody").querySelectorAll(".change-row").forEach(b => b.onclick = () => {
    const x = CAT.find(v => v.g === b.dataset.guid);
    if (x) openDetail(x.g, x.la, x.lo);
  });
}

boot().catch(e => {
  console.error(e);
  setLoad(100, "FAILED: " + e.message);
});
