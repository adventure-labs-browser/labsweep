(function () {
  "use strict";

  var style = document.createElement("style");
  style.textContent =
    "#labsweep-tools{display:flex;gap:5px;flex-wrap:wrap;margin-top:8px;align-items:center}" +
    "#labsweep-tools button,#labsweep-tools a{background:var(--panel2);color:var(--dim);border:1px solid var(--border);border-radius:6px;padding:4px 7px;font:inherit;font-size:11px;cursor:pointer;text-decoration:none}" +
    "#labsweep-tools button:hover,#labsweep-tools a:hover,#labsweep-tools button.on{color:var(--text);border-color:var(--accent)}" +
    "#dataset-age{margin-left:auto;font-size:10.5px;color:var(--dim)}" +
    "#dataset-age.fresh{color:var(--good)}#dataset-age.stale{color:var(--warn)}" +
    "#viewer-toast{position:absolute;left:50%;bottom:18px;z-index:80;transform:translate(-50%,20px);opacity:0;background:var(--panel);border:1px solid var(--border);border-radius:8px;padding:7px 11px;pointer-events:none;transition:.18s;box-shadow:0 8px 24px #0008}" +
    "#viewer-toast.on{transform:translate(-50%,0);opacity:1}" +
    ".drawer-extra-actions{display:flex;gap:6px;flex-wrap:wrap;margin-top:9px}.drawer-extra-actions a,.drawer-extra-actions button{background:var(--panel2);border:1px solid var(--border);color:var(--text);border-radius:6px;padding:5px 8px;font:inherit;text-decoration:none;cursor:pointer}" +
    ".lab-history{font-size:11.5px}.lab-history .event{padding:7px 0;border-top:1px solid var(--border)}.lab-history .event:first-child{border-top:0}.lab-history .when{color:var(--dim)}.lab-history code{color:var(--warn);white-space:pre-wrap;overflow-wrap:anywhere}" +
    ".row.favorite .t:before{content:'★ ';color:var(--warn)}";
  document.head.appendChild(style);

  var bar = document.createElement("div");
  bar.id = "labsweep-tools";
  bar.innerHTML =
    '<a href="history.html" title="Dataset history">history</a>' +
    '<button id="tool-random" title="Random visible lab (R)">random</button>' +
    '<button id="tool-fit" title="Fit matching labs">fit</button>' +
    '<button id="tool-near" title="Sort by distance from my location">near me</button>' +
    '<button id="tool-saved" title="Show only locally saved labs">★ saved</button>' +
    '<button id="tool-export" title="Export current filtered list as CSV">export</button>' +
    '<button id="tool-share" title="Copy this filtered view">copy view</button>' +
    '<span id="dataset-age">checking…</span>';
  var search = document.getElementById("search");
  if (search && search.parentNode) search.parentNode.insertBefore(bar, search.nextSibling);

  var toastEl = document.createElement("div");
  toastEl.id = "viewer-toast";
  document.getElementById("app").appendChild(toastEl);
  var toastTimer;
  function toast(msg) {
    toastEl.textContent = msg;
    toastEl.classList.add("on");
    clearTimeout(toastTimer);
    toastTimer = setTimeout(function () { toastEl.classList.remove("on"); }, 1800);
  }

  function fetchGzJson(url) {
    return fetch(url).then(function (r) {
      if (!r.ok) throw new Error(String(r.status));
      if (!r.body || typeof DecompressionStream === "undefined") return r.json();
      return new Response(r.body.pipeThrough(new DecompressionStream("gzip"))).json();
    });
  }

  var favorites = new Set();
  try {
    JSON.parse(localStorage.getItem("labsweep:favorites") || "[]").forEach(function (g) { favorites.add(g); });
  } catch (_) {}
  var savedOnly = false;
  function saveFavorites() {
    localStorage.setItem("labsweep:favorites", JSON.stringify(Array.from(favorites)));
  }
  function toggleFavorite(g) {
    if (favorites.has(g)) favorites.delete(g); else favorites.add(g);
    saveFavorites();
    applyFilters();
    toast(favorites.has(g) ? "saved locally" : "removed from saved");
  }

  var basePassFilters = passFilters;
  passFilters = function (x) {
    return basePassFilters(x) && (!savedOnly || favorites.has(x.g));
  };

  var baseRenderList = renderList;
  renderList = function () {
    baseRenderList();
    document.querySelectorAll("#list .row").forEach(function (node) {
      var idx = Math.round(parseFloat(node.style.top || "0") / ROWH);
      var x = filtered[idx];
      node.classList.toggle("favorite", !!x && favorites.has(x.g));
    });
  };

  function randomLab() {
    if (!filtered || !filtered.length) return toast("no matching labs");
    var x = filtered[Math.floor(Math.random() * filtered.length)];
    openDetail(x.g, x.la, x.lo);
  }

  function fitVisible() {
    if (!map || !filtered || !filtered.length) return;
    var pts = filtered.filter(function (x) { return x.la != null && x.lo != null; });
    if (!pts.length) return toast("no mapped labs");
    var b = new maplibregl.LngLatBounds();
    pts.forEach(function (x) { b.extend([x.lo, x.la]); });
    map.fitBounds(b, { padding: 50, maxZoom: 11 });
  }

  function haversine(lat1, lon1, lat2, lon2) {
    var r = 6371, p = Math.PI / 180;
    var a = Math.sin((lat2-lat1)*p/2) ** 2 +
      Math.cos(lat1*p) * Math.cos(lat2*p) * Math.sin((lon2-lon1)*p/2) ** 2;
    return 2 * r * Math.asin(Math.sqrt(a));
  }

  function locateAndSort() {
    if (!navigator.geolocation) return toast("location unavailable");
    navigator.geolocation.getCurrentPosition(function (p) {
      var lat = p.coords.latitude, lon = p.coords.longitude;
      CAT.forEach(function (x) {
        x.di = x.la == null || x.lo == null ? -Infinity : -haversine(lat, lon, x.la, x.lo);
      });
      state.sort = "di";
      document.querySelectorAll(".sortbtn").forEach(function (b) { b.classList.remove("on"); });
      var nearSort = document.getElementById("sort-nearest");
      if (!nearSort) {
        nearSort = document.createElement("button");
        nearSort.id = "sort-nearest";
        nearSort.className = "sortbtn";
        nearSort.dataset.k = "di";
        nearSort.textContent = "nearest";
        nearSort.onclick = function () {
          state.sort = "di";
          document.querySelectorAll(".sortbtn").forEach(function (b) { b.classList.remove("on"); });
          nearSort.classList.add("on");
          applyFilters();
        };
        document.getElementById("count").before(nearSort);
      }
      nearSort.classList.add("on");
      applyFilters();
      map.flyTo({ center: [lon, lat], zoom: Math.max(map.getZoom(), 8) });
      toast("sorted nearest first");
    }, function () { toast("location unavailable"); }, { enableHighAccuracy: false, timeout: 8000 });
  }

  function exportCsv() {
    if (!filtered || !filtered.length) return toast("nothing to export");
    var rows = [["guid","title","owner","type","rating","ratings","reviews","stages","completions","latitude","longitude","published","archived"]];
    filtered.forEach(function (x) {
      rows.push([x.g, x.t || "", x.o || "", x.ty || "", x.ra == null ? "" : x.ra,
        x.rc == null ? "" : x.rc, x.vc == null ? "" : x.vc, x.sc == null ? "" : x.sc,
        x.cc == null ? "" : x.cc, x.la == null ? "" : x.la, x.lo == null ? "" : x.lo,
        x.p || "", !!x.arch]);
    });
    var csv = rows.map(function (r) {
      return r.map(function (v) { return '"' + String(v).replace(/"/g, '""') + '"'; }).join(",");
    }).join("\n");
    var a = document.createElement("a");
    a.href = URL.createObjectURL(new Blob([csv], { type: "text/csv" }));
    a.download = "labsweep-" + new Date().toISOString().slice(0, 10) + "-" + filtered.length + ".csv";
    a.click();
    setTimeout(function () { URL.revokeObjectURL(a.href); }, 1000);
    toast("exported " + filtered.length.toLocaleString() + " labs");
  }

  function viewUrl() {
    var u = new URL(location.href);
    var params = {
      q: document.getElementById("search").value || "",
      type: document.getElementById("ftype").value || "",
      minr: document.getElementById("fminr").value || "0",
      minst: document.getElementById("fminst").value || "0",
      minrv: document.getElementById("fminrv").value || "0",
      hr: document.getElementById("fhr").checked ? "1" : "",
      fetched: document.getElementById("ffetch").checked ? "1" : "",
      active: document.getElementById("factive").checked ? "1" : "0",
      sort: state.sort || "ra"
    };
    Object.keys(params).forEach(function (k) {
      if (params[k] === "" || params[k] === "0" && !["minr","minst","minrv","active"].includes(k)) u.searchParams.delete(k);
      else u.searchParams.set(k, params[k]);
    });
    u.searchParams.delete("lab");
    return u;
  }

  function restoreView() {
    var p = new URLSearchParams(location.search);
    if (p.has("q")) { document.getElementById("search").value = p.get("q"); state.q = p.get("q").trim().toLowerCase(); }
    if (p.has("type")) document.getElementById("ftype").value = p.get("type");
    if (p.has("minr")) document.getElementById("fminr").value = p.get("minr");
    if (p.has("minst")) document.getElementById("fminst").value = p.get("minst");
    if (p.has("minrv")) document.getElementById("fminrv").value = p.get("minrv");
    if (p.has("hr")) document.getElementById("fhr").checked = p.get("hr") === "1";
    if (p.has("fetched")) document.getElementById("ffetch").checked = p.get("fetched") === "1";
    if (p.has("active")) document.getElementById("factive").checked = p.get("active") === "1";
    if (p.has("sort") && p.get("sort") !== "di") state.sort = p.get("sort");
    applyFilters();
  }

  document.getElementById("tool-random").onclick = randomLab;
  document.getElementById("tool-fit").onclick = fitVisible;
  document.getElementById("tool-near").onclick = locateAndSort;
  document.getElementById("tool-export").onclick = exportCsv;
  document.getElementById("tool-saved").onclick = function () {
    savedOnly = !savedOnly;
    this.classList.toggle("on", savedOnly);
    applyFilters();
  };
  document.getElementById("tool-share").onclick = function () {
    navigator.clipboard.writeText(viewUrl().toString()).then(function () { toast("view link copied"); });
  };

  document.addEventListener("keydown", function (e) {
    var tag = document.activeElement && document.activeElement.tagName || "";
    if (e.key === "/" && tag !== "INPUT") {
      e.preventDefault();
      document.getElementById("search").focus();
      document.getElementById("search").select();
    } else if (e.key.toLowerCase() === "r" && !/INPUT|SELECT|TEXTAREA/.test(tag)) {
      randomLab();
    } else if (e.key === "Escape") {
      var d = document.getElementById("drawer");
      if (d) d.classList.remove("open");
    }
  });

  function updateFreshness(meta) {
    var el = document.getElementById("dataset-age");
    var built = meta && (meta.generatedAt || meta.generated_at);
    var stamp = meta && meta.freshness && meta.freshness.latestAdventureFetch || built;
    if (!stamp) {
      el.textContent = "live dataset";
      return;
    }
    var ageMs = Date.now() - new Date(stamp).getTime();
    var hours = Math.max(0, Math.round(ageMs / 3600000));
    el.textContent = hours < 1 ? "data <1h old" : "data " + hours + "h old";
    el.classList.toggle("fresh", hours < 30);
    el.classList.toggle("stale", hours >= 30);
    el.title = "Latest adventure fetch " + new Date(stamp).toLocaleString() +
      (built ? " · viewer built " + new Date(built).toLocaleString() : "");
  }
  fetch("data/meta.json", { cache: "no-cache" }).then(function (r) {
    return r.ok ? r.json() : null;
  }).then(updateFreshness).catch(function () { updateFreshness(null); });

  function val(v) {
    if (v == null) return "∅";
    if (typeof v === "string") return v;
    return JSON.stringify(v);
  }

  async function appendLabHistory(g) {
    var host = document.getElementById("lab-history");
    if (!host) return;
    var key = String(g).slice(0, 2).toLowerCase();
    try {
      var shard = await fetchGzJson("data/history/" + key + ".json.gz");
      var h = shard[g] && shard[g].events || [];
      if (!h.length) {
        host.innerHTML = '<span style="color:var(--dim)">No recorded changes yet.</span>';
        return;
      }
      host.innerHTML = h.slice(0, 80).map(function (e) {
        var diffs = (e.d || []).map(function (d) {
          return '<div class="delta"><code>' + esc(d.f) + '</code>: ' + esc(val(d.a)) + ' → ' + esc(val(d.b)) + '</div>';
        }).join("");
        return '<div class="event"><div><b>' + esc(e.k) + '</b>' +
          (e.x == null ? "" : " #" + esc(e.x)) + ' · ' + esc(e.c || "changed") +
          ' <span class="when">' + esc((e.t || "").replace("T", " ").slice(0, 19)) + '</span></div>' +
          diffs + '</div>';
      }).join("");
    } catch (_) {
      host.innerHTML = '<span style="color:var(--dim)">No recorded changes yet.</span>';
    }
  }

  var originalRenderDetail = renderDetail;
  renderDetail = function (d) {
    originalRenderDetail(d);
    var first = document.querySelector("#dbody .dsec");
    if (!first) return;
    var actions = document.createElement("div");
    actions.className = "drawer-extra-actions";
    var g = d.adventureGuid || "";
    actions.innerHTML =
      '<a target="_blank" rel="noopener" href="https://labs.geocaching.com/goto/' + encodeURIComponent(g) + '">open Adventure Lab ↗</a>' +
      '<button type="button" id="fav-lab">' + (favorites.has(g) ? "★ saved" : "☆ save") + '</button>' +
      '<button type="button" id="copy-lab-link">copy link</button>' +
      (d.location ? '<button type="button" id="copy-lab-coords">copy coords</button>' : "");
    first.appendChild(actions);
    document.getElementById("fav-lab").onclick = function () {
      toggleFavorite(g);
      this.textContent = favorites.has(g) ? "★ saved" : "☆ save";
    };
    document.getElementById("copy-lab-link").onclick = function () {
      var u = new URL(location.href);
      u.searchParams.set("lab", g);
      navigator.clipboard.writeText(u.toString()).then(function () { toast("link copied"); });
    };
    if (d.location) {
      document.getElementById("copy-lab-coords").onclick = function () {
        var s = Number(d.location.latitude).toFixed(6) + ", " + Number(d.location.longitude).toFixed(6);
        navigator.clipboard.writeText(s).then(function () { toast("coordinates copied"); });
      };
    }
    var sec = document.createElement("div");
    sec.className = "dsec lab-history";
    sec.innerHTML = '<h3>change history</h3><div id="lab-history"><span style="color:var(--dim)">loading…</span></div>';
    document.getElementById("dbody").appendChild(sec);
    appendLabHistory(g);
  };

  var originalOpenDetail = openDetail;
  openDetail = async function (g, la, lo) {
    var u = new URL(location.href);
    u.searchParams.set("lab", g);
    history.replaceState(null, "", u);
    return originalOpenDetail(g, la, lo);
  };

  var close = document.getElementById("dclose");
  if (close) close.addEventListener("click", function () {
    var u = new URL(location.href);
    u.searchParams.delete("lab");
    history.replaceState(null, "", u);
  });

  function openDeepLink(attempt) {
    var g = new URLSearchParams(location.search).get("lab");
    if (!g) return;
    if (!CAT || !CAT.length) {
      if (attempt < 40) setTimeout(function () { openDeepLink(attempt + 1); }, 250);
      return;
    }
    var x = CAT.find(function (r) { return String(r.g).toLowerCase() === g.toLowerCase(); });
    if (x) openDetail(x.g, x.la, x.lo);
  }

  window.addEventListener("load", function () {
    restoreView();
    openDeepLink(0);
  });
})();