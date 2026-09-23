// Metrics page: one uPlot chart per card. Colours and fonts come from the CSS
// custom properties at draw time, so a custom stylesheet restyles the charts
// without a second source of truth.
(function () {
  "use strict";

  var cardsEl = document.getElementById("chart-cards");
  if (!cardsEl || typeof uPlot === "undefined") return;
  var CARDS = JSON.parse(cardsEl.textContent || "[]");
  var charts = {};
  var range = currentRange();

  function currentRange() {
    var m = /[?&]range=(24h|7d|90d)/.exec(location.search);
    return m ? m[1] : "24h";
  }

  function css(name, fallback) {
    var v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
    return v || fallback;
  }

  // Fill colours need alpha; accept the hex values our stylesheet uses and
  // fall back to a plain translucent grey for anything else.
  function alpha(color, a) {
    var m = /^#([0-9a-f]{6})$/i.exec(color);
    if (!m) return "rgba(128,128,128," + a + ")";
    var n = parseInt(m[1], 16);
    return "rgba(" + (n >> 16) + "," + ((n >> 8) & 255) + "," + (n & 255) + "," + a + ")";
  }

  function fmtBytes(v, perSec) {
    var units = ["B", "KB", "MB", "GB", "TB"];
    var i = 0;
    var x = Math.abs(v);
    while (x >= 1024 && i < units.length - 1) { x /= 1024; i++; }
    var s = (x >= 100 ? x.toFixed(0) : x >= 10 ? x.toFixed(1) : x.toFixed(2)) + " " + units[i];
    return perSec ? s + "/s" : s;
  }

  function fmt(v, unit) {
    if (v === null || v === undefined || isNaN(v)) return "–";
    if (unit === "%") return v.toFixed(1) + "%";
    if (unit === "ms") return (v >= 100 ? v.toFixed(0) : v.toFixed(1)) + " ms";
    if (unit === "bytes/s") return fmtBytes(v, true);
    if (unit === "bytes") return fmtBytes(v, false);
    return v.toFixed(2);
  }

  function fmtTime(ts) {
    var d = new Date(ts * 1000);
    var opts = range === "24h"
      ? { hour: "2-digit", minute: "2-digit" }
      : { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" };
    return d.toLocaleString(undefined, opts);
  }

  // One short line per tick; uPlot's default two-line dates collide at
  // card width.
  function fmtAxisTime(ts) {
    var d = new Date(ts * 1000);
    return range === "24h"
      ? d.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" })
      : d.toLocaleDateString(undefined, { month: "short", day: "numeric" });
  }

  function part(card, role) {
    return document.querySelector("#" + card.id + " [data-role=" + role + "]");
  }

  function fetchSeries(s) {
    var url = "/api/metrics?scope=" + encodeURIComponent(s.scope) +
      "&metric=" + encodeURIComponent(s.metric) +
      "&key=" + encodeURIComponent(s.key) +
      "&range=" + encodeURIComponent(range);
    return fetch(url, { credentials: "same-origin" }).then(function (r) {
      if (!r.ok) throw new Error("HTTP " + r.status);
      return r.json();
    });
  }

  // Series of one card can have different timestamps (e.g. network in/out
  // written in the same tick but possibly missing one); merge on a shared x.
  function align(results) {
    var xs = {};
    results.forEach(function (r) { r.t.forEach(function (t) { xs[t] = true; }); });
    var x = Object.keys(xs).map(Number).sort(function (a, b) { return a - b; });
    var data = [x];
    results.forEach(function (r) {
      var map = {};
      r.t.forEach(function (t, i) { map[t] = r.v[i]; });
      data.push(x.map(function (t) { return t in map ? map[t] : null; }));
    });
    return data;
  }

  function summary(card, results) {
    var first = results[0];
    var value = part(card, "value");
    var sub = part(card, "sub");
    if (results.length === 1) {
      value.textContent = fmt(first.last, first.unit);
      sub.textContent = first.points ? "avg " + fmt(first.avg, first.unit) + " · max " + fmt(first.max, first.unit) : " ";
    } else {
      value.textContent = results.map(function (r) { return fmt(r.last, r.unit); }).join(" / ");
      sub.textContent = card.series.map(function (s, i) {
        return s.label + " max " + fmt(results[i].max, results[i].unit);
      }).join(" · ");
    }
    card.defaultValue = value.textContent;
  }

  // Lines are ink; extra series step down to softer greys instead of hues,
  // because colour on this page is kept for problems.
  function colors() {
    return [css("--ink", "#17150f"), css("--soft", "#5e5a50"), css("--calm", "#a8a293")];
  }

  // The trouble line of a card, dashed in the problem colour.
  function thresholdHook(card) {
    return function (u) {
      if (card.threshold == null) return;
      var y = u.valToPos(card.threshold, "y", true);
      if (!isFinite(y) || y < u.bbox.top || y > u.bbox.top + u.bbox.height) return;
      var ctx = u.ctx;
      ctx.save();
      ctx.strokeStyle = css("--bad", "#e2461f");
      ctx.lineWidth = 1.5 * devicePixelRatio;
      ctx.setLineDash([5 * devicePixelRatio, 4 * devicePixelRatio]);
      ctx.beginPath();
      ctx.moveTo(u.bbox.left, Math.round(y));
      ctx.lineTo(u.bbox.left + u.bbox.width, Math.round(y));
      ctx.stroke();
      ctx.restore();
    };
  }

  function build(card, results) {
    var body = part(card, "chart");
    var empty = part(card, "empty");
    if (charts[card.id]) { charts[card.id].destroy(); delete charts[card.id]; }
    var total = results.reduce(function (n, r) { return n + r.points; }, 0);
    if (!total) {
      empty.textContent = "No data for this range yet.";
      empty.hidden = false;
      return;
    }
    empty.hidden = true;

    var palette = colors();
    var grid = css("--hair", "#dcd9d0");
    var muted = css("--soft", "#5e5a50");
    var mono = css("--mono", "ui-monospace, monospace");
    var axisFont = "10.5px " + mono;
    var hasRight = card.series.some(function (s) { return s.right; });
    var leftUnit = results[0].unit;
    var rightIdx = card.series.findIndex(function (s) { return s.right; });
    var rightUnit = rightIdx >= 0 ? results[rightIdx].unit : "";

    var series = [{ value: function (u, v) { return v == null ? "" : fmtTime(v); } }];
    card.series.forEach(function (s, i) {
      var c = palette[i % palette.length];
      series.push({
        label: s.label,
        scale: s.right ? "r" : "y",
        stroke: c,
        width: 1.5,
        fill: card.series.length === 1 ? alpha(grid, 0.55) : undefined,
        points: { show: false },
        spanGaps: false,
        value: function (u, v) { return fmt(v, results[i].unit); }
      });
    });

    var axis = function (scale, unit, side) {
      return {
        scale: scale,
        side: side,
        stroke: muted,
        font: axisFont,
        size: unit === "bytes/s" || unit === "bytes" ? 76 : 58,
        grid: { show: side === 3, stroke: grid, width: 1 },
        ticks: { show: false },
        values: function (u, vals) { return vals.map(function (v) { return fmt(v, unit); }); }
      };
    };
    var axes = [
      {
        stroke: muted, font: axisFont, space: 70, grid: { show: false },
        ticks: { stroke: grid, width: 1, size: 4 },
        values: function (u, vals) { return vals.map(fmtAxisTime); }
      },
      axis("y", leftUnit, 3)
    ];
    if (hasRight) axes.push(axis("r", rightUnit, 1));

    var scales = { x: { time: true }, y: { range: yRange(leftUnit, card.threshold) } };
    if (hasRight) scales.r = { range: yRange(rightUnit) };

    var opts = {
      width: Math.max(body.clientWidth, 200),
      height: 160,
      series: series,
      axes: axes,
      scales: scales,
      legend: { show: false },
      cursor: {
        y: false,
        points: { size: 6, fill: css("--bg", "#f1f0ec") },
        drag: { x: false, y: false }
      },
      hooks: {
        draw: [thresholdHook(card)],
        setCursor: [function (u) {
          var idx = u.cursor.idx;
          var value = part(card, "value");
          if (idx == null) { value.textContent = card.defaultValue; return; }
          var vals = card.series.map(function (s, i) { return fmt(u.data[i + 1][idx], results[i].unit); });
          value.textContent = vals.join(" / ");
          part(card, "sub").textContent = fmtTime(u.data[0][idx]);
        }]
      }
    };
    var u = new uPlot(opts, align(results), body);
    u.over.addEventListener("mouseleave", function () { summary(card, results); });
    charts[card.id] = u;
    card.results = results;
  }

  // Percentages keep a fixed 0-100 axis so cards are comparable at a glance;
  // other units leave room for the trouble line so it is always visible.
  function yRange(unit, line) {
    if (unit === "%") return [0, 100];
    return function (u, min, max) {
      var top = Math.max(max, line || 0);
      return [0, top > 0 ? top * 1.1 : 1];
    };
  }

  function load(card) {
    Promise.all(card.series.map(fetchSeries)).then(function (results) {
      summary(card, results);
      build(card, results);
    }).catch(function () {
      var empty = part(card, "empty");
      empty.textContent = "Could not load this chart.";
      empty.hidden = false;
    });
  }

  function loadAll() { CARDS.forEach(load); }

  // Range control: links work without script; with it, swap data in place.
  var seg = document.querySelector(".range");
  if (seg) {
    seg.addEventListener("click", function (e) {
      var a = e.target.closest("a[data-range]");
      if (!a) return;
      e.preventDefault();
      range = a.getAttribute("data-range");
      seg.querySelectorAll("a").forEach(function (x) { x.removeAttribute("aria-current"); });
      a.setAttribute("aria-current", "true");
      history.replaceState(null, "", "?range=" + range);
      loadAll();
    });
  }

  if (typeof ResizeObserver !== "undefined") {
    var ro = new ResizeObserver(function (entries) {
      entries.forEach(function (entry) {
        var card = entry.target.closest(".chart-card");
        var u = card && charts[card.id];
        if (u) u.setSize({ width: Math.max(entry.contentRect.width, 200), height: 160 });
      });
    });
    document.querySelectorAll(".chart-body").forEach(function (b) { ro.observe(b); });
  }

  // Canvas text does not wait for web fonts; draw once they are in so the
  // axis labels are not stuck in the fallback face.
  if (document.fonts && document.fonts.ready) {
    document.fonts.ready.then(loadAll, loadAll);
  } else {
    loadAll();
  }
})();
