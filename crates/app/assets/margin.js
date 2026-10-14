// Small webview helpers for the Margin app. Rust calls these through document::eval.
(function () {
  "use strict";

  // The content is English; screen readers pick their voice from this. (Release builds
  // set it in the page itself; Dioxus's development page has none.)
  if (!document.documentElement.lang) document.documentElement.lang = "en";

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
        // HTML for the eye plus hidden MathML (with the TeX as its annotation) for
        // screen readers; KaTeX marks the visual tree aria-hidden.
        window.katex.render(tex, el, {
          displayMode: el.classList.contains("math-display"),
          throwOnError: false,
          output: "htmlAndMathml",
        });
        el.setAttribute("data-tex", tex);
        el.classList.add("katex-done");
      } catch (e) {
        el.classList.add("katex-done");
        el.textContent = tex;
      }
    }
  }

  // The selection's text as the reader sees it. Typeset math also holds hidden MathML
  // for screen readers; leave that out so a formula isn't quoted twice.
  function selectedText(sel, range) {
    var frag = range.cloneContents();
    var hidden = frag.querySelectorAll ? frag.querySelectorAll(".katex-mathml") : [];
    if (!hidden.length) return sel.toString();
    for (var i = 0; i < hidden.length; i++) hidden[i].parentNode.removeChild(hidden[i]);
    var box = document.createElement("div");
    box.style.cssText = "position:fixed;left:-10000px;top:0;width:600px;white-space:normal";
    box.appendChild(frag);
    document.body.appendChild(box);
    var text = box.innerText;
    document.body.removeChild(box);
    return text;
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
        var text = selectedText(sel, range).replace(/\s+/g, " ").trim();
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
    // Resizing the sidebar by its right edge. The width follows the pointer live (a CSS
    // variable on .app); the final width is sent to the app to save. Double-click resets.
    watchSidebarResize: function (min, max, send) {
      if (window.MarginApp._stopResize) window.MarginApp._stopResize();
      function app() { return document.querySelector(".app"); }
      function onDown(e) {
        var handle = e.target.closest && e.target.closest(".sb-resize");
        if (!handle || e.button !== 0) return;
        e.preventDefault();
        var sb = handle.parentElement;
        var startX = e.clientX, startW = sb.getBoundingClientRect().width, width = startW;
        handle.classList.add("active");
        document.body.classList.add("resizing-sb");
        function onMove(ev) {
          width = Math.round(Math.min(max, Math.max(min, startW + ev.clientX - startX)));
          var a = app();
          if (a) a.style.setProperty("--sb-w", width + "px");
        }
        function onUp() {
          document.removeEventListener("mousemove", onMove, true);
          document.removeEventListener("mouseup", onUp, true);
          handle.classList.remove("active");
          document.body.classList.remove("resizing-sb");
          if (width !== startW) send(width);
        }
        document.addEventListener("mousemove", onMove, true);
        document.addEventListener("mouseup", onUp, true);
      }
      function onDbl(e) {
        if (e.target.closest && e.target.closest(".sb-resize")) send(0);
      }
      document.addEventListener("mousedown", onDown, true);
      document.addEventListener("dblclick", onDbl, true);
      window.MarginApp._stopResize = function () {
        document.removeEventListener("mousedown", onDown, true);
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
    // Say `text` once through the app's polite live region (#announcer, in the shell).
    // Cleared first so the same message twice is still read.
    announce: function (text) {
      var region = document.getElementById("announcer");
      if (!region || !text) return;
      clearTimeout(window.MarginApp._announceTimer);
      region.textContent = "";
      window.MarginApp._announceTimer = setTimeout(function () { region.textContent = text; }, 100);
    },
    // The system text size as a factor of the default (iOS Dynamic Type through the
    // -apple-system-body font; 1 where the platform has no such setting).
    systemTextScale: function () {
      if (!(window.CSS && CSS.supports && CSS.supports("font", "-apple-system-body"))) return 1;
      var probe = document.createElement("span");
      probe.style.cssText = "position:absolute;visibility:hidden;font:-apple-system-body";
      probe.textContent = "x";
      document.body.appendChild(probe);
      var px = parseFloat(getComputedStyle(probe).fontSize) || 17;
      document.body.removeChild(probe);
      return px / 17;
    },
    // Record the page zoom for CSS (`--page-zoom` keeps the macOS title bar as tall as
    // the traffic lights). With `css` set (no native webview zoom, as on Android), the
    // zoom is applied with CSS instead.
    setPageZoom: function (zoom, css) {
      var root = document.documentElement;
      root.style.setProperty("--page-zoom", String(zoom));
      root.style.zoom = css && zoom !== 1 ? String(zoom) : "";
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
  // element (above it near the bottom edge), gone on click, key press or scroll.
  // Assistive technology keeps the text: Apple's webviews never draw native tooltips, so
  // there the title stays (VoiceOver reads it as help); elsewhere it moves to data-tip,
  // so the native one can't double up, and to aria-description. While the tip shows,
  // the element is also aria-describedby it.
  (function tooltips() {
    var TIP_ID = "margin-tooltip";
    var ua = navigator.userAgent || "";
    var keepTitle = /Macintosh|iPhone|iPad|iPod/.test(ua) && !/Android|Linux/.test(ua);
    var tip = null, timer = null, current = null, described = null;
    function el() {
      if (!tip) {
        tip = document.createElement("div");
        tip.className = "tooltip";
        tip.id = TIP_ID;
        tip.setAttribute("role", "tooltip");
        document.body.appendChild(tip);
      }
      return tip;
    }
    function tipText(t) {
      return t.getAttribute("data-tip") || t.getAttribute("title") || "";
    }
    function target(node) {
      var t = node && node.closest ? node.closest("[title], [data-tip]") : null;
      if (!t) return null;
      if (!keepTitle && t.hasAttribute("title")) {
        var text = t.getAttribute("title");
        t.removeAttribute("title");
        if (text) {
          t.setAttribute("data-tip", text);
          // (No author sets aria-description, so it always mirrors the latest title.)
          if (text !== t.getAttribute("aria-label")) t.setAttribute("aria-description", text);
          else t.removeAttribute("aria-description");
        } else {
          t.removeAttribute("data-tip");
        }
      }
      return tipText(t) ? t : null;
    }
    function describe(t) {
      undescribe();
      var ids = (t.getAttribute("aria-describedby") || "").split(/\s+/).filter(Boolean);
      if (ids.indexOf(TIP_ID) < 0) ids.push(TIP_ID);
      t.setAttribute("aria-describedby", ids.join(" "));
      described = t;
    }
    function undescribe() {
      if (!described) return;
      var ids = (described.getAttribute("aria-describedby") || "").split(/\s+/).filter(function (id) {
        return id && id !== TIP_ID;
      });
      if (ids.length) described.setAttribute("aria-describedby", ids.join(" "));
      else described.removeAttribute("aria-describedby");
      described = null;
    }
    function hide() {
      clearTimeout(timer);
      timer = null;
      current = null;
      undescribe();
      if (tip) tip.classList.remove("show");
    }
    function show(t) {
      if (!document.body.contains(t)) return;
      var box = el();
      box.textContent = tipText(t);
      box.classList.add("show");
      describe(t);
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
    document.addEventListener("mouseover", function (e) { arm(target(e.target), 500); }, true);
    document.addEventListener("mouseout", function (e) {
      if (current && !(e.relatedTarget && current.contains(e.relatedTarget))) hide();
    }, true);
    document.addEventListener("focusin", function (e) {
      if (e.target.matches && e.target.matches(":focus-visible")) arm(target(e.target), 300);
    }, true);
    document.addEventListener("focusout", hide, true);
    ["mousedown", "keydown", "scroll", "wheel"].forEach(function (type) {
      document.addEventListener(type, hide, true);
    });
    window.addEventListener("blur", hide);
  })();

  // Keyboard and focus for the layers that open over the page (dialogs and menus) and
  // for the sidebar tree.
  //  - A layer that opens without focus inside it gets focus (Rust focuses the intended
  //    control on mount; this is the backstop). Tab and Shift+Tab stay inside the top
  //    dialog; Tab closes a menu. Escape closes the top layer even when focus is
  //    elsewhere. When a layer closes and focus fell to the page, it goes back to what
  //    opened the layer (a layer opened as another closes, like Delete… opening a
  //    confirmation, inherits that target).
  //  - Menus: Up/Down (wrapping), Home and End move between items.
  //  - Tree: one tab stop (roving tabindex); Up/Down move between visible rows, Right
  //    expands or enters, Left collapses or goes to the parent, Home/End, and Shift+F10
  //    or the Menu key opens the row's ⋯ menu.
  (function focusManagement() {
    var LAYER = '[role="dialog"], [role="alertdialog"], [role="menu"]';
    var FOCUSABLE = 'a[href], button:not([disabled]), input:not([disabled]):not([type="hidden"]), ' +
      'select:not([disabled]), textarea:not([disabled]), video[controls], [contenteditable="true"], ' +
      '[tabindex]:not([tabindex="-1"])';
    var layers = [];
    var lastPointer = null, lastPointerAt = 0;
    var pending = null; // { target, at, timer }: focus to give back once a layer closed

    function visible(el) {
      return !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
    }
    function focusables(root) {
      return Array.prototype.filter.call(root.querySelectorAll(FOCUSABLE), visible);
    }
    function pageHasNoFocus() {
      var a = document.activeElement;
      return !a || a === document.body || a === document.documentElement;
    }
    function focusEl(el) {
      if (el && el.focus) el.focus({ preventScroll: true });
    }
    function byLabel(label) {
      return label ? document.querySelector('[aria-label="' + label.replace(/["\\]/g, "\\$&") + '"]') : null;
    }
    function isMenu(layer) { return layer.el.getAttribute("role") === "menu"; }
    function topLayer() {
      // A layer can go before the observer reports it (closed by this very key press).
      while (layers.length && !layers[layers.length - 1].el.isConnected) closed(layers.pop());
      return layers.length ? layers[layers.length - 1] : null;
    }
    function menuItems(menu) {
      return Array.prototype.filter.call(menu.querySelectorAll('[role="menuitem"]'), function (b) {
        return !b.disabled && visible(b);
      });
    }

    // What focus returns to when a layer opening now closes.
    function opener() {
      if (pending && Date.now() - pending.at < 1500) {
        // Opened as another layer closed: take over its target.
        clearTimeout(pending.timer);
        var inherited = pending.target;
        pending = null;
        if (pageHasNoFocus()) return inherited;
      }
      if (!pageHasNoFocus()) return document.activeElement;
      // Clicking a button doesn't focus it in WebKit; use what the pointer pressed.
      if (lastPointer && lastPointer.isConnected && Date.now() - lastPointerAt < 1500) return lastPointer;
      return null;
    }

    function opened(el) {
      for (var i = 0; i < layers.length; i++) if (layers[i].el === el) return;
      var target = opener();
      var label = target && target.getAttribute ? target.getAttribute("aria-label") : null;
      var layer = { el: el, target: target, label: label };
      layers.push(layer);
      // Backstop for the initial focus.
      setTimeout(function () {
        if (!el.isConnected || el.contains(document.activeElement)) return;
        if (layers[layers.length - 1] !== layer) return;
        if (isMenu(layer)) { focusEl(menuItems(el)[0]); return; }
        var first = el.querySelector("[data-autofocus]") || focusables(el)[0];
        if (first) { focusEl(first); return; }
        if (!el.hasAttribute("tabindex")) el.setAttribute("tabindex", "-1");
        focusEl(el);
      }, 150);
    }

    function closed(layer) {
      var target = layer.target && layer.target.isConnected ? layer.target : byLabel(layer.label);
      if (!target) return;
      if (pending) clearTimeout(pending.timer);
      // Wait a moment: the action that closed the layer may move focus itself (Rename
      // focuses its field once mounted). Only a page left without focus gets it back.
      pending = {
        target: target,
        at: Date.now(),
        timer: setTimeout(function () {
          pending = null;
          if (!pageHasNoFocus()) return;
          focusEl(target.isConnected ? target : byLabel(layer.label));
        }, 120),
      };
    }

    function scan(records) {
      for (var j = layers.length - 1; j >= 0; j--) {
        if (!layers[j].el.isConnected) closed(layers.splice(j, 1)[0]);
      }
      for (var k = 0; k < records.length; k++) {
        var added = records[k].addedNodes;
        for (var n = 0; n < added.length; n++) {
          var node = added[n];
          if (node.nodeType !== 1 || !node.isConnected) continue;
          if (node.matches(LAYER)) opened(node);
          var inner = node.querySelectorAll(LAYER);
          for (var m = 0; m < inner.length; m++) opened(inner[m]);
        }
      }
      scheduleTreeSync();
    }

    function closeLayer(layer) {
      if (isMenu(layer)) {
        // Menus close from their scrim, the element just before them.
        var scrim = layer.el.previousElementSibling;
        if (scrim && scrim.classList.contains("pop-scrim")) { scrim.click(); return; }
      }
      // Dialogs handle Escape themselves (on the dialog or its scrim).
      layer.el.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", code: "Escape", bubbles: true, cancelable: true }));
    }

    function onKeyDown(e) {
      var layer = topLayer();
      if (layer) {
        var inside = layer.el.contains(e.target);
        if (e.key === "Tab") {
          if (isMenu(layer)) {
            e.preventDefault();
            closeLayer(layer);
            return;
          }
          var list = focusables(layer.el);
          if (!list.length) { e.preventDefault(); focusEl(layer.el); return; }
          var first = list[0], last = list[list.length - 1];
          var active = document.activeElement;
          if (!inside || !layer.el.contains(active)) {
            e.preventDefault();
            focusEl(e.shiftKey ? last : first);
          } else if (e.shiftKey && (active === first || active === layer.el)) {
            e.preventDefault();
            focusEl(last);
          } else if (!e.shiftKey && active === last) {
            e.preventDefault();
            focusEl(first);
          }
          return;
        }
        if (e.key === "Escape" && !inside) {
          e.preventDefault();
          closeLayer(layer);
          return;
        }
        if (isMenu(layer) && !e.metaKey && !e.ctrlKey && !e.altKey) {
          var items = menuItems(layer.el);
          if (!items.length) return;
          var at = items.indexOf(document.activeElement);
          var next = -1;
          if (e.key === "ArrowDown") next = at < 0 ? 0 : (at + 1) % items.length;
          else if (e.key === "ArrowUp") next = at < 0 ? items.length - 1 : (at - 1 + items.length) % items.length;
          else if (e.key === "Home") next = 0;
          else if (e.key === "End") next = items.length - 1;
          if (next >= 0) { e.preventDefault(); focusEl(items[next]); }
          return;
        }
        if (!inside) return;
      }
      treeKey(e);
    }

    // ----- the sidebar tree -----
    function treeOf(node) {
      return node && node.closest ? node.closest('[role="tree"]') : null;
    }
    function itemOf(node) {
      return node && node.closest ? node.closest('li[role="treeitem"]') : null;
    }
    function rowOfItem(li) {
      return li ? li.querySelector(":scope > .tree-row > .row, :scope > .row") : null;
    }
    function rowsOf(tree) {
      return Array.prototype.filter.call(
        tree.querySelectorAll('li[role="treeitem"] > .tree-row > .row, li[role="treeitem"] > .row'),
        visible
      );
    }

    var syncQueued = false;
    function scheduleTreeSync() {
      if (syncQueued) return;
      syncQueued = true;
      // A timer, not requestAnimationFrame: that one pauses while the window is hidden.
      setTimeout(function () {
        syncQueued = false;
        var trees = document.querySelectorAll('[role="tree"]');
        for (var i = 0; i < trees.length; i++) syncTree(trees[i]);
      }, 0);
    }
    // One tab stop: the current row (the last focused, else the open page's, else the
    // first). ⋯ buttons are reached with Shift+F10 or the pointer.
    function syncTree(tree) {
      var rows = rowsOf(tree);
      if (!rows.length) return;
      var current = tree.__marginRow;
      if (!current || rows.indexOf(current) < 0) {
        current = tree.querySelector('.row[aria-current="true"]');
        if (rows.indexOf(current) < 0) current = rows[0];
        tree.__marginRow = current;
      }
      for (var i = 0; i < rows.length; i++) {
        var want = rows[i] === current ? "0" : "-1";
        if (rows[i].getAttribute("tabindex") !== want) rows[i].setAttribute("tabindex", want);
      }
      var more = tree.querySelectorAll(".tree-row > .more");
      for (var j = 0; j < more.length; j++) {
        if (more[j].getAttribute("tabindex") !== "-1") more[j].setAttribute("tabindex", "-1");
      }
    }
    function moveTo(tree, row) {
      if (!row) return;
      tree.__marginRow = row;
      syncTree(tree);
      focusEl(row);
      row.scrollIntoView({ block: "nearest" });
    }

    function treeKey(e) {
      var tree = treeOf(e.target);
      if (!tree || e.metaKey || e.ctrlKey || e.altKey) return;
      var tag = e.target.tagName;
      if (tag === "INPUT" || tag === "TEXTAREA" || e.target.closest('[role="menu"]')) return;
      var li = itemOf(e.target);
      var row = rowOfItem(li);
      if (!row) return;
      var rows = rowsOf(tree);
      var at = rows.indexOf(row);
      var expanded = li.getAttribute("aria-expanded");
      switch (e.key) {
        case "ArrowDown":
          e.preventDefault();
          moveTo(tree, rows[Math.min(at + 1, rows.length - 1)]);
          break;
        case "ArrowUp":
          e.preventDefault();
          moveTo(tree, rows[Math.max(at - 1, 0)]);
          break;
        case "Home":
          e.preventDefault();
          moveTo(tree, rows[0]);
          break;
        case "End":
          e.preventDefault();
          moveTo(tree, rows[rows.length - 1]);
          break;
        case "ArrowRight":
          e.preventDefault();
          if (expanded === "false") {
            row.click();
            moveTo(tree, row);
          } else if (expanded === "true") {
            moveTo(tree, rowOfItem(li.querySelector(':scope > [role="group"] > li[role="treeitem"]')));
          }
          break;
        case "ArrowLeft":
          e.preventDefault();
          if (expanded === "true") {
            row.click();
            moveTo(tree, row);
          } else {
            moveTo(tree, rowOfItem(itemOf(li.parentElement)));
          }
          break;
        case "F10":
        case "ContextMenu":
          if (e.key === "F10" && !e.shiftKey) return;
          var more = li.querySelector(":scope > .tree-row > .more");
          if (more) {
            e.preventDefault();
            tree.__marginRow = row;
            focusEl(row);
            more.click();
          }
          break;
      }
    }

    document.addEventListener("keydown", onKeyDown);
    document.addEventListener("pointerdown", function (e) {
      var t = e.target && e.target.closest ? e.target.closest(FOCUSABLE + ', [tabindex="-1"]') : null;
      if (t) { lastPointer = t; lastPointerAt = Date.now(); }
    }, true);
    document.addEventListener("focusin", function (e) {
      var tree = treeOf(e.target);
      if (!tree) return;
      var row = rowOfItem(itemOf(e.target));
      if (row && row !== tree.__marginRow) { tree.__marginRow = row; syncTree(tree); }
    });
    function start() {
      new MutationObserver(scan).observe(document.body, { childList: true, subtree: true });
      var present = document.querySelectorAll(LAYER);
      for (var i = 0; i < present.length; i++) opened(present[i]);
      scheduleTreeSync();
    }
    if (document.body) start(); else document.addEventListener("DOMContentLoaded", start);
  })();

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
