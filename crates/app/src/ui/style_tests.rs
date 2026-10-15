//! Checks on `assets/main.css` and `assets/margin.js` that keep text readable: the color
//! tokens used for text meet WCAG AA contrast in both themes, the two copies of the dark
//! theme agree, and the tooltip's truncation test behaves.

const CSS: &str = include_str!("../../assets/main.css");
const JS: &str = include_str!("../../assets/margin.js");

/// The declarations of the first rule whose selector line starts with `selector`.
fn block(selector: &str) -> &'static str {
    let start = CSS
        .lines()
        .scan(0usize, |pos, line| {
            let at = *pos;
            *pos += line.len() + 1;
            Some((at, line))
        })
        .find(|(_, line)| line.trim_start().starts_with(selector))
        .map(|(at, _)| at)
        .unwrap_or_else(|| panic!("no rule {selector}"));
    let open = start + CSS[start..].find('{').unwrap() + 1;
    let close = open + CSS[open..].find('}').unwrap();
    &CSS[open..close]
}

fn token(block: &str, name: &str) -> String {
    let key = format!("{name}:");
    block
        .split(';')
        .filter_map(|decl| {
            // Drop comments in front of a declaration.
            let decl = decl.rsplit("*/").next().unwrap().trim();
            decl.strip_prefix(&key).map(|v| v.trim().to_string())
        })
        .next()
        .unwrap_or_else(|| panic!("no {name}"))
}

/// An sRGB color with alpha, from `#rrggbb` or `rgba(r,g,b,a)`.
fn parse(color: &str) -> [f64; 4] {
    if let Some(hex) = color.strip_prefix('#') {
        let c = |i: usize| f64::from(u8::from_str_radix(&hex[i..i + 2], 16).unwrap());
        return [c(0), c(2), c(4), 1.0];
    }
    let inner = color
        .strip_prefix("rgba(")
        .and_then(|c| c.strip_suffix(')'))
        .unwrap();
    let v: Vec<f64> = inner
        .split(',')
        .map(|n| n.trim().parse().unwrap())
        .collect();
    [v[0], v[1], v[2], v[3]]
}

fn over(fg: [f64; 4], bg: [f64; 4]) -> [f64; 4] {
    let a = fg[3];
    let mix = |i: usize| fg[i] * a + bg[i] * (1.0 - a);
    [mix(0), mix(1), mix(2), 1.0]
}

fn luminance(c: [f64; 4]) -> f64 {
    let ch = |v: f64| {
        let s = v / 255.0;
        if s <= 0.040_45 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * ch(c[0]) + 0.7152 * ch(c[1]) + 0.0722 * ch(c[2])
}

fn contrast(fg: &str, bg: &str) -> f64 {
    let bg = parse(bg);
    let (a, b) = (luminance(over(parse(fg), bg)), luminance(bg));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

fn themes() -> [(&'static str, &'static str); 2] {
    [
        ("light", block(":root {")),
        ("dark", block(":root[data-theme=\"dark\"]")),
    ]
}

#[test]
fn contrast_math_matches_wcag() {
    assert!((contrast("#000000", "#ffffff") - 21.0).abs() < 1e-9);
    assert!((contrast("#777777", "#ffffff") - 4.48).abs() < 0.01);
}

#[test]
fn text_tokens_meet_aa_on_every_surface() {
    for (theme, b) in themes() {
        let surfaces = ["--win", "--sidebar", "--surface"].map(|s| token(b, s));
        for text in ["--fg-2", "--fg-3", "--link", "--danger"] {
            let fg = token(b, text);
            for bg in &surfaces {
                let ratio = contrast(&fg, bg);
                assert!(ratio >= 4.5, "{theme} {text} {fg} on {bg}: {ratio:.2}");
            }
        }
        // Error text sits on its own tint too.
        let tint = over(parse(&token(b, "--danger-soft")), parse(&token(b, "--win")));
        let tint = format!(
            "#{:02x}{:02x}{:02x}",
            tint[0] as u8, tint[1] as u8, tint[2] as u8
        );
        let ratio = contrast(&token(b, "--danger"), &tint);
        assert!(
            ratio >= 4.5,
            "{theme} --danger on --danger-soft: {ratio:.2}"
        );
        // The filled Delete button's label.
        let ratio = contrast(&token(b, "--on-danger"), &token(b, "--danger-bg"));
        assert!(
            ratio >= 4.5,
            "{theme} --on-danger on --danger-bg: {ratio:.2}"
        );
    }
}

#[test]
fn dark_theme_copies_agree() {
    let forced = block(":root[data-theme=\"dark\"]");
    let system = block(":root:not([data-theme=\"light\"])");
    for name in [
        "--fg-2",
        "--fg-3",
        "--fg-4",
        "--link",
        "--danger",
        "--danger-soft",
        "--danger-bg",
        "--on-danger",
    ] {
        assert_eq!(token(forced, name), token(system, name), "{name}");
    }
}

#[test]
fn placeholder_and_danger_button_use_tokens() {
    assert!(CSS.contains("::placeholder { color: var(--fg-3); opacity: 1; }"));
    assert!(CSS.contains(".btn-danger { background: var(--danger-bg); color: var(--on-danger); }"));
}

/// A stand-in for the browser globals margin.js touches while it loads: every property
/// is another stand-in (callable, constructible), and assignments stick.
const DOM_STUB: &str = r#"function stub(){const store={};return new Proxy(function(){},{get(t,k){if(k in store)return store[k];if(k===Symbol.toPrimitive)return ()=>'';if(k==='then'||typeof k==='symbol')return undefined;return store[k]=stub();},set(t,k,v){store[k]=v;return true},apply(){return stub()},construct(){return stub()}})}
global.window=stub();global.document=stub();global.navigator={userAgent:'',platform:''};
for (const k of ['MutationObserver','ResizeObserver','MouseEvent','KeyboardEvent','Event','CustomEvent','CSS','getComputedStyle','requestAnimationFrame','cancelAnimationFrame','location','history','localStorage','matchMedia','visualViewport','katex','renderMathInElement']) global[k]=stub();"#;

/// Runs `MarginApp.truncated` under Node when it's installed (skipped otherwise).
#[test]
fn truncation_check_for_tooltips() {
    let script = format!(
        "{DOM_STUB}\n{JS}\n\
         const t=window.MarginApp.truncated;\n\
         const cases=[\
           [{{ellipsis:true,scrollWidth:120,clientWidth:100}},true],\
           [{{ellipsis:true,scrollWidth:100,clientWidth:100}},false],\
           [{{ellipsis:false,scrollWidth:120,clientWidth:100}},false],\
           [{{clamped:true,scrollHeight:80,clientHeight:60}},true],\
           [{{clamped:true,scrollHeight:61,clientHeight:60}},false],\
           [{{clamped:false,scrollHeight:80,clientHeight:60}},false]];\n\
         for (const [m,want] of cases) if (t(m)!==want) {{ console.error(JSON.stringify(m)); process.exit(1); }}"
    );
    let out = match std::process::Command::new("node")
        .arg("-e")
        .arg(&script)
        .output()
    {
        Ok(out) => out,
        Err(_) => return,
    };
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Runs a check of margin.js's pure helpers under Node when it's installed (skipped
/// otherwise): `body` exits non-zero with a message on failure. `eq(got, want, what)`
/// is defined for it.
fn run_js(body: &str) {
    let script = format!(
        "{DOM_STUB}\n{JS}\nconst M=window.MarginApp;\n\
         const eq=(a,b,m)=>{{const x=JSON.stringify(a),y=JSON.stringify(b);if(x!==y){{console.error(m,x,y);process.exit(1)}}}};\n\
         {body}"
    );
    let out = match std::process::Command::new("node")
        .arg("-e")
        .arg(&script)
        .output()
    {
        Ok(out) => out,
        Err(_) => return,
    };
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn long_press_tips_sit_above_the_finger_inside_the_window() {
    run_js(
        "const v={width:200,height:40,viewWidth:390,viewHeight:844};\n\
         eq(M.touchTipPosition({...v,x:195,y:400}),{top:316,left:95},'above, centred');\n\
         eq(M.touchTipPosition({...v,x:20,y:400}),{top:316,left:6},'kept off the left edge');\n\
         eq(M.touchTipPosition({...v,x:385,y:400}),{top:316,left:184},'kept off the right edge');\n\
         eq(M.touchTipPosition({...v,x:195,y:60}),{top:104,left:95},'below when no room above');\n\
         eq(M.touchTipPosition({...v,x:195,y:840,height:40}),{top:756,left:95},'near the bottom');",
    );
}

#[test]
fn transcript_reveal_puts_the_line_a_quarter_down() {
    run_js(
        "eq(M.revealTop({scrollTop:0,boxTop:100,boxHeight:400,itemTop:700}),500,'below');\n\
         eq(M.revealTop({scrollTop:300,boxTop:100,boxHeight:400,itemTop:150}),250,'above');\n\
         eq(M.revealTop({scrollTop:0,boxTop:100,boxHeight:400,itemTop:120}),0,'near the top');",
    );
}

#[test]
fn chats_follow_their_end_until_scrolled_up() {
    run_js(
        "eq(M.nearEnd({scrollTop:600,scrollHeight:1000,clientHeight:400}),true,'at the end');\n\
         eq(M.nearEnd({scrollTop:540,scrollHeight:1000,clientHeight:400}),true,'a line above it');\n\
         eq(M.nearEnd({scrollTop:200,scrollHeight:1000,clientHeight:400}),false,'scrolled up');\n\
         eq(M.nearEnd({scrollTop:0,scrollHeight:300,clientHeight:400}),true,'shorter than the box');",
    );
}

/// The chat's own text colors are tokens that pass the contrast checks above, and its
/// controls follow the app's touch sizes.
#[test]
fn chat_styles_use_checked_tokens() {
    for (rule, color) in [
        (".chat-action {", "var(--fg-3)"),
        (".chat-cut {", "var(--fg-3)"),
        (".msg-user small {", "var(--fg-3)"),
        (".tool-state {", "var(--fg-3)"),
        (".tool-row.bad .tool-state {", "var(--danger)"),
        (".pending small {", "var(--fg-3)"),
    ] {
        assert_eq!(token(block(rule), "color"), color, "{rule}");
    }
    // Rows open with the whole line on touch screens.
    let start = CSS.find("@media (pointer: coarse) {").unwrap();
    let touch = &CSS[start..];
    assert!(touch.contains(".tool-head { min-height: 44px; }"));
}

/// iOS zooms into any field under 16px when it gets focus, whatever the pointer, and
/// stays zoomed (the page then runs off the right edge): on iOS WebKit every field class
/// is 16px, not only with a coarse pointer.
#[test]
fn fields_are_16px_on_ios_whatever_the_pointer() {
    let start = CSS
        .find("@supports (-webkit-touch-callout: none) {")
        .expect("no iOS-only @supports block");
    let inner = &CSS[start..];
    let inner = &inner[inner.find('{').unwrap() + 1..];
    let rule = &inner[..inner.find('}').unwrap()];
    let (selectors, decls) = rule
        .split_once('{')
        .expect("no rule in the @supports block");
    for class in [
        ".input",
        ".textarea",
        ".select-input",
        ".tree-input",
        "select.chip",
    ] {
        assert!(
            selectors.split(',').any(|s| s.trim() == class),
            "{class} isn't 16px on iOS"
        );
    }
    assert_eq!(token(decls, "font-size"), "16px");
}

/// Nothing in a chat scrolls the transcript sideways: the log is one column exactly as
/// wide as the pane (an `auto` track grows to its widest item's min-content, so a long
/// reasoning headline or a code block once widened every row past the window), long
/// text wraps, and only code blocks and tables scroll, inside themselves.
#[test]
fn chat_transcript_never_scrolls_sideways() {
    let log = block(".chat-log {");
    assert_eq!(token(log, "display"), "grid");
    assert_eq!(token(log, "grid-template-columns"), "minmax(0, 1fr)");
    let items = block(".chat-log > * {");
    assert_eq!(token(items, "min-width"), "0");
    assert_eq!(token(items, "max-width"), "100%");

    // Reasoning: the headline is one line cut with an ellipsis; the summary wraps.
    let summary = block(".reasoning > summary {");
    assert_eq!(token(summary, "max-width"), "100%");
    assert_eq!(token(summary, "min-width"), "0");
    let headline = block(".reasoning > summary .faint {");
    for (name, value) in [
        ("min-width", "0"),
        ("overflow", "hidden"),
        ("text-overflow", "ellipsis"),
        ("white-space", "nowrap"),
    ] {
        assert_eq!(token(headline, name), value, "headline {name}");
    }
    let body = block(".reasoning-md {");
    assert_eq!(token(body, "min-width"), "0");
    assert_eq!(token(body, "overflow-wrap"), "anywhere");

    // Markdown (answers and reasoning): text wraps; code blocks and tables scroll.
    assert_eq!(token(block(".md {"), "overflow-wrap"), "anywhere");
    for rule in [".md pre {", ".md table {"] {
        assert_eq!(token(block(rule), "overflow-x"), "auto", "{rule}");
        assert_eq!(token(block(rule), "max-width"), "100%", "{rule}");
    }
    // Tool output wraps inside its box; tool and card rows can shrink.
    let output = block(".tool-body .code {");
    assert_eq!(token(output, "white-space"), "pre-wrap");
    assert_eq!(token(output, "overflow-wrap"), "anywhere");
    assert_eq!(token(block(".tool-label {"), "min-width"), "0");
    assert_eq!(token(block(".tool-row {"), "min-width"), "0");
    assert_eq!(token(block(".chat-card {"), "min-width"), "0");
    assert_eq!(
        token(block(".msg-user .bubble {"), "overflow-wrap"),
        "anywhere"
    );
}

/// The text of every `@media (<query>) { … }` block with that exact query.
fn media_blocks(query: &str) -> Vec<&'static str> {
    let head = format!("@media ({query}) {{");
    CSS.match_indices(&head)
        .map(|(at, _)| {
            let body = &CSS[at + head.len()..];
            // Blocks close with a `}` at the start of a line.
            let end = body.find("\n}").unwrap_or(body.len());
            &body[..end]
        })
        .collect()
}

/// L2 at phone width: the Audience control comes up from the bottom as a sheet with
/// the room to scroll its pickers, an audit row is a card of lines, a summary line
/// isn't indented; at desktop width the audit log is a five-column table. Touch gets
/// 44pt targets.
#[test]
fn audiences_and_the_audit_log_fit_phones_and_computers() {
    let phone = media_blocks("max-width: 719px").join("\n");
    assert!(
        phone.contains(".modal-scrim.sheet { place-items: end stretch;"),
        "the sheet"
    );
    assert!(phone.contains(".modal-scrim.sheet > .modal { width: 100%; max-height: 100%;"));
    assert!(phone.contains(".audit-row { grid-template-columns: minmax(0, 1fr);"));
    assert!(phone.contains(".audit-head { display: none; }"));
    assert!(phone.contains(".aud-scroll { max-height: 40vh; }"));
    assert!(phone.contains(".aud-under { padding-left: 0; }"));
    // Desktop: the table, and pickers that scroll inside the dialog.
    assert_eq!(
        token(block(".audit-row {"), "grid-template-columns"),
        "136px 128px 120px minmax(0, 1fr) minmax(0, 1.4fr)"
    );
    assert_eq!(token(block(".aud-scroll {"), "overflow"), "auto");
    assert_eq!(token(block(".org-dialog {"), "overflow"), "auto");
    assert_eq!(token(block(".aud-line {"), "flex-wrap"), "wrap");
    let touch = media_blocks("pointer: coarse").join("\n");
    assert!(touch.contains(
        ".aud-line .link-btn, .aud-deny > summary, .agent-docs > summary { min-height: 44px; }"
    ));
    assert!(touch.contains(".aud-dialog .check-row { min-height: 44px; }"));
}

/// A title that only repeats an element's own text (a tool row's label) shows as a
/// tooltip only while that text is cut off.
#[test]
fn repeated_titles_are_not_tooltips() {
    run_js(
        "eq(M.repeatsText('Listing the topics','Listing the topics'),true,'same');\n\
         eq(M.repeatsText(' Listing  the\\ntopics ','Listing the topics'),true,'spacing');\n\
         eq(M.repeatsText('Engineering / Control','Control'),false,'a path');\n\
         eq(M.repeatsText('Stop the answer',''),false,'an icon button');",
    );
}
