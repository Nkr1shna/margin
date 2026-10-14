//! Runs a tool, handing each line it prints to the caller as it arrives.

use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stream {
    Stdout,
    Stderr,
}

#[derive(Debug)]
pub(crate) struct Finished {
    /// `None` when the run was stopped for taking too long.
    pub status: Option<ExitStatus>,
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
}

/// Runs `cmd` to the end (or until `limit`, when it is killed), calling `on_line` for
/// every line of its stdout and stderr in the order they arrive.
pub(crate) async fn run_streaming(
    mut cmd: Command,
    limit: Duration,
    mut on_line: impl FnMut(Stream, &str),
) -> std::io::Result<Finished> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn()?;
    let (tx, mut rx) = mpsc::unbounded_channel::<(Stream, String)>();
    if let Some(out) = child.stdout.take() {
        tokio::spawn(read_lines(out, Stream::Stdout, tx.clone()));
    }
    if let Some(err) = child.stderr.take() {
        tokio::spawn(read_lines(err, Stream::Stderr, tx.clone()));
    }
    drop(tx);

    let deadline = tokio::time::Instant::now() + limit;
    let mut finished = Finished {
        status: None,
        stdout: Vec::new(),
        stderr: Vec::new(),
    };
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some((stream, line))) => {
                on_line(stream, &line);
                match stream {
                    Stream::Stdout => finished.stdout.push(line),
                    Stream::Stderr => finished.stderr.push(line),
                }
            }
            Ok(None) => break,
            Err(_) => {
                child.kill().await.ok();
                return Ok(finished);
            }
        }
    }
    match tokio::time::timeout_at(deadline, child.wait()).await {
        Ok(status) => finished.status = Some(status?),
        Err(_) => {
            child.kill().await.ok();
        }
    }
    Ok(finished)
}

async fn read_lines(
    reader: impl AsyncRead + Unpin,
    stream: Stream,
    tx: mpsc::UnboundedSender<(Stream, String)>,
) {
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let text = String::from_utf8_lossy(&buf);
                let line = text.trim_end_matches(['\n', '\r']).to_string();
                if tx.send((stream, line)).is_err() {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn streams_both_outputs_and_status() {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg("echo one; echo two >&2; echo three; exit 3");
        let mut seen = Vec::new();
        let done = run_streaming(cmd, Duration::from_secs(10), |s, l| {
            seen.push((s, l.to_string()))
        })
        .await
        .unwrap();
        assert_eq!(done.status.and_then(|s| s.code()), Some(3));
        assert_eq!(done.stdout, vec!["one", "three"]);
        assert_eq!(done.stderr, vec!["two"]);
        assert_eq!(seen.len(), 3);
    }

    #[tokio::test]
    async fn stops_a_run_that_takes_too_long() {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("echo start; sleep 30");
        let done = run_streaming(cmd, Duration::from_millis(500), |_, _| {})
            .await
            .unwrap();
        assert!(done.status.is_none());
        assert_eq!(done.stdout, vec!["start"]);
    }
}
