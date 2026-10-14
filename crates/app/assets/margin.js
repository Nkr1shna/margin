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

  window.MarginApp = {
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
