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
