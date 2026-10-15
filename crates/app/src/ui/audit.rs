//! Settings → Organisation → Audit (L2, `docs/l2-brief.md` D7, D9): who changed who
//! may read or do what, for admins. Filters by kind and day, "Load earlier" pages back a
//! week at a time, and new rows arrive live (`Event::AuditAppended`). Members read why
//! they don't see it. Exports have no control here: they run on the service host and
//! show up as rows.

use chrono::{DateTime, Duration, Local, NaiveDate, TimeZone, Utc};
use dioxus::prelude::*;
use margin_shared::context::{AuditKind, AuditRow};

use crate::org;
use crate::state::use_app;
use crate::ui::audience::Directory;
use crate::ui::common::{ErrorNotice, date};

/// What a member reads instead of the log.
pub const ADMINS_ONLY: &str = "Only an organisation admin reads the audit log.";

/// How far back one page of the log goes when no day is picked.
pub const WEEK: i64 = 7;

/// The range the view asks for: a picked day (local midnight to midnight), else the last
/// `weeks` weeks up to `now`.
pub fn audit_window(
    day: Option<NaiveDate>,
    weeks: i64,
    now: DateTime<Utc>,
) -> (DateTime<Utc>, DateTime<Utc>) {
    match day {
        Some(d) => {
            let start = Local
                .from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap_or_default())
                .earliest()
                .map(|t| t.with_timezone(&Utc))
                .unwrap_or(now);
            (start, start + Duration::days(1))
        }
        None => (now - Duration::days(WEEK * weeks.max(1)), now),
    }
}

/// The rows the view shows, newest first: of `kind` (or every kind), and on `day` when
/// one is picked (rows that arrived live since the fetch included otherwise).
pub fn shown_rows<'a>(
    rows: &[&'a AuditRow],
    kind: Option<AuditKind>,
    day: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Vec<&'a AuditRow> {
    rows.iter()
        .copied()
        .filter(|r| kind.is_none_or(|k| r.kind == k))
        .filter(|r| day.is_none_or(|(from, to)| r.at >= from && r.at < to))
        .collect()
}

/// When, as the table shows it: "Oct 14, 2026 · 14:02".
fn when(at: &DateTime<Utc>) -> String {
    format!(
        "{} · {}",
        date(at),
        at.with_timezone(&Local).format("%H:%M")
    )
}

/// The Audit section of Settings → Organisation.
#[component]
pub fn AuditSection() -> Element {
    let app = use_app();
    let admin = app.is_admin();
    let mut kind = use_signal(|| None::<AuditKind>);
    let mut day = use_signal(|| None::<NaiveDate>);
    let mut weeks = use_signal(|| 1i64);
    let mut load = use_resource(move || async move {
        let (kind, day, weeks) = (*kind.read(), *day.read(), *weeks.read());
        if !app.is_admin() {
            return Ok(());
        }
        let api = app.api().ok_or_else(|| "Not signed in.".to_string())?;
        let (from, to) = audit_window(day, weeks, Utc::now());
        let rows = api
            .audit(kind, Some(from), Some(to))
            .await
            .map_err(|e| app.failure(&e))?;
        let mut lib = app.lib;
        lib.write().replace_audit(rows);
        Ok::<(), String>(())
    });

    if !admin {
        return rsx! {
            section { class: "set-sec",
                h2 { "Audit" }
                p { class: "help", "{ADMINS_ONLY}" }
            }
        };
    }

    let failed = match &*load.read() {
        Some(Err(e)) => Some(e.clone()),
        _ => None,
    };
    let loading = load.read().is_none();
    let picked_day = *day.read();
    let range = picked_day.map(|d| audit_window(Some(d), 1, Utc::now()));
    let (from, _) = audit_window(None, *weeks.read(), Utc::now());
    let lib = app.lib.read();
    let dir = Directory::of(&lib);
    let loaded = lib.audit_loaded;
    let all = lib.audit_newest_first();
    let rows: Vec<AuditRow> = shown_rows(&all, *kind.read(), range)
        .into_iter()
        .cloned()
        .collect();
    drop(lib);
    let names = dir.names();
    let day_value = picked_day
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default();

    rsx! {
        section { class: "set-sec",
            h2 { "Audit" }
            p { class: "help", "Every change to who may read or do what: audiences, groups and memberships, roles, agents and their tokens, and exports run on the service host. It never holds what anything says." }
            div { class: "audit-filters",
                div { class: "field",
                    label { r#for: "audit-kind", "Kind" }
                    select {
                        id: "audit-kind",
                        class: "select-input",
                        onchange: move |e: FormEvent| {
                            let v = e.value();
                            kind.set(AuditKind::ALL.into_iter().find(|k| k.as_str() == v));
                        },
                        option { value: "", selected: kind.read().is_none(), "Every kind" }
                        for k in AuditKind::ALL {
                            option {
                                key: "{k.as_str()}",
                                value: "{k.as_str()}",
                                selected: *kind.read() == Some(k),
                                {org::audit_kind_label(k)}
                            }
                        }
                    }
                }
                div { class: "field",
                    label { r#for: "audit-day", "Day" }
                    div { class: "token-line",
                        input {
                            id: "audit-day",
                            class: "input",
                            r#type: "date",
                            value: "{day_value}",
                            onchange: move |e: FormEvent| {
                                day.set(NaiveDate::parse_from_str(&e.value(), "%Y-%m-%d").ok());
                            },
                        }
                        if picked_day.is_some() {
                            button {
                                class: "btn btn-ghost btn-sm",
                                onclick: move |_| day.set(None),
                                "Any day"
                            }
                        }
                    }
                }
            }
            if let Some(err) = failed {
                ErrorNotice { message: err, on_retry: move |_| load.restart() }
            }
            if loading && !loaded {
                p { class: "faint", "Loading the audit log…" }
            } else if rows.is_empty() {
                p { class: "faint",
                    match picked_day {
                        Some(d) => format!("Nothing changed on {}.", d.format("%b %-d, %Y")),
                        None => format!("Nothing changed since {}.", date(&from)),
                    }
                }
            } else {
                div { class: "audit-table", role: "table", "aria-label": "Audit log",
                    div { class: "audit-row audit-head", role: "row",
                        span { role: "columnheader", "When" }
                        span { role: "columnheader", "Who" }
                        span { role: "columnheader", "What" }
                        span { role: "columnheader", "About" }
                        span { role: "columnheader", "Change" }
                    }
                    for (i, row) in rows.iter().enumerate() {
                        div { key: "{i}-{row.at.timestamp_millis()}", class: "audit-row", role: "row",
                            span { class: "audit-when", role: "cell", {when(&row.at)} }
                            span { class: "audit-who", role: "cell", {org::actor_label(row, names)} }
                            span { class: "audit-what", role: "cell", {org::audit_kind_label(row.kind)} }
                            span { class: "audit-subject", role: "cell", {org::subject_label(row, names)} }
                            span { class: "audit-change mono", role: "cell", {org::audit_change(row, names)} }
                        }
                    }
                }
            }
            if picked_day.is_none() {
                div { class: "row-actions",
                    button {
                        class: "btn btn-secondary btn-sm",
                        disabled: loading,
                        onclick: move |_| {
                            let next = *weeks.peek() + 1;
                            weeks.set(next);
                        },
                        if loading { span { class: "spinner" } }
                        "Load earlier (before {date(&from)})"
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{ALICE, BOB, FakeState, TOKEN, TOKEN_BOB};
    use crate::ui::audience::tests::{as_person, lib_of};
    use crate::ui::settings::SettingsPane;
    use crate::ui::test_render::render_with;

    #[test]
    fn admins_read_the_log_as_who_what_and_a_diff() {
        let s = FakeState::new();
        let mut lib = lib_of(ALICE);
        lib.replace_audit(
            s.audit
                .iter()
                .map(|r| s.with_subject_name(ALICE, r.clone()))
                .collect(),
        );
        let html = render_with("/", lib, Some(SettingsPane::Organisation), as_person(TOKEN));
        let at = html.find("<h2>Audit</h2>").expect("the Audit section");
        let audit = &html[at..];
        assert!(audit.contains("role=\"table\""), "{audit}");
        assert!(audit.contains(">When<") && audit.contains(">Change<"));
        // Newest first: Bob's document (unreadable to Alice: no name), then the token,
        // the topic Alice shared, the membership.
        let order: Vec<usize> = [
            "a document of @bob",
            ">claude-code<",
            "Systems</span>",
            "groups: +@engineering",
        ]
        .iter()
        .map(|t| audit.find(t).unwrap_or_else(|| panic!("{t}: {audit}")))
        .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
        assert!(audit.contains("access: +@engineering"));
        assert!(audit.contains(">Audience changed<") && audit.contains(">Agent token<"));
        // Filters and paging.
        assert!(audit.contains("id=\"audit-kind\"") && audit.contains(">Every kind<"));
        assert!(audit.contains("type=\"date\""));
        assert!(audit.contains("Load earlier (before "));
    }

    #[test]
    fn members_read_why_they_dont_see_it() {
        let mut lib = lib_of(BOB);
        lib.replace_audit(FakeState::new().audit.clone());
        let html = render_with(
            "/",
            lib,
            Some(SettingsPane::Organisation),
            as_person(TOKEN_BOB),
        );
        assert!(html.contains(ADMINS_ONLY), "{html}");
        assert!(!html.contains("role=\"table\""));
    }

    #[test]
    fn the_window_is_a_day_or_the_last_weeks() {
        let now = Utc::now();
        let (from, to) = audit_window(None, 2, now);
        assert_eq!((to - from).num_days(), 14);
        let day = NaiveDate::from_ymd_opt(2026, 10, 14).unwrap();
        let (from, to) = audit_window(Some(day), 1, now);
        assert_eq!((to - from).num_hours(), 24);
        assert_eq!(from.with_timezone(&Local).date_naive(), day);
        let rows = crate::fake::sample_audit(now);
        let all: Vec<&AuditRow> = rows.iter().collect();
        assert_eq!(shown_rows(&all, None, None).len(), 4);
        assert_eq!(
            shown_rows(&all, Some(AuditKind::AudienceChange), None).len(),
            2
        );
        let empty = (now - Duration::days(9), now - Duration::days(8));
        assert!(shown_rows(&all, None, Some(empty)).is_empty());
    }
}
