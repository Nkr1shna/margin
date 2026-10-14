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
      if (el.classList.contains("math-display")) watchOverflow(el);
    }
  }

  // Re-check a display formula when its typeset width changes (KaTeX's fonts load late)
  // as well as on resize.
  var overflowObserver = window.ResizeObserver ? new ResizeObserver(function (entries) {
    for (var i = 0; i < entries.length; i++) {
      var box = entries[i].target.closest(".math-display");
      if (box) mathOverflow(box);
    }
  }) : null;
  function watchOverflow(el) {
    mathOverflow(el);
    var inner = el.querySelector(".katex");
    if (overflowObserver && inner) overflowObserver.observe(inner);
    if (document.fonts && document.fonts.ready) document.fonts.ready.then(function () { mathOverflow(el); });
  }

  // A display formula wider than its box scrolls sideways; fade the edge it continues
  // past so it doesn't look cut off. Updated on scroll and on resize.
  function mathOverflow(el) {
    var max = el.scrollWidth - el.clientWidth;
    el.classList.toggle("more-right", max > 1 && el.scrollLeft < max - 1);
    el.classList.toggle("more-left", max > 1 && el.scrollLeft > 1);
  }
  document.addEventListener("scroll", function (e) {
    var t = e.target;
    if (t && t.classList && t.classList.contains("math-display")) mathOverflow(t);
  }, true);
  var overflowTimer = null;
  window.addEventListener("resize", function () {
    clearTimeout(overflowTimer);
    overflowTimer = setTimeout(function () {
      var all = document.querySelectorAll(".math-display.katex-done");
      for (var i = 0; i < all.length; i++) mathOverflow(all[i]);
    }, 100);
  });

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
      if (quoteMarks[id]) markQuotes(el, quoteMarks[id]);
    },
    // Mark each margin note's passage in #`id` ([{id, quote}]); kept across setHtml.
    // Tapping a marked passage shows its note (#note-<id>).
    markQuotes: function (id, notes) {
      quoteMarks[id] = notes || [];
      var el = document.getElementById(id);
      if (el) markQuotes(el, quoteMarks[id]);
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
    // Resizing the sidebar by its right edge, with a mouse, a finger or a pencil. The width
    // follows the pointer live (a CSS variable on .app); the final width is sent to the
    // app to save. A double click or double tap resets.
    watchSidebarResize: function (min, max, send) {
      if (window.MarginApp._stopResize) window.MarginApp._stopResize();
      function app() { return document.querySelector(".app"); }
      var lastTap = 0;
      function onDown(e) {
        var handle = e.target.closest && e.target.closest(".sb-resize");
        if (!handle || (e.pointerType === "mouse" && e.button !== 0)) return;
        e.preventDefault();
        if (e.pointerType !== "mouse") {
          // Double tap (dblclick doesn't fire for touch everywhere).
          if (e.timeStamp - lastTap < 350) { lastTap = 0; send(0); return; }
          lastTap = e.timeStamp;
        }
        var sb = handle.parentElement;
        var startX = e.clientX, startW = sb.getBoundingClientRect().width, width = startW;
        var pid = e.pointerId;
        try { handle.setPointerCapture(pid); } catch (err) { /* capture is optional */ }
        handle.classList.add("active");
        document.body.classList.add("resizing-sb");
        function onMove(ev) {
          if (ev.pointerId !== pid) return;
          width = Math.round(Math.min(max, Math.max(min, startW + ev.clientX - startX)));
          var a = app();
          if (a) a.style.setProperty("--sb-w", width + "px");
        }
        function onUp(ev) {
          if (ev.pointerId !== pid) return;
          document.removeEventListener("pointermove", onMove, true);
          document.removeEventListener("pointerup", onUp, true);
          document.removeEventListener("pointercancel", onUp, true);
          handle.classList.remove("active");
          document.body.classList.remove("resizing-sb");
          if (width !== startW) send(width);
        }
        document.addEventListener("pointermove", onMove, true);
        document.addEventListener("pointerup", onUp, true);
        document.addEventListener("pointercancel", onUp, true);
      }
      function onDbl(e) {
        if (e.target.closest && e.target.closest(".sb-resize")) send(0);
      }
      document.addEventListener("pointerdown", onDown, true);
      document.addEventListener("dblclick", onDbl, true);
      window.MarginApp._stopResize = function () {
        document.removeEventListener("pointerdown", onDown, true);
        document.removeEventListener("dblclick", onDbl, true);
      };
    },
    watchShortcuts: function (send) {
      if (window.MarginApp._stopShortcuts) window.MarginApp._stopShortcuts();
      function onKey(e) {
        if (!(e.metaKey || e.ctrlKey) || e.altKey) return;
        var k = (e.key || "").toLowerCase();
        if (e.shiftKey) {
          if (k === "n") { e.preventDefault(); send("notes"); }
          return;
        }
        if (k === "k") { e.preventDefault(); send("search"); }
        else if (k === "n") { e.preventDefault(); send("ask"); }
        else if (k === "\\") { e.preventDefault(); send(drawerMode() ? "drawer" : "sidebar"); }
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
    // Whether a box's text is cut off: an ellipsis that is in use (content wider than the
    // box) or a line clamp that hides lines. Takes plain numbers so it can be tested
    // without layout.
    truncated: function (m) {
      if (m.ellipsis && m.scrollWidth > m.clientWidth) return true;
      return !!m.clamped && m.scrollHeight > m.clientHeight + 1;
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

  // Tooltips. The macOS webview never shows native `title` tooltips, so elements with a
  // title get this one instead: after a short hover (or keyboard focus), below the
  // element (above it near the bottom edge), gone on click, key press or scroll. The
  // title moves to data-tip so the native one can't double up.
  (function tooltips() {
    var tip = null, timer = null, current = null;
    function el() {
      if (!tip) {
        tip = document.createElement("div");
        tip.className = "tooltip";
        tip.setAttribute("role", "tooltip");
        document.body.appendChild(tip);
      }
      return tip;
    }
    // The nearest element under the pointer that has a tooltip: its own title/data-tip,
    // or, for text cut off with an ellipsis or a line clamp, its full text.
    function target(node) {
      for (var n = node; n && n.nodeType === 1 && n !== document.body; n = n.parentElement) {
        if (n.hasAttribute("title")) {
          var text = n.getAttribute("title");
          n.removeAttribute("title");
          if (text) n.setAttribute("data-tip", text); else n.removeAttribute("data-tip");
        }
        if (n.getAttribute("data-tip")) return n;
        if (isTruncated(n)) return n;
      }
      return null;
    }
    // A focused control whose label is cut off (a sidebar row, a note in the list):
    // its first truncated descendant, so keyboard users get the full text too.
    function truncatedInside(node) {
      var kids = node.querySelectorAll ? node.querySelectorAll("*") : [];
      for (var i = 0; i < kids.length && i < 24; i++) {
        if (isTruncated(kids[i])) return kids[i];
      }
      return null;
    }
    function tipText(t) {
      return t.getAttribute("data-tip") || fullText(t);
    }
    function fullText(t) {
      return (t.textContent || "").replace(/\s+/g, " ").trim();
    }
    function isTruncated(n) {
      if (n.matches("input, textarea, select, [contenteditable]") || !fullText(n)) return false;
      var cs = window.getComputedStyle(n);
      var clamp = cs.webkitLineClamp || cs.getPropertyValue("-webkit-line-clamp");
      return window.MarginApp.truncated({
        ellipsis: cs.textOverflow === "ellipsis",
        clamped: !!clamp && clamp !== "none",
        scrollWidth: n.scrollWidth, clientWidth: n.clientWidth,
        scrollHeight: n.scrollHeight, clientHeight: n.clientHeight,
      });
    }
    function hide() {
      clearTimeout(timer);
      timer = null;
      current = null;
      if (tip) tip.classList.remove("show");
    }
    function show(t) {
      if (!document.body.contains(t)) return;
      var box = el();
      var text = tipText(t);
      if (!text) return;
      box.textContent = text;
      box.classList.add("show");
      var r = t.getBoundingClientRect(), b = box.getBoundingClientRect();
      var top = r.bottom + 6;
      if (top + b.height > window.innerHeight - 4) top = r.top - b.height - 6;
      var left = r.left + r.width / 2 - b.width / 2;
      left = Math.max(6, Math.min(left, window.innerWidth - b.width - 6));
      box.style.top = Math.round(top) + "px";
      box.style.left = Math.round(left) + "px";
    }
    function arm(t, delay) {
      if (t === current) return;
      hide();
      if (!t) return;
      current = t;
      timer = setTimeout(function () { show(t); }, delay);
    }
    // Only for a mouse or trackpad: a tap fires compatibility mouse events too, and a
    // tooltip it raised would stay up with nothing to move off.
    var lastPointer = "mouse";
    document.addEventListener("pointerdown", function (e) { lastPointer = e.pointerType; }, true);
    document.addEventListener("pointermove", function (e) { lastPointer = e.pointerType; }, true);
    function finePointer() {
      return lastPointer === "mouse" && !window.matchMedia("(hover: none)").matches;
    }
    document.addEventListener("mouseover", function (e) {
      if (finePointer()) arm(target(e.target), 500);
    }, true);
    document.addEventListener("mouseout", function (e) {
      if (current && !(e.relatedTarget && current.contains(e.relatedTarget))) hide();
    }, true);
    document.addEventListener("focusin", function (e) {
      if (finePointer() && e.target.matches && e.target.matches(":focus-visible")) arm(target(e.target) || truncatedInside(e.target), 300);
    }, true);
    document.addEventListener("focusout", hide, true);
    ["mousedown", "pointerdown", "touchstart", "keydown", "scroll", "wheel"].forEach(function (type) {
      document.addEventListener(type, hide, true);
    });
    window.addEventListener("blur", hide);
  })();

  // Below 1024px the sidebar is a drawer (main.css); ⌘\ opens and closes it there.
  function drawerMode() {
    return window.matchMedia("(max-width: 1023px)").matches;
  }

  // Long press = right click: rows with a context menu ([data-long-press]) get it from a
  // ~500 ms touch or pencil press that doesn't move. iOS fires no contextmenu for that;
  // Android does, so a native one cancels ours (or is dropped if ours came first), and the
  // tap that ends the press doesn't also activate the row.
  (function longPress() {
    var timer = null, start = null, firedAt = 0;
    function cancel() { clearTimeout(timer); timer = null; start = null; }
    document.addEventListener("pointerdown", function (e) {
      cancel();
      if (e.pointerType === "mouse") return;
      var row = e.target.closest && e.target.closest("[data-long-press]");
      if (!row) return;
      start = { x: e.clientX, y: e.clientY, target: e.target };
      timer = setTimeout(function () {
        var s = start;
        cancel();
        if (!s || !document.body.contains(s.target)) return;
        firedAt = Date.now();
        s.target.dispatchEvent(new MouseEvent("contextmenu", {
          bubbles: true, cancelable: true, clientX: s.x, clientY: s.y, button: 2,
        }));
      }, 500);
    }, true);
    document.addEventListener("pointermove", function (e) {
      if (start && Math.abs(e.clientX - start.x) + Math.abs(e.clientY - start.y) > 10) cancel();
    }, true);
    ["pointerup", "pointercancel", "scroll"].forEach(function (type) {
      document.addEventListener(type, cancel, true);
    });
    document.addEventListener("contextmenu", function (e) {
      if (!e.isTrusted) return;
      if (timer) { cancel(); return; }
      if (Date.now() - firedAt < 1000) { e.preventDefault(); e.stopPropagation(); }
    }, true);
    document.addEventListener("click", function (e) {
      if (Date.now() - firedAt < 1000) { firedAt = 0; e.preventDefault(); e.stopPropagation(); }
    }, true);
  })();

  // The on-screen keyboard's height over the layout viewport, as --kb on <html>, so
  // bottom-pinned panels and dialog heights stay above it (iOS only shrinks the visual
  // viewport for the keyboard; 100vh doesn't change).
  (function keyboardInset() {
    var vv = window.visualViewport;
    if (!vv) return;
    function update() {
      var kb = Math.max(0, Math.round(window.innerHeight - vv.height - vv.offsetTop));
      // Pinch zoom also shrinks the visual viewport; only count it at normal scale.
      if (vv.scale > 1.01) kb = 0;
      document.documentElement.style.setProperty("--kb", kb + "px");
    }
    vv.addEventListener("resize", update);
    vv.addEventListener("scroll", update);
    update();
  })();

  // Margin-note passages: <mark class="qmark" data-note=id> around the quoted text in a
  // lesson body, found in its text with whitespace collapsed (as the selection reported it).
  var quoteMarks = {};
  function unmark(root) {
    var marks = root.querySelectorAll("mark.qmark");
    for (var i = 0; i < marks.length; i++) {
      var m = marks[i], parent = m.parentNode;
      if (!parent) continue;
      while (m.firstChild) parent.insertBefore(m.firstChild, m);
      parent.removeChild(m);
      parent.normalize();
    }
  }
  // The body's text, whitespace collapsed, with each character's text node and offset.
  function flatText(root) {
    var walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
    var text = "", map = [], space = true, n;
    while ((n = walker.nextNode())) {
      var data = n.data;
      for (var i = 0; i < data.length; i++) {
        var c = data.charAt(i);
        if (/\s/.test(c)) {
          if (space) continue;
          space = true;
          text += " ";
        } else {
          space = false;
          text += c;
        }
        map.push({ node: n, offset: i });
      }
    }
    return { text: text, map: map };
  }
  function markOne(root, id, quote) {
    quote = (quote || "").replace(/\s+/g, " ").trim();
    if (!quote) return;
    var flat = flatText(root);
    var at = flat.text.indexOf(quote);
    if (at < 0) return;
    // Character ranges per text node, last node first so earlier offsets stay valid.
    var runs = [];
    for (var k = at; k < at + quote.length; k++) {
      var p = flat.map[k], last = runs[runs.length - 1];
      if (last && last.node === p.node) last.end = p.offset + 1;
      else runs.push({ node: p.node, start: p.offset, end: p.offset + 1 });
    }
    for (var r = runs.length - 1; r >= 0; r--) {
      var run = runs[r], node = run.node;
      // Typeset math is drawn from many tiny spans; leave it unwrapped.
      if (node.parentNode && node.parentNode.closest && node.parentNode.closest(".katex, .math")) continue;
      if (!/\S/.test(node.data.slice(run.start, run.end))) continue;
      var mid = node.splitText(run.start);
      mid.splitText(run.end - run.start);
      var mark = document.createElement("mark");
      mark.className = "qmark";
      mark.setAttribute("data-note", id);
      mid.parentNode.insertBefore(mark, mid);
      mark.appendChild(mid);
    }
  }
  function markQuotes(root, notes) {
    unmark(root);
    for (var i = 0; i < notes.length; i++) markOne(root, notes[i].id, notes[i].quote);
  }

  var sheet = null;
  function closeSheet() {
    if (sheet && sheet.parentNode) sheet.parentNode.removeChild(sheet);
    sheet = null;
  }
  function flash(note) {
    note.classList.remove("flash");
    void note.offsetWidth;
    note.classList.add("flash");
  }
  function goToNote(note) {
    closeSheet();
    note.scrollIntoView({ block: "center", behavior: "smooth" });
    flash(note);
  }
  document.addEventListener("click", function (e) {
    if (sheet && !sheet.contains(e.target)) closeSheet();
    var mark = e.target.closest && e.target.closest("mark.qmark");
    if (!mark) return;
    var sel = window.getSelection();
    if (sel && !sel.isCollapsed) return;
    var note = document.getElementById("note-" + mark.getAttribute("data-note"));
    if (!note) return;
    var doc = mark.closest(".doc");
    var beside = doc && note.getBoundingClientRect().left >= doc.getBoundingClientRect().right - 1;
    if (beside) {
      note.scrollIntoView({ block: "nearest", behavior: "smooth" });
      flash(note);
      return;
    }
    // Notes under the text: show this one in a sheet, with a way to it.
    sheet = document.createElement("div");
    sheet.className = "qsheet";
    sheet.setAttribute("role", "dialog");
    sheet.setAttribute("aria-label", "Margin note");
    var body = note.querySelector(".md");
    if (body) sheet.appendChild(body.cloneNode(true));
    var actions = document.createElement("div");
    actions.className = "row-actions";
    var close = document.createElement("button");
    close.className = "btn btn-ghost btn-sm";
    close.textContent = "Close";
    close.addEventListener("click", closeSheet);
    var go = document.createElement("button");
    go.className = "btn btn-secondary btn-sm";
    go.textContent = "Show in notes";
    go.addEventListener("click", function () { goToNote(note); });
    actions.appendChild(close);
    actions.appendChild(go);
    sheet.appendChild(actions);
    document.body.appendChild(sheet);
    go.focus();
  });
  document.addEventListener("keydown", function (e) {
    if (sheet && e.key === "Escape") closeSheet();
  }, true);
  window.addEventListener("popstate", closeSheet);

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
