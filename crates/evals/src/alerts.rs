//! `margin-eval alerts --service-url …` (`docs/phase5-brief.md` D6): the service's
//! active alerts (`GET /api/alerts`) and the last day's rises and clears (its `alert`
//! trace rows).

use std::fmt::Write as _;

use anyhow::Context;
use chrono::{DateTime, Utc};
use margin_shared::context::{Alert, AlertList, TraceKind};
use margin_shared::routes::ALERTS;

use crate::traces::{self, AlertLine, TraceFilter};

/// How far back the history goes.
pub const HISTORY: chrono::Duration = chrono::Duration::days(1);

/// `GET {base}/api/alerts` with an admin's device token (the route is admin-only).
pub async fn fetch(
    http: &reqwest::Client,
    base_url: &str,
    token: &str,
) -> anyhow::Result<AlertList> {
    let url = format!("{}{ALERTS}", base_url.trim_end_matches('/'));
    let resp = http
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .with_context(|| format!("couldn't reach {url}"))?;
    let status = resp.status();
    let body = resp.text().await?;
    anyhow::ensure!(
        status.is_success(),
        "{url} answered {status}: {}",
        body.chars().take(200).collect::<String>()
    );
    serde_json::from_str(&body).with_context(|| format!("{url} answered something else"))
}

/// Prints the active alerts and the last day's history.
pub async fn run(http: &reqwest::Client, url: &str, token: &str) -> anyhow::Result<()> {
    let now = Utc::now();
    let list = fetch(http, url, token).await?;
    let rows = traces::fetch(
        http,
        url,
        token,
        &TraceFilter {
            kind: Some(TraceKind::Alert),
            from: Some(now - HISTORY),
            ..Default::default()
        },
    )
    .await?;
    let history: Vec<AlertLine> = rows.iter().filter_map(AlertLine::of).collect();
    print!("{}", render(&list.active, &history, now));
    Ok(())
}

fn number(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{v:.0}")
    } else {
        format!("{v:.3}")
    }
}

fn kind_of(k: Option<&str>) -> String {
    k.map(|k| format!(" ({k})")).unwrap_or_default()
}

fn ago(now: DateTime<Utc>, t: DateTime<Utc>) -> String {
    let m = (now - t).num_minutes().max(0);
    if m < 60 {
        format!("{m} min ago")
    } else {
        format!("{}h{:02} ago", m / 60, m % 60)
    }
}

/// The text `run` prints.
pub fn render(active: &[Alert], history: &[AlertLine], now: DateTime<Utc>) -> String {
    let mut s = String::new();
    if active.is_empty() {
        s.push_str("No alert is active.\n");
    } else {
        let _ = writeln!(s, "Active alerts ({}):", active.len());
        for a in active {
            let _ = writeln!(
                s,
                "  {}{}: {} over {}, since {} ({}), last seen {}",
                a.name,
                kind_of(a.kind.map(|k| k.as_str())),
                number(a.value),
                number(a.threshold),
                a.since.format("%Y-%m-%d %H:%M UTC"),
                ago(now, a.since),
                ago(now, a.last_seen),
            );
        }
    }
    let raised = history.iter().filter(|h| h.raised).count();
    let _ = writeln!(
        s,
        "\nLast 24 hours: {raised} raised, {} cleared.",
        history.len() - raised
    );
    for h in history {
        let _ = writeln!(
            s,
            "  {}  {:<7} {}{} = {}",
            h.at.format("%Y-%m-%d %H:%M"),
            if h.raised { "raised" } else { "cleared" },
            h.name,
            kind_of(h.kind.as_deref()),
            number(h.value),
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use margin_shared::context::AlertEvent;

    fn t(m: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_760_000_000 + m * 60, 0).unwrap()
    }

    #[test]
    fn renders_active_and_history() {
        let a = Alert {
            name: "error_rate".into(),
            kind: Some(TraceKind::ModelCall),
            value: 0.45,
            threshold: 0.2,
            since: t(0),
            last_seen: t(90),
        };
        let history = vec![
            AlertLine {
                at: t(0),
                name: "error_rate".into(),
                kind: Some("model_call".into()),
                raised: true,
                value: 0.45,
            },
            AlertLine {
                at: t(30),
                name: "http_5xx".into(),
                kind: None,
                raised: false,
                value: 6.0,
            },
        ];
        let out = render(&[a], &history, t(95));
        for want in [
            "Active alerts (1):",
            "  error_rate (model_call): 0.450 over 0.200, since 2025-10-09 08:53 UTC (1h35 ago), last seen 5 min ago",
            "Last 24 hours: 1 raised, 1 cleared.",
            "  2025-10-09 08:53  raised  error_rate (model_call) = 0.450",
            "  2025-10-09 09:23  cleared http_5xx = 6",
        ] {
            assert!(out.contains(want), "{want:?} not in\n{out}");
        }
        let quiet = render(&[], &[], t(0));
        assert!(quiet.starts_with("No alert is active.\n"));
        assert!(quiet.contains("0 raised, 0 cleared"));
    }

    #[tokio::test]
    async fn reads_the_list_with_the_device_token() {
        use axum::http::HeaderMap;
        let list = AlertList {
            active: Vec::new(),
            recent: vec![AlertEvent {
                alert: Alert {
                    name: "http_5xx".into(),
                    kind: None,
                    value: 6.0,
                    threshold: 5.0,
                    since: t(0),
                    last_seen: t(0),
                },
                raised: true,
                at: t(0),
            }],
        };
        let body = serde_json::to_string(&list).unwrap();
        let app = axum::Router::new().route(
            ALERTS,
            axum::routing::get(move |h: HeaderMap| {
                let body = body.clone();
                async move {
                    if h.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer tok") {
                        return (axum::http::StatusCode::UNAUTHORIZED, "no".to_string());
                    }
                    (axum::http::StatusCode::OK, body)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = reqwest::Client::new();
        let base = format!("http://{addr}/");
        assert_eq!(fetch(&http, &base, "tok").await.unwrap(), list);
        let err = fetch(&http, &base, "bad").await.unwrap_err().to_string();
        assert!(err.contains("401"), "{err}");
    }
}
