// Small webview helpers for the Margin app. Rust calls these through document::eval.
(function () {
  "use strict";

  // Render every un-rendered math span under `root` with the bundled KaTeX.
  function renderMath(root) {
    if (!root) return;
    if (!window.katex) {
      // KaTeX loads with the page; try again shortly if this runs first.
      setTimeout(function () { renderMath(root); }, 50);
      return;
    }
    var spans = root.querySelectorAll(".math:not(.katex-done)");
    for (var i = 0; i < spans.length; i++) {
      var el = spans[i];
      var tex = el.textContent;
      try {
        window.katex.render(tex, el, {
          displayMode: el.classList.contains("math-display"),
          throwOnError: false,
          output: "html",
        });
        el.classList.add("katex-done");
      } catch (e) {
        el.classList.add("katex-done");
        el.textContent = tex;
      }
    }
  }

  // Typesets the math in `node` itself and in its descendants.
  function renderMathNode(node) {
    if (node.nodeType !== 1) return;
    if (node.classList.contains("math")) {
      var wrap = document.createElement("div");
      node.parentNode ? renderMath(node.parentNode) : (wrap.appendChild(node), renderMath(wrap), wrap.removeChild(node));
    } else {
      renderMath(node);
    }
  }

  window.MarginApp = {
    // Updates #id's content to `html` without flicker: top-level blocks whose source is
    // unchanged keep their existing (already typeset) DOM; changed blocks are typeset
    // before they're inserted, so raw TeX never paints. Used while a lesson streams in.
    setHtml: function (id, html) {
      var el = document.getElementById(id);
      if (!el) return;
      var tpl = document.createElement("template");
      tpl.innerHTML = html;
      var fresh = Array.prototype.slice.call(tpl.content.childNodes);
      var old = Array.prototype.slice.call(el.childNodes);
      for (var i = 0; i < fresh.length; i++) {
        var node = fresh[i];
        var src = node.nodeType === 1 ? node.outerHTML : node.textContent;
        var current = old[i];
        if (current && current.__marginSrc === src) continue;
        if (window.katex) renderMathNode(node);
        node.__marginSrc = src;
        if (current) el.replaceChild(node, current); else el.appendChild(node);
      }
      for (var j = old.length - 1; j >= fresh.length; j--) el.removeChild(old[j]);
      if (!window.katex) setTimeout(function () { renderMath(el); }, 50);
    },
    renderMath: function (id) {
      setTimeout(function () { renderMath(document.getElementById(id)); });
    },
    renderMathIn: function (selector) {
      setTimeout(function () {
        var nodes = document.querySelectorAll(selector);
        for (var i = 0; i < nodes.length; i++) renderMath(nodes[i]);
      });
    },
    // Watch text selection inside #`bodyId`; call send({text, top}) where `top` is the
    // selection's offset from the top of the `.margin-layout` container. Only one
    // watcher is active at a time.
    watchSelection: function (bodyId, send) {
      if (window.MarginApp._stopSelection) window.MarginApp._stopSelection();
      var timer = null;
      function report() {
        var body = document.getElementById(bodyId);
        if (!body) return;
        var sel = window.getSelection();
        if (!sel || sel.rangeCount === 0 || sel.isCollapsed) {
          send({ text: "", top: 0 });
          return;
        }
        var range = sel.getRangeAt(0);
        if (!body.contains(range.commonAncestorContainer)) return;
        var text = sel.toString().replace(/\s+/g, " ").trim();
        if (!text) { send({ text: "", top: 0 }); return; }
        var rect = range.getBoundingClientRect();
        var layout = body.closest(".margin-layout") || body;
        var top = rect.top - layout.getBoundingClientRect().top;
        send({ text: text, top: Math.max(0, top) });
      }
      function later() { clearTimeout(timer); timer = setTimeout(report, 10); }
      function onUp(e) {
        var body = document.getElementById(bodyId);
        if (body && body.contains(e.target)) later();
      }
      function onKey(e) {
        if (e.shiftKey || e.key === "Escape") later();
      }
      function onSelChange() {
        // Touch devices adjust selections with handles and fire no mouseup.
        if (window.matchMedia("(pointer: coarse)").matches) {
          clearTimeout(timer);
          timer = setTimeout(report, 400);
        }
      }
      document.addEventListener("mouseup", onUp);
      document.addEventListener("touchend", onUp);
      document.addEventListener("keyup", onKey);
      document.addEventListener("selectionchange", onSelChange);
      window.MarginApp._stopSelection = function () {
        document.removeEventListener("mouseup", onUp);
        document.removeEventListener("touchend", onUp);
        document.removeEventListener("keyup", onKey);
        document.removeEventListener("selectionchange", onSelChange);
        clearTimeout(timer);
        window.MarginApp._stopSelection = null;
      };
    },
    stopSelection: function () {
      if (window.MarginApp._stopSelection) window.MarginApp._stopSelection();
    },
    clearSelection: function () {
      var sel = window.getSelection();
      if (sel) sel.removeAllRanges();
    },
    // Global shortcuts: Cmd/Ctrl+K opens search, Cmd/Ctrl+N opens Ask.
    watchShortcuts: function (send) {
      if (window.MarginApp._stopShortcuts) window.MarginApp._stopShortcuts();
      function onKey(e) {
        if (!(e.metaKey || e.ctrlKey) || e.altKey || e.shiftKey) return;
        var k = (e.key || "").toLowerCase();
        if (k === "k") { e.preventDefault(); send("search"); }
        else if (k === "n") { e.preventDefault(); send("ask"); }
        else if (k === "\\") { e.preventDefault(); send("sidebar"); }
        else if (k === ",") { e.preventDefault(); send("settings"); }
        else if (k === "[") { e.preventDefault(); send("back"); }
        else if (k === "]") { e.preventDefault(); send("forward"); }
      }
      document.addEventListener("keydown", onKey, true);
      window.MarginApp._stopShortcuts = function () {
        document.removeEventListener("keydown", onKey, true);
      };
    },
    setTheme: function (theme) {
      if (theme) document.documentElement.setAttribute("data-theme", theme);
      else document.documentElement.removeAttribute("data-theme");
    },
    // Set up a lesson video player once it's in the page: apply the remembered volume
    // and mute, remember every change (volumechange) and mirror it on the other players,
    // and, without a poster image, show a frame from a few seconds in as the thumbnail.
    preparePlayer: function (id, hasPoster) {
      var v = document.getElementById(id);
      if (!v || v.__marginPrepared) return;
      v.__marginPrepared = true;
      var saved = readVolume();
      if (saved) {
        v.volume = saved.volume;
        v.muted = saved.muted;
      }
      v.addEventListener("volumechange", function () {
        writeVolume(v.volume, v.muted);
        var players = document.querySelectorAll("video.player");
        for (var i = 0; i < players.length; i++) {
          var p = players[i];
          if (p === v) continue;
          if (p.volume !== v.volume) p.volume = v.volume;
          if (p.muted !== v.muted) p.muted = v.muted;
        }
      });
      if (hasPoster || v.dataset.thumb) return;
      v.dataset.thumb = "pending";
      var show = function () {
        if (v.dataset.thumb !== "pending" || !isFinite(v.duration)) return;
        v.dataset.thumb = "shown";
        v.currentTime = Math.min(v.duration * 0.25, 3);
      };
      if (v.readyState >= 1) show(); else v.addEventListener("loadedmetadata", show, { once: true });
      v.addEventListener("play", function () {
        if (v.dataset.thumb === "shown") v.currentTime = 0;
        v.dataset.thumb = "played";
        for (var i = 0; i < v.textTracks.length; i++) {
          var t = v.textTracks[i];
          if (t.label === "Captions" && t.mode === "hidden") t.mode = "showing";
        }
      }, { once: true });
    },
    scrollIntoView: function (selector) {
      var el = document.querySelector(selector);
      if (el) el.scrollIntoView({ block: "nearest" });
    },
    copyText: function (text) {
      if (navigator.clipboard && window.isSecureContext) {
        return navigator.clipboard.writeText(text).then(function () { return true; }, function () { return fallbackCopy(text); });
      }
      return Promise.resolve(fallbackCopy(text));
    },
  };

  // The player volume and mute, shared by every lesson video and kept across restarts.
  var VOLUME_KEY = "margin.player.volume";
  function readVolume() {
    try {
      var raw = window.localStorage.getItem(VOLUME_KEY);
      if (!raw) return null;
      var s = JSON.parse(raw);
      if (typeof s.volume !== "number" || !(s.volume >= 0 && s.volume <= 1)) return null;
      return { volume: s.volume, muted: s.muted === true };
    } catch (e) {
      return null;
    }
  }
  function writeVolume(volume, muted) {
    try {
      window.localStorage.setItem(VOLUME_KEY, JSON.stringify({ volume: volume, muted: muted }));
    } catch (e) {
      // Storage unavailable: the setting still applies for this session's players.
    }
  }

  function fallbackCopy(text) {
    var ta = document.createElement("textarea");
    ta.value = text;
    ta.setAttribute("readonly", "");
    ta.style.position = "fixed";
    ta.style.opacity = "0";
    document.body.appendChild(ta);
    ta.select();
    var ok = false;
    try { ok = document.execCommand("copy"); } catch (e) { ok = false; }
    document.body.removeChild(ta);
    return ok;
  }
})();
