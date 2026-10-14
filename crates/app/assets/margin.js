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
    // Global shortcuts (src/commands.rs). The modifier is Cmd on Apple platforms and Ctrl
    // elsewhere, never both, so the Cocoa text keys (Ctrl+K deletes to the end of the
    // line, Ctrl+N moves down…) keep working in macOS fields. Cmd/Ctrl+K search, +N Ask,
    // +Shift+N All notes, +\ sidebar, +, Settings, +[ and +] back and forward.
    watchShortcuts: function (send) {
      if (window.MarginApp._stopShortcuts) window.MarginApp._stopShortcuts();
      var apple = /Mac|iPhone|iPad|iPod/.test(navigator.platform || navigator.userAgent || "");
      window.MarginApp._sendCommand = send;
      function onKey(e) {
        if (e.altKey) return;
        if (apple ? (!e.metaKey || e.ctrlKey) : (!e.ctrlKey || e.metaKey)) return;
        var k = (e.key || "").toLowerCase();
        var name = null;
        if (e.shiftKey) {
          if (k === "n") name = "notes";
        } else if (k === "k") name = "search";
        else if (k === "n") name = "ask";
        else if (k === "\\") name = "sidebar";
        else if (k === ",") name = "settings";
        else if (k === "[") name = "back";
        else if (k === "]") name = "forward";
        if (!name) return;
        e.preventDefault();
        window.MarginApp.command(name);
      }
      document.addEventListener("keydown", onKey, true);
      window.MarginApp._stopShortcuts = function () {
        document.removeEventListener("keydown", onKey, true);
        window.MarginApp._sendCommand = null;
      };
    },
    // Runs a shortcut command, from a key press or the menu bar. Ignored while a modal
    // dialog is open (except the search palette, which commands close), and when the
    // same command just arrived by the other route (one press, one run).
    command: function (name) {
      var send = window.MarginApp._sendCommand;
      if (!send) return;
      if (document.querySelector('[aria-modal="true"]:not(.palette)')) return;
      var now = Date.now(), last = window.MarginApp._lastCommand;
      if (last && last.name === name && now - last.at < 150) return;
      window.MarginApp._lastCommand = { name: name, at: now };
      // Below 1024px the sidebar is a drawer, which the same command opens and closes.
      if (name === "sidebar" && drawerMode()) name = "drawer";
      send(name);
    },
    // Focus the first element matching `selector`, retrying for a few frames while it isn't
    // in the page or can't take focus yet (still hidden). With `finePointerOnly`, only when
    // there's a mouse or trackpad: on a touch screen, focusing a field pops up the
    // on-screen keyboard.
    focus: function (selector, finePointerOnly) {
      if (finePointerOnly && !window.matchMedia("(any-pointer: fine)").matches) return;
      var tries = 0;
      (function attempt() {
        var el = document.querySelector(selector);
        if (el) el.focus();
        if (el && document.activeElement === el) return;
        if (++tries < 30) requestAnimationFrame(attempt);
      })();
    },
    // A dialog opening keeps the element that had the focus, to give it back on close
    // (or, when that's gone, the dialog underneath, so Escape still reaches it).
    saveFocus: function () {
      window.MarginApp._savedFocus = document.activeElement;
    },
    restoreFocus: function () {
      var el = window.MarginApp._savedFocus;
      window.MarginApp._savedFocus = null;
      requestAnimationFrame(function () {
        if (el && el !== document.body && document.contains(el)) { el.focus(); return; }
        var dialog = document.querySelector('[aria-modal="true"][tabindex]');
        if (dialog) dialog.focus();
      });
    },
    // Focus `selector` inside the open drawer unless the focus is in the drawer already.
    focusDrawer: function (selector) {
      var sb = document.querySelector(".sb.open");
      if (sb && sb.contains(document.activeElement)) return;
      window.MarginApp.focus(selector);
    },
    // Calls send(true) when the window grows out of the narrow (drawer) layout.
    watchWide: function (send) {
      if (window.MarginApp._stopWide) window.MarginApp._stopWide();
      var mq = window.matchMedia("(min-width: 1024px)");
      function onChange() { if (mq.matches) send(true); }
      if (mq.addEventListener) mq.addEventListener("change", onChange); else mq.addListener(onChange);
      window.MarginApp._stopWide = function () {
        if (mq.removeEventListener) mq.removeEventListener("change", onChange); else mq.removeListener(onChange);
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
    // Clicks on the citation markers (button.cite[data-cite]) inside #`bodyId`: call
    // send(n). The body's HTML is replaced as it changes, so the listener sits on the
    // document. Only one watcher is active at a time.
    watchCitations: function (bodyId, send) {
      if (window.MarginApp._stopCitations) window.MarginApp._stopCitations();
      function onClick(e) {
        var b = e.target.closest && e.target.closest("button.cite[data-cite]");
        var body = document.getElementById(bodyId);
        if (!b || !body || !body.contains(b)) return;
        e.preventDefault();
        var n = parseInt(b.getAttribute("data-cite"), 10);
        if (n > 0) send(n);
      }
      document.addEventListener("click", onClick);
      window.MarginApp._stopCitations = function () {
        document.removeEventListener("click", onClick);
        window.MarginApp._stopCitations = null;
      };
    },
    stopCitations: function () {
      if (window.MarginApp._stopCitations) window.MarginApp._stopCitations();
    },
    // A modal dialog (#id) was opened or changed what it shows: remember what had focus
    // before it opened (to give it back on close), keep Tab inside it, and move focus in
    // if it isn't there yet.
    dialogOpened: function (id) {
      var d = document.getElementById(id);
      if (!d) return;
      var active = document.activeElement;
      if (!active || active === document.body) active = lastPressed;
      if (!d.__marginTrap) {
        // A fresh dialog element: this is the open, not a change of what it shows.
        d.__marginTrap = true;
        dialogReturn[id] = active && !d.contains(active) ? active : null;
        d.addEventListener("keydown", function (e) {
          if (e.key !== "Tab") return;
          var items = focusables(d);
          if (!items.length) { e.preventDefault(); d.focus(); return; }
          var first = items[0], last = items[items.length - 1], at = document.activeElement;
          if (e.shiftKey && (at === first || at === d)) { e.preventDefault(); last.focus(); }
          else if (!e.shiftKey && at === last) { e.preventDefault(); first.focus(); }
        });
      }
      if (!d.contains(document.activeElement)) d.focus();
    },
    // The dialog closed: focus what opened it, or `fallback` (a selector) when that
    // element is gone (e.g. re-rendered).
    dialogClosed: function (id, fallback) {
      var el = dialogReturn[id];
      delete dialogReturn[id];
      setTimeout(function () {
        if (el && document.body.contains(el)) { el.focus(); return; }
        var f = fallback && document.querySelector(fallback);
        if (f) f.focus();
      });
    },
    // Start the media element #id at `seconds` once it knows its duration; `play` starts
    // it too.
    seekMedia: function (id, seconds, play) {
      var m = document.getElementById(id);
      if (!m) return;
      function go() {
        var t = isFinite(m.duration) ? Math.min(seconds, Math.max(0, m.duration - 0.05)) : seconds;
        if (Math.abs(m.currentTime - t) > 0.25) m.currentTime = t;
        if (play) { var p = m.play(); if (p && p.catch) p.catch(function () {}); }
      }
      if (m.readyState >= 1) go(); else m.addEventListener("loadedmetadata", go, { once: true });
    },
    scrollIntoView: function (selector) {
      var el = document.querySelector(selector);
      if (el) el.scrollIntoView({ block: "nearest" });
    },
    // Scroll the list #id (a scroll box of its own) so its first item matching `selector`
    // sits a quarter of the way down, without moving anything around the list. Tries for
    // a few frames, since Rust renders asynchronously.
    revealIn: function (id, selector) {
      var tries = 0;
      (function attempt() {
        var box = document.getElementById(id);
        var el = box && box.querySelector(selector);
        if (!el) {
          if (tries++ < 10) requestAnimationFrame(attempt);
          return;
        }
        box.scrollTop = window.MarginApp.revealTop({
          scrollTop: box.scrollTop,
          boxTop: box.getBoundingClientRect().top,
          boxHeight: box.clientHeight,
          itemTop: el.getBoundingClientRect().top,
        });
      })();
    },
    // Keep a chat's transcript (#id, its own scroll box) at its end as the answer streams
    // in, unless the reader has scrolled up to read; it starts at the end. Tries for a
    // few frames, since Rust renders asynchronously.
    followChat: function (id) {
      var tries = 0;
      (function attempt() {
        var box = document.getElementById(id);
        if (!box) {
          if (tries++ < 10) requestAnimationFrame(attempt);
          return;
        }
        if (box.__follow) box.__follow.disconnect();
        box.__stick = true;
        box.scrollTop = box.scrollHeight;
        if (!box.__watched) {
          box.__watched = true;
          box.addEventListener("scroll", function () {
            box.__stick = window.MarginApp.nearEnd({
              scrollTop: box.scrollTop,
              scrollHeight: box.scrollHeight,
              clientHeight: box.clientHeight,
            });
          }, { passive: true });
        }
        var follow = new MutationObserver(function () {
          if (box.__stick) box.scrollTop = box.scrollHeight;
        });
        follow.observe(box, { childList: true, subtree: true, characterData: true });
        box.__follow = follow;
      })();
    },
    // Back to the end of the transcript #id and following it (after sending a message).
    chatToEnd: function (id) {
      var box = document.getElementById(id);
      if (!box) return;
      box.__stick = true;
      requestAnimationFrame(function () { box.scrollTop = box.scrollHeight; });
    },
    // Whether a scroll box is at (or within a line or two of) its end. Plain numbers, so
    // it can be tested without layout.
    nearEnd: function (m) {
      return m.scrollHeight - m.scrollTop - m.clientHeight < 80;
    },
    // Where revealIn scrolls to: the item a quarter of the box's height below its top.
    // Plain numbers, so it can be tested without layout.
    revealTop: function (m) {
      return Math.max(0, Math.round(m.scrollTop + (m.itemTop - m.boxTop) - m.boxHeight / 4));
    },
    // Where a long press's tooltip goes: centred above the finger at (x, y), clear of
    // it (fingertips cover ~40px); below it when there's no room above; always inside
    // the window. Plain numbers, so it can be tested without layout.
    touchTipPosition: function (m) {
      var gap = 44;
      var top = m.y - gap - m.height;
      if (top < 6) top = m.y + gap;
      top = Math.max(6, Math.min(top, m.viewHeight - m.height - 6));
      var left = m.x - m.width / 2;
      left = Math.max(6, Math.min(left, m.viewWidth - m.width - 6));
      return { top: Math.round(top), left: Math.round(left) };
    },
    // Whether a box's text is cut off: an ellipsis that is in use (content wider than the
    // box) or a line clamp that hides lines. Takes plain numbers so it can be tested
    // without layout.
    truncated: function (m) {
      if (m.ellipsis && m.scrollWidth > m.clientWidth) return true;
      return !!m.clamped && m.scrollHeight > m.clientHeight + 1;
    },
    // Focus (and show) the first element matching the selector once the pending DOM
    // update has landed; tries for a few frames, since Rust renders asynchronously.
    focusFirst: function (selector) {
      var tries = 0;
      (function attempt() {
        var el = document.querySelector(selector);
        if (el) {
          el.focus();
          el.scrollIntoView({ block: "nearest" });
        } else if (tries++ < 10) {
          requestAnimationFrame(attempt);
        }
      })();
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

  // Dialogs give focus back to what opened them. WebKit doesn't focus a button that is
  // clicked, so the last control pressed stands in for the focused element.
  var dialogReturn = {};
  var lastPressed = null;
  document.addEventListener("mousedown", function (e) {
    var t = e.target.closest && e.target.closest("button, a[href], [tabindex]");
    if (t) lastPressed = t;
  }, true);
  function focusables(root) {
    var all = root.querySelectorAll(
      "button:not([disabled]), a[href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), audio[controls], video[controls], [tabindex]:not([tabindex='-1'])"
    );
    return Array.prototype.filter.call(all, function (el) {
      return el.offsetWidth > 0 || el.offsetHeight > 0 || el.getClientRects().length > 0;
    });
  }

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

  // Segmented controls (role tablist or radiogroup) follow the ARIA keyboard pattern: a
  // single tab stop (the markup gives the selected option tabindex 0, the others -1), and
  // the arrow keys, Home and End move to another option and select it.
  document.addEventListener("keydown", function (e) {
    var item = e.target && e.target.closest ? e.target.closest('[role="tab"], [role="radio"]') : null;
    if (!item || e.altKey || e.ctrlKey || e.metaKey) return;
    var group = item.closest('[role="tablist"], [role="radiogroup"]');
    if (!group) return;
    var items = Array.prototype.filter.call(
      group.querySelectorAll('[role="tab"], [role="radio"]'),
      function (b) { return !b.disabled; }
    );
    var i = items.indexOf(item), next;
    if (i < 0) return;
    switch (e.key) {
      case "ArrowRight": case "ArrowDown": next = (i + 1) % items.length; break;
      case "ArrowLeft": case "ArrowUp": next = (i - 1 + items.length) % items.length; break;
      case "Home": next = 0; break;
      case "End": next = items.length - 1; break;
      default: return;
    }
    e.preventDefault();
    if (next === i) return;
    items[next].focus();
    items[next].click();
  });

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
    // Moves a title into data-tip (and aria-description) where the webview would draw
    // its own tooltip too; Apple's webviews keep it (see above).
    function adoptTitle(n) {
      if (keepTitle || !n.hasAttribute("title")) return;
      var text = n.getAttribute("title");
      n.removeAttribute("title");
      if (text) {
        n.setAttribute("data-tip", text);
        // (No author sets aria-description, so it always mirrors the latest title.)
        if (text !== n.getAttribute("aria-label")) n.setAttribute("aria-description", text);
        else n.removeAttribute("aria-description");
      } else {
        n.removeAttribute("data-tip");
        n.removeAttribute("aria-description");
      }
    }
    // The nearest element under the pointer that has a tooltip: its own title/data-tip,
    // or, for text cut off with an ellipsis or a line clamp, its full text.
    function target(node) {
      for (var n = node; n && n.nodeType === 1 && n !== document.body; n = n.parentElement) {
        adoptTitle(n);
        if (n.getAttribute("data-tip") || n.getAttribute("title")) return n;
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
      return t.getAttribute("data-tip") || t.getAttribute("title") || fullText(t);
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
      var text = tipText(t);
      if (!text) return;
      box.textContent = text;
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

    // Touch has no hover: a long press (~500 ms, not moving) on text that is cut off shows
    // its full text in the same tooltip, above the finger, until the next tap anywhere or
    // a scroll (the listeners above). Rows with an actions menu ([data-long-press]) open
    // that instead, headed by the full name. The tap that ends the press doesn't also
    // activate what's under it, and the system's text selection doesn't start.
    // <html data-input> says what the last press was ("touch" or "mouse"), which the
    // menus' headings follow.
    var press = null, pressTimer = null, revealedAt = 0;
    function cancelPress() { clearTimeout(pressTimer); pressTimer = null; press = null; }
    function truncatedAt(node) {
      for (var n = node; n && n.nodeType === 1 && n !== document.body; n = n.parentElement) {
        if (isTruncated(n)) return n;
      }
      return null;
    }
    function reveal(t, x, y) {
      if (!document.body.contains(t)) return;
      var box = el();
      box.textContent = fullText(t);
      box.classList.add("show");
      describe(t);
      current = t;
      var b = box.getBoundingClientRect();
      var at = window.MarginApp.touchTipPosition({
        x: x, y: y, width: b.width, height: b.height,
        viewWidth: window.innerWidth, viewHeight: window.innerHeight,
      });
      box.style.top = at.top + "px";
      box.style.left = at.left + "px";
      revealedAt = Date.now();
    }
    document.addEventListener("pointerdown", function (e) {
      cancelPress();
      document.documentElement.setAttribute("data-input", e.pointerType === "mouse" ? "mouse" : "touch");
      if (e.pointerType === "mouse") return;
      if (e.target.closest && e.target.closest("[data-long-press]")) return;
      var t = truncatedAt(e.target);
      if (!t) return;
      press = { x: e.clientX, y: e.clientY, target: t };
      pressTimer = setTimeout(function () {
        var p = press;
        cancelPress();
        if (p) reveal(p.target, p.x, p.y);
      }, 500);
    }, true);
    document.addEventListener("pointermove", function (e) {
      if (press && Math.abs(e.clientX - press.x) + Math.abs(e.clientY - press.y) > 10) cancelPress();
    }, true);
    ["pointerup", "pointercancel", "scroll"].forEach(function (type) {
      document.addEventListener(type, cancelPress, true);
    });
    function justRevealed() { return Date.now() - revealedAt < 1000; }
    document.addEventListener("click", function (e) {
      if (justRevealed()) { revealedAt = 0; e.preventDefault(); e.stopPropagation(); }
    }, true);
    ["contextmenu", "selectstart"].forEach(function (type) {
      document.addEventListener(type, function (e) {
        if (press || justRevealed()) e.preventDefault();
      }, true);
    });
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
