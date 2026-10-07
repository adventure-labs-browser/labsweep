/* labsweep static viewer — catalog + lazy detail shards, no server.
   Catalog record keys (from `labsweep export`):
   g guid | t title | la lo coords | ty type | ra rating avg | rc ratings count
   vc reviews count | sc stages | cc completions | p publishedUtc | o owner
   img keyImageUrl | vis visibility | mt median mins | th themes
   hr highlyRecommended | arch archived | test | f fetched-detail */
"use strict";

const esc = s => String(s ?? "").replace(/[&<>"']/g,
  c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

/* Sanitize API-provided HTML (descriptions can contain markup).
   Falls back to plain escaped text if the CDN lib didn't load. */
const san = h => {
  if (typeof DOMPurify === "undefined") return esc(h);
  const out = DOMPurify.sanitize(h ?? "", { USE_PROFILES: { html: true } });
  const t = document.createElement("div");
  t.innerHTML = out;
  t.querySelectorAll("a").forEach(a => { a.target = "_blank"; a.rel = "noopener"; });
  return t.innerHTML;
};

async function fetchGz(url) {
  const r = await fetch(url);
  if (!r.ok) throw new Error(`${url}: ${r.status}`);
  const ds = r.body.pipeThrough(new DecompressionStream("gzip"));
  return new Response(ds).json();
}

const $ = id => document.getElementById(id);
const setLoad = (pct, msg) => {
  $("loadbar").firstElementChild.style.width = pct + "%";
  if (msg) $("loadmsg").textContent = msg;
};

let CAT = [];                 // all catalog records
let filtered = [];            // current filter+sort result
let META = {};
let CHANGES = [];
const shards = new Map();     // "xx" -> {guid: detail}
const historyShards = new Map();
const state = { q: "", sort: "ra", sel: null };

/* ── boot ── */
async function boot() {
  setLoad(5, "fetching dataset");
  const [cat, meta, changes] = await Promise.all([
    fetchGz("data/catalog.json.gz"),
    fetchGz("data/meta.json.gz").catch(() => ({})),
    fetchGz("data/changes.json.gz").catch(() => []),
  ]);
  CAT = cat;
  META = meta;
  CHANGES = changes;
  setLoad(40, `parsing ${CAT.length} labs`);
  await new Promise(r => setTimeout(r));           // let UI paint
  initMap();
  bindUi();
  applyFilters();
  const nFetched = CAT.filter(x => x.f).length;
  const nRev = CAT.reduce((a, x) => a + (x.vc || 0), 0);
  $("stats").innerHTML =
    `${CAT.length.toLocaleString()} labs · ${nFetched.toLocaleString()} full · ` +
    `${nRev.toLocaleString()} reviews`;
  if (META.generatedUnix) {
    const d = new Date(META.generatedUnix * 1000);
    $("fresh").textContent =
      `updated ${d.toLocaleString()} · ${(META.historyEvents || 0).toLocaleString()} history events`;
  }
  setLoad(100, "ready");
  $("load").style.display = "none";

  const deep = new URLSearchParams(location.search).get("g");
  if (deep) {
    const x = CAT.find(v => v.g.toLowerCase() === deep.toLowerCase());
    if (x) setTimeout(() => openDetail(x.g, x.la, x.lo), 150);
  }
}

/* ── filters / sort ── */
function passFilters(x) {
  if (state.q) {
    const q = state.q;
    if (!((x.t || "").toLowerCase().includes(q) ||
          (x.o || "").toLowerCase().includes(q) ||
          x.g.includes(q))) return false;
  }
  if ($("ftype").value && x.ty !== $("ftype").value) return false;
  if (+$("fminr").value && !(x.ra >= +$("fminr").value)) return false;
  if (+$("fminst").value && !(x.sc >= +$("fminst").value)) return false;
  if (+$("fminrv").value && !(x.vc >= +$("fminrv").value)) return false;
  if ($("fhr").checked && !x.hr) return false;
  if ($("ffetch").checked && !x.f) return false;
  if ($("factive").checked && x.arch) return false;
  return true;
}

function applyFilters() {
  filtered = CAT.filter(passFilters);
  const k = state.sort;
  filtered.sort((a, b) => (b[k] ?? -Infinity) - (a[k] ?? -Infinity));
  $("count").textContent = `${filtered.length.toLocaleString()} shown`;
  renderList();
  pushMapData();
  setFlatFilter();
}

let deb;
function bindUi() {
  $("search").addEventListener("input", e => {
    state.q = e.target.value.trim().toLowerCase();
    clearTimeout(deb); deb = setTimeout(applyFilters, 120);
  });
  for (const id of ["ftype", "fminr", "fminst", "fminrv", "fhr", "ffetch", "factive"])
    $(id).addEventListener("change", applyFilters);
  document.querySelectorAll(".sortbtn").forEach(b => b.addEventListener("click", () => {
    document.querySelectorAll(".sortbtn").forEach(x => x.classList.remove("on"));
    b.classList.add("on");
    state.sort = b.dataset.k;
    applyFilters();
  }));
  document.querySelector('.sortbtn[data-k="ra"]').classList.add("on");
  $("dclose").onclick = () => {
    $("drawer").classList.remove("open");
    state.sel = null;
    $("dshare").style.display = "";
    const u = new URL(location.href);
    u.searchParams.delete("g");
    history.replaceState(null, "", u);
  };
  $("dshare").onclick = async () => {
    if (!state.sel) return;
    const u = new URL(location.href);
    u.searchParams.set("g", state.sel);
    await navigator.clipboard.writeText(u.toString());
    $("dshare").textContent = "copied";
    setTimeout(() => $("dshare").textContent = "copy link", 900);
  };
  $("near").onclick = locateMe;
  $("changes").onclick = showChanges;
  $("random").onclick = randomLab;
  $("fit").onclick = fitVisible;
  $("csv").onclick = exportCsv;
  document.addEventListener("keydown", e => {
    const tag = document.activeElement?.tagName || "";
    if (e.key === "/" && tag !== "INPUT") { e.preventDefault(); $("search").focus(); }
    if (e.key.toLowerCase() === "r" && !/INPUT|SELECT|TEXTAREA/.test(tag)) randomLab();
  });
  $("list").addEventListener("scroll", renderList);
  window.addEventListener("resize", renderList);
  window.addEventListener("popstate", () => {
    const g = new URLSearchParams(location.search).get("g");
    if (!g) $("drawer").classList.remove("open");
  });
  document.querySelectorAll("#mapmode button").forEach(b =>
    b.onclick = () => setMode(b.dataset.m));
}

function randomLab() {
  if (!filtered.length) return;
  const x = filtered[Math.floor(Math.random() * filtered.length)];
  openDetail(x.g, x.la, x.lo);
}

function fitVisible() {
  if (!map) return;
  const pts = filtered.filter(x => x.la != null && x.lo != null);
  if (!pts.length) return;
  const b = new maplibregl.LngLatBounds();
  pts.forEach(x => b.extend([x.lo, x.la]));
  map.fitBounds(b, { padding: 50, maxZoom: 11, duration: 500 });
}

function locateMe() {
  if (!navigator.geolocation) return;
  $("near").textContent = "locating…";
  navigator.geolocation.getCurrentPosition(
    p => {
      if (map) map.flyTo({ center: [p.coords.longitude, p.coords.latitude], zoom: 10 });
      $("near").textContent = "near me";
    },
    () => {
      $("near").textContent = "location blocked";
      setTimeout(() => $("near").textContent = "near me", 1400);
    },
    { enableHighAccuracy: false, timeout: 8000 }
  );
}

function exportCsv() {
  const cols = ["guid","title","owner","type","rating","reviews","stages","completions","published","lat","lon"];
  const q = v => '"' + String(v ?? "").replaceAll('"', '""') + '"';
  const rows = filtered.map(x =>
    [x.g,x.t,x.o,x.ty,x.ra,x.vc,x.sc,x.cc,x.p,x.la,x.lo].map(q).join(","));
  const blob = new Blob([[cols.join(","), ...rows].join("\n")], { type: "text/csv" });
  const a = document.createElement("a");
  a.href = URL.createObjectURL(blob);
  a.download = "labsweep-filtered.csv";
  a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 1000);
}

function showChanges() {
  state.sel = null;
  $("drawer").classList.add("open");
  $("dtitle").textContent = "recent changes";
  $("dshare").style.display = "none";
  const rows = CHANGES.slice(0, 500).map(c => {
    const x = CAT.find(v => v.g === c.g);
    const label = x?.t || c.g;
    const entity = c.e === "stage" ? `stage #${(c.i ?? 0) + 1}` :
      c.e === "review" ? `review #${c.i}` : "adventure";
    return `<div class="change-row" data-g="${esc(c.g)}">
      <b>${esc(label)}</b> · ${esc(c.c)} ${esc(entity)}
      <small>${esc(c.at || "")}</small></div>`;
  }).join("");
  $("dbody").innerHTML = `<div class="dsec">${rows || "<i>no version history yet</i>"}</div>`;
  $("dbody").querySelectorAll(".change-row").forEach(n => n.onclick = () => {
    const x = CAT.find(v => v.g === n.dataset.g);
    if (x) openDetail(x.g, x.la, x.lo);
  });
}

/* ── virtual list ── */
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
    const stars = x.ra ? `<b>${x.ra.toFixed(1)}★</b>` : "";
    const revs = x.vc ? ` · ${x.vc} rev` : "";
    const hr = x.hr ? ` · <span class="hr">★rec</span>` : "";
    const ac = x.ac ? ` · <span class="ac">${x.ac}✓</span>` : "";
    d.innerHTML =
      `<div class="t">${esc(x.t || "(untitled)")}</div>` +
      `<div class="s">${stars}${x.rc ? ` (${x.rc})` : ""} · ${esc(x.ty || "?")} · ` +
      `${x.sc ?? "?"} stg${revs}${hr}${ac} · ${esc(x.o || "")}</div>`;
    d.onclick = () => openDetail(x.g, x.la, x.lo);
    frag.appendChild(d);
  }
  list.appendChild(frag);
}

/* ── map ── */
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
    for (const l of ["clust", "pts"])
      map.on("mouseenter", l, () => map.getCanvas().style.cursor = "pointer"),
      map.on("mouseleave", l, () => map.getCanvas().style.cursor = "");

    /* flat source: every point, indexed once, filtered via layer filter
       (setFilter never re-tiles — this is what keeps "all" mode lag-free).
       maxzoom caps the geojson-vt tile pyramid; higher zooms overzoom. */
    map.addSource("labs-flat", {
      type: "geojson", data: { type: "FeatureCollection", features: [] },
      maxzoom: 12, buffer: 512          // wide buffer so heat splats aren't clipped at tile edges
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
    setTimeout(loadFlat, 60);   // index all points off the boot critical path
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

/* mirrors passFilters() as a MapLibre layer filter for labs-flat layers */
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
  if ($("fhr").checked)     all.push(["==", ["coalesce", ["get", "hr"], false], true]);
  if ($("ffetch").checked)  all.push(["==", ["coalesce", ["get", "f"], false], true]);
  if ($("factive").checked) all.push(["!=", ["coalesce", ["get", "arch"], false], true]);
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
  v("heat", m === "heat");
  v("pts-all", m === "all");
}
function pushMapData() {
  if (!map || !map.getSource("labs")) return;
  const feats = new Array(filtered.length);
  for (let i = 0; i < filtered.length; i++) {
    const x = filtered[i];
    if (x.la == null) continue;
    feats[i] = { type: "Feature",
      geometry: { type: "Point", coordinates: [x.lo, x.la] },
      properties: { g: x.g, ra: x.ra ?? 0, f: !!x.f } };
  }
  map.getSource("labs").setData({ type: "FeatureCollection",
    features: feats.filter(Boolean) });
}

/* ── detail drawer ── */
async function getDetail(g) {
  const key = g.slice(0, 2).toLowerCase();
  if (!shards.has(key)) {
    try {
      shards.set(key, await fetchGz(`data/detail/${key}.json.gz`));
    } catch {
      shards.set(key, {});          // shard not exported yet
    }
  }
  return shards.get(key)[g] || null;
}

async function getHistory(g) {
  const key = g.slice(0, 2).toLowerCase();
  if (!historyShards.has(key)) {
    try {
      historyShards.set(key, await fetchGz(`data/history/${key}.json.gz`));
    } catch {
      historyShards.set(key, {});
    }
  }
  return historyShards.get(key)[g] || [];
}

async function openDetail(g, la, lo) {
  state.sel = g;
  $("dshare").style.display = "";
  const u = new URL(location.href);
  u.searchParams.set("g", g);
  history.replaceState(null, "", u);
  $("drawer").classList.add("open");
  $("dtitle").textContent = "loading…";
  $("dbody").innerHTML = "";
  if (la != null && map) map.flyTo({
    center: [lo, la],
    zoom: Math.max(map.getZoom(), 10),
    padding: { right: $("drawer").offsetWidth + 40, left: 40, top: 40, bottom: 40 },
  });
  const [d, hist] = await Promise.all([getDetail(g), getHistory(g)]);
  const cat = CAT.find(x => x.g === g) || {};
  if (!d) {
    $("dtitle").textContent = cat.t || g;
    $("dbody").innerHTML = `<div class="dsec">Discovery data only — detail not fetched yet.<br>
      <span class="chip">${esc(g)}</span></div>`;
    return;
  }
  renderDetail(d, hist);
}

function kv(k, v) { return v == null || v === "" ? "" : `<div><span class="k">${k}</span>${esc(v)}</div>`; }
const stars = n => n ? "★".repeat(Math.round(n)) + "☆".repeat(5 - Math.round(n)) : "—";

function renderDetail(d, hist = []) {
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
    ${kv("published", (d.publishedUtc || "").slice(0, 10))}
    ${kv("visibility", d.visibility)}
    ${kv("coords", d.location ? d.location.latitude.toFixed(5) + ", " + d.location.longitude.toFixed(5) : null)}
    ${kv("guid", d.adventureGuid)}
  </div>${themes}</div>
  ${d.description ? `<div class="dsec"><h3>description</h3><div id="ddesc">${san(d.description)}</div></div>` : ""}
  <div class="dsec"><h3>stages (${st.length})</h3>${st.map((s, i) => renderStage(s, i, d.answers?.[i])).join("") || "<i>none</i>"}</div>
  <div class="dsec"><h3>recent reviews (${rv.length} of ${d.reviewsTotalCount || rv.length})</h3>${rv.map(renderReview).join("") || "<i>none</i>"}</div>
  <div class="dsec"><h3>history (${hist.length})</h3>${renderHistory(hist)}</div>`;
}

function renderHistory(hist) {
  if (!hist.length) return "<i>no recorded changes yet</i>";
  return hist.map(h => {
    const entity = h.e === "stage" ? `stage #${(h.i ?? 0) + 1}` :
      h.e === "review" ? `review #${h.i}` : "adventure";
    const snapshot = h.d == null ? "" :
      `<details><summary>view previous snapshot</summary><pre>${esc(JSON.stringify(h.d, null, 2))}</pre></details>`;
    return `<div class="hist"><div class="hh">
      <span class="chip">${esc(h.c)}</span>
      <b>${esc(entity)}</b>
      <span>v${esc(h.v)}</span>
      <span class="when">${esc(h.at || "")}</span>
    </div>${snapshot}</div>`;
  }).join("");
}

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
    `<a href="${esc(im.url || im.imageUrl || "#")}" target="_blank"><img src="${esc(im.url || im.imageUrl)}"></a>`).join("");
  const tags = (r.playerTags || []).map(t => `<span class="chip">${esc(t.name || t)}</span>`).join("");
  return `<div class="rev">
    <div class="rh"><span class="stars">${stars(r.rating)}</span>
      <b>${esc(r.playerUsername || "?")}</b>
      · ${(r.createdUtc || "").slice(0, 10)}
      ${r.isCreator ? " · <span class='chip'>creator</span>" : ""}</div>
    ${r.reviewText ? `<div class="rt">${esc(r.reviewText)}</div>` : ""}
    ${imgs}${tags}</div>`;
}

boot().catch(e => setLoad(100, "FAILED: " + e.message));
