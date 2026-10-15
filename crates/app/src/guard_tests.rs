//! No signal or `RefCell` guard is held across an `.await`.
//!
//! `sig.peek()`, `.read()` and `.write()` return a guard that lives until the end of the
//! statement it's a temporary of: `save(x.peek().clone()).await` holds `x`'s borrow for
//! the whole wait, so anything that writes `x` meanwhile (a keystroke's `oninput`)
//! panics with "already borrowed", on the main thread, which ends the app. The same goes
//! for a `match` / `if let` / `while let` / `for` header's temporaries, which last
//! through its block. Bind the value first: `let v = x.peek().clone(); save(v).await`.
//!
//! This scans the crate's source for those shapes. It's textual (strings and comments
//! are blanked out first) and errs on the side of flagging.

use std::path::{Path, PathBuf};

const GUARDS: &[&str] = &[
    ".peek()",
    ".read()",
    ".write()",
    ".borrow()",
    ".borrow_mut()",
];

/// Headers whose temporaries last through the block they open.
fn extends_temporaries(header: &str) -> bool {
    let h = header.trim_start();
    let h = h.strip_prefix("else ").unwrap_or(h).trim_start();
    h.starts_with("match ")
        || h.starts_with("if let ")
        || h.starts_with("while let ")
        || h.starts_with("for ")
}

/// Headers that continue the same statement's block nesting outward.
fn is_control(header: &str) -> bool {
    let h = header.trim_start();
    let h = h.strip_prefix("else").unwrap_or(h).trim_start();
    h.is_empty()
        || h.starts_with("match ")
        || h.starts_with("if ")
        || h.starts_with("while ")
        || h.starts_with("for ")
        || h.starts_with("loop")
        || h.contains("=>")
}

/// `src` with comments, string and char literals replaced by spaces (same length, so
/// offsets still match lines).
fn blank(src: &str) -> Vec<u8> {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let mut i = 0;
    let wipe = |out: &mut Vec<u8>, from: usize, to: usize| {
        for c in &mut out[from..to] {
            if *c != b'\n' {
                *c = b' ';
            }
        }
    };
    while i < b.len() {
        if b[i..].starts_with(b"//") {
            let end = b[i..]
                .iter()
                .position(|&c| c == b'\n')
                .map_or(b.len(), |n| i + n);
            wipe(&mut out, i, end);
            i = end;
        } else if b[i..].starts_with(b"/*") {
            let end = src[i + 2..].find("*/").map_or(b.len(), |n| i + 2 + n + 2);
            wipe(&mut out, i, end);
            i = end;
        } else if b[i] == b'r'
            && (b.get(i + 1) == Some(&b'"') || b.get(i + 1) == Some(&b'#'))
            && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_'))
        {
            let hashes = b[i + 1..].iter().take_while(|&&c| c == b'#').count();
            if b.get(i + 1 + hashes) != Some(&b'"') {
                i += 1;
                continue;
            }
            let close = format!("\"{}", "#".repeat(hashes));
            let body = i + 2 + hashes;
            let end = src[body..]
                .find(&close)
                .map_or(b.len(), |n| body + n + close.len());
            wipe(&mut out, i, end);
            i = end;
        } else if b[i] == b'"' {
            let mut j = i + 1;
            while j < b.len() && b[j] != b'"' {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            wipe(&mut out, i, (j + 1).min(b.len()));
            i = j + 1;
        } else if b[i] == b'\'' {
            // A char literal ('x', '\n', '\''); a lifetime ('a) has no closing quote.
            let end = if b.get(i + 1) == Some(&b'\\') {
                b[i + 2..]
                    .iter()
                    .position(|&c| c == b'\'')
                    .map(|n| i + 2 + n + 1)
            } else {
                let len = src[i + 1..].chars().next().map_or(1, char::len_utf8);
                (b.get(i + 1 + len) == Some(&b'\'')).then_some(i + 1 + len + 1)
            };
            match end {
                Some(end) => {
                    wipe(&mut out, i, end);
                    i = end;
                }
                None => i += 1,
            }
        } else {
            i += 1;
        }
    }
    out
}

/// From `at`, back to where the statement or match arm starts: a `;`, a `,`, a block's
/// opening `{`, or a block's closing `}` that ends an earlier statement. Parentheses and
/// brackets around `at` are stepped out of (a call's arguments are the call's
/// statement). Returns that boundary's offset and byte (0 at the start of the file).
fn back_to_boundary(code: &[u8], at: usize) -> (usize, u8) {
    let mut depth = 0i32;
    let mut i = at;
    while i > 0 {
        i -= 1;
        match code[i] {
            b'}' if depth == 0 => {
                // `S { .. }.await`, `match .. { .. }?` go on; anything else ended before.
                let next = code[i + 1..].iter().find(|c| !c.is_ascii_whitespace());
                if matches!(next, Some(b'.') | Some(b'?')) {
                    depth += 1;
                } else {
                    return (i, b'}');
                }
            }
            b')' | b']' | b'}' => depth += 1,
            b'{' if depth == 0 => return (i, b'{'),
            b'(' | b'[' if depth == 0 => {}
            b'(' | b'[' | b'{' => depth -= 1,
            b';' | b',' if depth == 0 => return (i, code[i]),
            _ => {}
        }
    }
    (0, 0)
}

/// `segment` without the bodies of the blocks inside it (`async { .. }`, a closure's
/// `{ .. }`, any `{ .. }` holding statements): their temporaries end inside them. A
/// struct literal's `{ .. }` stays, being part of the expression.
fn strip_blocks(segment: &str) -> String {
    let b = segment.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(segment.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'{' {
            // Find the matching `}` and whether a `;` sits at the group's own level.
            let (mut depth, mut j, mut statements) = (0i32, i, false);
            while j < b.len() {
                match b[j] {
                    b'{' | b'(' | b'[' => depth += 1,
                    b'}' | b')' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    b';' if depth == 1 => statements = true,
                    _ => {}
                }
                j += 1;
            }
            let before = segment[..i].trim_end();
            let block = statements
                || before.ends_with("async")
                || before.ends_with("move")
                || before.ends_with('|');
            if block {
                out.extend_from_slice(b"{}");
                i = (j + 1).min(b.len());
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The places in `src` where a guard may be held across an `.await`, as
/// (line, statement) pairs.
fn held_across_await(src: &str) -> Vec<(usize, String)> {
    let code = blank(src);
    let text = String::from_utf8_lossy(&code).into_owned();
    let mut found = Vec::new();
    for (at, _) in text.match_indices(".await") {
        let mut suspect = Vec::new();
        let (mut start, mut kind) = back_to_boundary(&code, at);
        suspect.push(text[start..at].to_string());
        // A block's header: its temporaries may last through the block; keep going
        // out through control flow in the same statement.
        while kind == b'{' && start > 0 {
            let (outer, outer_kind) = back_to_boundary(&code, start);
            let from = if outer_kind == 0 { 0 } else { outer + 1 };
            let header = &text[from..start];
            if extends_temporaries(header) {
                suspect.push(header.to_string());
            }
            if !is_control(header) {
                break;
            }
            (start, kind) = (outer, outer_kind);
        }
        if let Some(s) = suspect
            .into_iter()
            .map(|s| strip_blocks(&s))
            .find(|s| GUARDS.iter().any(|g| s.contains(g)))
        {
            let line = text[..at].matches('\n').count() + 1;
            found.push((line, s.split_whitespace().collect::<Vec<_>>().join(" ")));
        }
    }
    found
}

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_guard_is_held_across_an_await() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&root, &mut files);
    assert!(files.len() > 20);
    let mut problems = Vec::new();
    for file in files {
        let src = std::fs::read_to_string(&file).unwrap();
        for (line, statement) in held_across_await(&src) {
            let name = file.strip_prefix(&root).unwrap().display();
            problems.push(format!("src/{name}:{line}: {statement}"));
        }
    }
    assert!(
        problems.is_empty(),
        "a guard is held across an .await (bind the value in a statement of its own):\n{}",
        problems.join("\n")
    );
}

#[test]
fn the_check_finds_the_shapes_that_panic_and_passes_the_fixes() {
    let bad = [
        // The lesson rename that panicked while typing during the save.
        "async move { save(app, rename.peek().clone(), busy).await }",
        "fn f() { save_title(app, id, rename_value.peek().clone(), r)\n    .await; }",
        // A match arm: the struct literal's guard lasts through the request.
        "let r = match &edit { A::Create => api.create(&New { kind: *kind.peek(), n }).await, A::B => x };",
        // Scrutinee temporaries last through the block.
        "match app.lib.read().get(id) { Some(_) => go().await, None => {} }",
        "if let Some(x) = sig.peek().first() { send(x).await; }",
        "for x in list.read().iter() { send(x).await; }",
        "let n = cell.borrow_mut().next(f().await);",
    ];
    for code in bad {
        assert!(!held_across_await(code).is_empty(), "not flagged: {code}");
    }
    let good = [
        "let v = rename.peek().clone(); save(v).await;",
        "local.run_until(async { assert!(c.borrow().is_empty()); go().await; }).await;",
        "if x { a.write().push(1); } save(v).await;",
        "if !*busy.peek() { save(next).await; }",
        "if *busy.read() { return; } go().await;",
        "let s = \"x.peek() {\"; go().await;",
        "// sig.read() then\ngo().await;",
        "let c = '{'; let x = sig.read().len(); go(x).await;",
        "while let Ok(m) = eval.recv::<String>().await { list.write().push(m); }",
    ];
    for code in good {
        assert_eq!(held_across_await(code), Vec::new(), "flagged: {code}");
    }
}
