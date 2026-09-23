// Shared page behaviour: local times, the uptime-bar tooltip and keyboard
// movement along a bar strip. Listeners are delegated from `document` so the
// status page can be swapped in place by htmx without re-binding anything.
(function () {
  "use strict";

  var timeFmt = { hour: "2-digit", minute: "2-digit" };
  var dateTimeFmt = { year: "numeric", month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" };

  function zoneName(d) {
    try {
      var parts = new Intl.DateTimeFormat(undefined, { timeZoneName: "short" }).formatToParts(d);
      for (var i = 0; i < parts.length; i++) {
        if (parts[i].type === "timeZoneName") return parts[i].value;
      }
    } catch (e) { /* old engines: keep the plain time */ }
    return "";
  }

  function localizeTimes(root) {
    var nodes = (root || document).querySelectorAll("time[data-local]");
    for (var i = 0; i < nodes.length; i++) {
      var el = nodes[i];
      var d = new Date(el.getAttribute("datetime"));
      if (isNaN(d)) continue;
      var text = d.toLocaleString(undefined, el.getAttribute("data-local") === "time" ? timeFmt : dateTimeFmt);
      if (el.hasAttribute("data-zone")) {
        var z = zoneName(d);
        if (z) text += " " + z;
      }
      el.textContent = text;
      el.title = d.toUTCString();
    }
  }

  var tip = null;
  function tooltip() {
    if (!tip) {
      tip = document.createElement("div");
      tip.className = "tooltip";
      tip.id = "tooltip";
      tip.setAttribute("role", "tooltip");
      tip.hidden = true;
      document.body.appendChild(tip);
    }
    return tip;
  }

  function show(target) {
    var t = tooltip();
    t.textContent = "";
    if (target.classList.contains("bar")) {
      var date = document.createElement("strong");
      date.textContent = target.getAttribute("data-date");
      var state = document.createElement("span");
      state.className = "tooltip-state";
      state.textContent = target.getAttribute("data-label");
      t.appendChild(date);
      var up = target.getAttribute("data-uptime");
      if (up) {
        var u = document.createElement("span");
        u.className = "tooltip-uptime num";
        u.textContent = up + " uptime";
        t.appendChild(u);
      }
      t.appendChild(state);
      t.setAttribute("data-state", target.className.replace(/.*\bs-(\S+).*/, "$1"));
    } else {
      t.textContent = target.getAttribute("data-tip");
      t.removeAttribute("data-state");
    }
    t.hidden = false;
    target.setAttribute("aria-describedby", "tooltip");

    var r = target.getBoundingClientRect();
    var w = t.offsetWidth, h = t.offsetHeight;
    var x = r.left + r.width / 2 - w / 2;
    x = Math.max(8, Math.min(x, document.documentElement.clientWidth - w - 8));
    var y = r.top - h - 8;
    if (y < 8) y = r.bottom + 8;
    t.style.left = x + window.scrollX + "px";
    t.style.top = y + window.scrollY + "px";
  }

  function hide() {
    if (tip) tip.hidden = true;
    var owner = document.querySelector("[aria-describedby=tooltip]");
    if (owner) owner.removeAttribute("aria-describedby");
  }

  function tipTarget(el) {
    return el && el.closest ? el.closest(".bar, [data-tip]") : null;
  }

  document.addEventListener("mouseover", function (e) {
    var t = tipTarget(e.target);
    if (t) show(t);
  });
  document.addEventListener("mouseout", function (e) {
    if (tipTarget(e.target) && !tipTarget(e.relatedTarget)) hide();
  });
  document.addEventListener("focusin", function (e) {
    var t = tipTarget(e.target);
    if (t) show(t); else hide();
  });
  document.addEventListener("focusout", function (e) {
    if (tipTarget(e.target)) hide();
  });

  // One tab stop per strip (roving tabindex); arrows walk the days so a
  // keyboard user is not forced through 90 stops per component.
  document.addEventListener("keydown", function (e) {
    if (e.key === "Escape") { hide(); return; }
    var bar = e.target;
    if (!bar.classList || !bar.classList.contains("bar")) return;
    var bars = Array.prototype.filter.call(bar.parentNode.children, function (b) {
      return b.offsetParent !== null;
    });
    var i = bars.indexOf(bar), next = null;
    if (e.key === "ArrowLeft") next = bars[i - 1];
    else if (e.key === "ArrowRight") next = bars[i + 1];
    else if (e.key === "Home") next = bars[0];
    else if (e.key === "End") next = bars[bars.length - 1];
    if (!next) return;
    e.preventDefault();
    bar.tabIndex = -1;
    next.tabIndex = 0;
    next.focus();
  });

  // A poll that lands on the login page (session expired under protect_read)
  // must not blank the status page.
  document.addEventListener("htmx:beforeSwap", function (e) {
    var xhr = e.detail && e.detail.xhr;
    if (!xhr || xhr.status !== 200 || xhr.responseText.indexOf('id="live"') < 0) {
      e.detail.shouldSwap = false;
    }
  });
  document.addEventListener("htmx:afterSettle", function () {
    hide();
    localizeTimes();
  });

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", function () { localizeTimes(); });
  } else {
    localizeTimes();
  }
})();
