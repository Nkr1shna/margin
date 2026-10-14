//! Turns what ManimGX prints for a failed scene into a short message for the model that
//! repairs it.
//!
//! ManimGX prints a failure to stderr as frames of the scene's own code, then the error,
//! then any hint:
//!
//! ```text
//! scene.py:7: in construct
//!     self.play(m.ShowCreation(c))
//! AttributeError: module 'manimgx' has no attribute 'ShowCreation'
//! hint: ShowCreation is Create in ManimGX
//! ```
//!
//! The message keeps the frames in `scene.py` (or the last frame when none is), and
//! everything after the last frame.

const MAX_TAIL_LINES: usize = 12;
const MAX_FRAMES: usize = 6;

/// A frame line looks like `scene.py:7: in construct`.
fn frame_file(line: &str) -> Option<&str> {
    let (file, rest) = line.split_once(':')?;
    if file.is_empty() || file.starts_with(char::is_whitespace) || !file.ends_with(".py") {
        return None;
    }
    let (num, rest) = rest.split_once(':')?;
    (!num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()) && rest.starts_with(" in "))
        .then_some(file)
}

/// Lines a sandboxed Metal prints that say nothing about the scene.
fn is_noise(line: &str) -> bool {
    line.trim().is_empty() || line.starts_with("sandbox_extension_issue_file")
}

pub(crate) fn scene_message(stderr: &[String], stdout: &[String]) -> String {
    let mut lines: Vec<&str> = stderr
        .iter()
        .map(String::as_str)
        .filter(|l| !is_noise(l))
        .collect();
    if lines.is_empty() {
        lines = stdout
            .iter()
            .map(String::as_str)
            .filter(|l| !is_noise(l))
            .collect();
    }
    let frames: Vec<usize> = (0..lines.len())
        .filter(|&i| frame_file(lines[i]).is_some())
        .collect();
    let Some(&last) = frames.last() else {
        let start = lines.len().saturating_sub(MAX_TAIL_LINES);
        return lines[start..].join("\n");
    };
    let with_code = |i: usize| -> Vec<&str> {
        let mut out = vec![lines[i]];
        if let Some(code) = lines
            .get(i + 1)
            .filter(|l| l.starts_with(char::is_whitespace))
        {
            out.push(code);
        }
        out
    };
    let mut chosen: Vec<usize> = frames
        .iter()
        .copied()
        .filter(|&i| frame_file(lines[i]) == Some("scene.py"))
        .collect();
    if chosen.is_empty() {
        chosen.push(last);
    }
    let skip = chosen.len().saturating_sub(MAX_FRAMES);
    let mut out: Vec<&str> = chosen[skip..].iter().flat_map(|&i| with_code(i)).collect();
    let tail_start = last + with_code(last).len();
    out.extend(lines[tail_start..].iter().take(MAX_TAIL_LINES));
    out.join("\n")
}

/// Whether the failure is the machine's (no GPU), not the scene's.
pub(crate) fn is_gpu_failure(message: &str) -> bool {
    message.contains("no GPU adapter") || message.contains("no GPU device")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Vec<String> {
        s.lines().map(str::to_string).collect()
    }

    #[test]
    fn keeps_the_failing_line_and_hint() {
        let err = v(
            "scene.py:7: in construct\n    self.play(m.ShowCreation(c))\nAttributeError: module 'manimgx' has no attribute 'ShowCreation'\nhint: ShowCreation is Create in ManimGX",
        );
        assert_eq!(scene_message(&err, &[]), err.join("\n"));
    }

    #[test]
    fn drops_library_frames_but_keeps_the_scene_frame() {
        let err = v(
            "scene.py:8: in construct\n    body = urllib.request.urlopen(url).read()\nrequest.py:189: in urlopen\n    return opener.open(url, data, timeout)\nrequest.py:1336: in do_open\n    raise URLError(err)\nURLError: <urlopen error [Errno 8] nodename nor servname provided, or not known>",
        );
        assert_eq!(
            scene_message(&err, &[]),
            "scene.py:8: in construct\n    body = urllib.request.urlopen(url).read()\nURLError: <urlopen error [Errno 8] nodename nor servname provided, or not known>"
        );
    }

    #[test]
    fn syntax_errors_have_no_frames() {
        let err = v("SyntaxError: '(' was never closed (scene.py, line 6)");
        assert_eq!(
            scene_message(&err, &[]),
            "SyntaxError: '(' was never closed (scene.py, line 6)"
        );
    }

    #[test]
    fn falls_back_to_stdout() {
        assert_eq!(scene_message(&[], &v("boom")), "boom");
    }

    #[test]
    fn frame_lines() {
        assert_eq!(frame_file("scene.py:12: in construct"), Some("scene.py"));
        assert_eq!(frame_file("    scene.py:12: in construct"), None);
        assert_eq!(frame_file("hint: x"), None);
        assert_eq!(frame_file("ValueError: a:1: in b"), None);
    }
}
