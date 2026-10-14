//! App-control tools: the agent shows a screen, a lesson's citation or a source passage,
//! or shows or hides the sidebar, on the device whose message started the turn. The
//! shell runs them on the main thread (`ui::shell::Shell` drains
//! `AppCtx::agent_commands` into `perform`); `plan` works out what a call does from the
//! library, so an unknown id is answered with an error and nothing changes.

use dioxus::prelude::*;
use margin_shared::{
    Card, Citation, DeviceToolCall, DeviceToolOutput, DeviceToolResult, Id, LessonView, NavTarget,
    SourceKind,
};

use crate::citations::locator;
use crate::routes::{Route, chat_title, nav_route};
use crate::state::{AgentCommand, AppCtx, SourceViewer};
use crate::store::LibraryState;
use crate::ui::common::js;
use crate::ui::settings::SettingsPane;
use crate::ui::shell::close_drawer;

/// What an app-control call does, worked out from the library.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// Show this screen.
    Go(Route),
    /// Open the Settings dialog.
    Settings,
    /// Open the lesson (`Route::LessonRead`) with the source viewer on a citation.
    Citation(SourceViewer),
    /// Open the source viewer on this passage.
    Passage(Citation),
    /// Show (`Some(true)`), hide or toggle (`None`) the sidebar.
    Sidebar(Option<bool>),
}

/// What `call` (from a turn of `chat_id`) does, and what to tell the model once done;
/// the error to answer when what it names doesn't exist. Sidebar messages depend on
/// the sidebar's state when it runs (`perform` writes them).
pub fn plan(
    lib: &LibraryState,
    chat_id: &str,
    call: &DeviceToolCall,
) -> Result<(Plan, String), String> {
    match call {
        DeviceToolCall::Navigate { to } => navigate(lib, to),
        DeviceToolCall::OpenCitation { lesson_id, n } => {
            let lesson = lib
                .lessons
                .get(lesson_id)
                .ok_or_else(|| missing("lesson", lesson_id))?;
            let Some(c) = lesson.citations.iter().find(|c| c.n == *n) else {
                let count = lesson.citations.len();
                return Err(match count {
                    0 => format!("The lesson “{}” has no citations.", lesson.title),
                    _ => format!(
                        "The lesson “{}” has no citation {n}; its citations are 1 to {count}.",
                        lesson.title
                    ),
                });
            };
            Ok((
                Plan::Citation(SourceViewer {
                    lesson_id: lesson_id.clone(),
                    n: *n,
                }),
                format!(
                    "Opened citation {n} of “{}”: “{}”.",
                    lesson.title, c.source_name
                ),
            ))
        }
        DeviceToolCall::ShowPassage {
            source_id,
            page,
            start_ms,
        } => {
            let passage = passage(lib, chat_id, source_id, *page, *start_ms)?;
            let message = match locator(&Citation {
                end_ms: None,
                ..passage.clone()
            }) {
                Some(at) => format!("Opened “{}” at {at}.", passage.source_name),
                None => format!("Opened “{}”.", passage.source_name),
            };
            Ok((Plan::Passage(passage), message))
        }
        DeviceToolCall::ToggleSidebar { visible } => Ok((Plan::Sidebar(*visible), String::new())),
        DeviceToolCall::ListFiles { .. }
        | DeviceToolCall::FindFiles { .. }
        | DeviceToolCall::SearchText { .. }
        | DeviceToolCall::ReadFile { .. }
        | DeviceToolCall::SyncFolder { .. }
        | DeviceToolCall::Manifest { .. }
        | DeviceToolCall::FetchFile { .. } => Err("That isn't an app-control tool.".into()),
    }
}

fn missing(what: &str, id: &str) -> String {
    format!("There's no {what} with the id “{id}”.")
}

fn navigate(lib: &LibraryState, to: &NavTarget) -> Result<(Plan, String), String> {
    let message = match to {
        NavTarget::Lesson { lesson_id, view } => {
            let lesson = lib
                .lessons
                .get(lesson_id)
                .ok_or_else(|| missing("lesson", lesson_id))?;
            match view {
                None | Some(LessonView::Read) => format!("Opened the lesson “{}”.", lesson.title),
                Some(LessonView::Notes) => {
                    format!("Opened the notes of the lesson “{}”.", lesson.title)
                }
                Some(LessonView::Video) => {
                    format!("Opened the video of the lesson “{}”.", lesson.title)
                }
            }
        }
        NavTarget::Topic { topic_id } => {
            let topic = lib
                .topics
                .get(topic_id)
                .ok_or_else(|| missing("topic", topic_id))?;
            format!("Opened the notes in the topic “{}”.", topic.title)
        }
        NavTarget::Note { note_id } => {
            let note = lib
                .notes
                .get(note_id)
                .ok_or_else(|| missing("note", note_id))?;
            let title = match note.title.trim() {
                "" => crate::drafts::UNTITLED,
                title => title,
            };
            format!("Opened the note “{title}”.")
        }
        NavTarget::AllNotes => "Opened All notes.".to_string(),
        NavTarget::Chat { chat_id } => {
            let chat = lib
                .chats
                .get(chat_id)
                .ok_or_else(|| missing("chat", chat_id))?;
            format!("Opened the chat “{}”.", chat_title(&chat.title))
        }
        NavTarget::Settings => return Ok((Plan::Settings, "Opened Settings.".to_string())),
    };
    let route = nav_route(to).expect("every target but Settings is a route");
    Ok((Plan::Go(route), message))
}

/// The passage of `source_id` at `page` / `start_ms` to show: one the chat's searches or
/// a lesson already found there (it has the text and picture), else the source at that
/// place.
fn passage(
    lib: &LibraryState,
    chat_id: &str,
    source_id: &Id,
    page: Option<u32>,
    start_ms: Option<u64>,
) -> Result<Citation, String> {
    let source = lib
        .sources
        .get(source_id)
        .ok_or_else(|| missing("source", source_id))?;
    let found_in_chat = lib
        .chat_items
        .get(chat_id)
        .into_iter()
        .flatten()
        .rev()
        .filter_map(|item| match &item.kind {
            margin_shared::ChatItemKind::Card {
                card: Card::Passages(card),
                ..
            } => Some(card.passages.iter()),
            _ => None,
        })
        .flatten();
    let found_in_lessons = lib.lessons.values().flat_map(|l| l.citations.iter());
    let at = |c: &&Citation| {
        c.source_id == *source_id
            && page.is_none_or(|p| c.page == Some(p))
            && start_ms.is_none_or(|t| {
                c.start_ms
                    .is_some_and(|s| s <= t && t <= c.end_ms.unwrap_or(s).max(s))
            })
    };
    let mut shown = match found_in_chat.chain(found_in_lessons).find(at) {
        Some(known) => known.clone(),
        None => Citation {
            n: 1,
            source_id: source.id.clone(),
            source_name: source.name.clone(),
            kind: source.kind,
            page,
            start_ms,
            end_ms: None,
            excerpt: String::new(),
            image_url: match source.kind {
                SourceKind::Image => source.media_url.clone(),
                _ => None,
            },
            document_id: None,
            start_line: None,
            end_line: None,
            file: None,
        },
    };
    shown.n = 1;
    // Play from the moment asked for, within the passage found.
    if let Some(t) = start_ms {
        shown.start_ms = Some(t);
    }
    Ok(shown)
}

/// Run one queued call and answer it. Runs in the shell's scope (on the main thread),
/// where the router is; `parked` is the shell's citation waiting for its lesson to open.
pub async fn perform(app: AppCtx, mut parked: Signal<Option<SourceViewer>>, command: AgentCommand) {
    let AgentCommand {
        call,
        chat_id,
        reply,
    } = command;
    // Cancelled (or timed out) while it waited.
    if reply.is_closed() {
        return;
    }
    let planned = plan(&app.lib.peek(), &chat_id, &call);
    let result = match planned {
        Err(message) => DeviceToolResult::Err { message },
        Ok((plan, message)) => {
            let message = run(app, &mut parked, plan, message).await;
            DeviceToolResult::Ok {
                output: DeviceToolOutput::Done { message },
            }
        }
    };
    let _ = reply.send(result);
}

/// Carry out `plan`; the message for the model.
async fn run(
    mut app: AppCtx,
    parked: &mut Signal<Option<SourceViewer>>,
    plan: Plan,
    message: String,
) -> String {
    let nav = navigator();
    // Whatever the agent shows comes to the front: the search palette and dialogs over
    // the screen close (a sidebar change leaves them).
    if !matches!(plan, Plan::Sidebar(_)) {
        app.search_open.set(false);
        if !matches!(plan, Plan::Settings) {
            app.settings_open.set(None);
        }
        app.shown_passage.set(None);
    }
    match plan {
        Plan::Go(route) => {
            nav.push(route);
            message
        }
        Plan::Settings => {
            if app.settings_open.peek().is_none() {
                app.settings_open.set(Some(SettingsPane::General));
            }
            message
        }
        Plan::Citation(viewer) => {
            let lesson = Route::LessonRead {
                id: viewer.lesson_id.clone(),
            };
            if router().current::<Route>() == lesson {
                app.source_viewer.set(Some(viewer));
            } else {
                // The lesson screen that's open (if any) closes its viewer as it goes;
                // the shell opens this one once the lesson is the screen.
                parked.set(Some(viewer));
                nav.push(lesson);
            }
            message
        }
        Plan::Passage(passage) => {
            app.source_viewer.set(None);
            app.shown_passage.set(Some(passage));
            message
        }
        Plan::Sidebar(visible) => {
            // Below 1024px the sidebar is the drawer.
            let narrow = js("return window.matchMedia('(max-width: 1023px)').matches;")
                .join::<bool>()
                .await
                .unwrap_or(false);
            let shown = if narrow {
                *app.drawer_open.peek()
            } else {
                !app.config.peek().sidebar_hidden
            };
            let show = visible.unwrap_or(!shown);
            if show == shown {
                return format!(
                    "The sidebar was already {}.",
                    if show { "showing" } else { "hidden" }
                );
            }
            if narrow {
                if show {
                    app.search_open.set(false);
                    app.drawer_open.set(true);
                } else {
                    close_drawer(app);
                }
            } else {
                let _ = app.set_config(|c| c.sidebar_hidden = !show);
                app.drawer_open.set(false);
            }
            if show {
                "Showed the sidebar."
            } else {
                "Hid the sidebar."
            }
            .to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::{lesson, note, source, topic};
    use margin_shared::{ChatItem, ChatItemKind, ItemStatus, LessonStatus, Library, PassagesCard};

    fn cite(n: u32, source_id: &str, page: Option<u32>, start_ms: Option<u64>) -> Citation {
        Citation {
            n,
            source_id: source_id.into(),
            source_name: format!("{source_id}.pdf"),
            kind: SourceKind::Pdf,
            page,
            start_ms,
            end_ms: start_ms.map(|s| s + 4_000),
            excerpt: format!("passage {n}"),
            image_url: None,
            document_id: None,
            start_line: None,
            end_line: None,
            file: None,
        }
    }

    fn library() -> LibraryState {
        let mut l1 = lesson("l1", "f", 0, LessonStatus::Ready);
        l1.title = "Partial sums".into();
        l1.citations = vec![
            cite(1, "src1", Some(3), None),
            cite(2, "rec", None, Some(10_000)),
        ];
        let mut untitled = note("n2", "f", None);
        untitled.title = " ".into();
        let mut lib = LibraryState::from_library(Library {
            topics: vec![topic("f", None, "Fourier", 0)],
            lessons: vec![l1],
            notes: vec![note("n1", "f", Some("l1")), untitled],
            videos: vec![],
            sources: vec![
                source("src1", Some("f"), "paper.pdf", None),
                source("rec", Some("f"), "talk.mp3", None),
                source("pic", None, "board.png", None),
            ],
        });
        let mut chat = crate::fake::sample_chats().remove(0).0;
        chat.id = "c1".into();
        chat.title = "Fourier questions".into();
        lib.chats.insert("c1".into(), chat);
        lib.sources.get_mut("pic").unwrap().media_url = Some("https://m/board.png".into());
        lib
    }

    fn go(to: NavTarget) -> Result<(Plan, String), String> {
        plan(&library(), "c1", &DeviceToolCall::Navigate { to })
    }

    #[test]
    fn navigate_opens_each_screen() {
        let lesson = |view| NavTarget::Lesson {
            lesson_id: "l1".into(),
            view,
        };
        assert_eq!(
            go(lesson(None)),
            Ok((
                Plan::Go(Route::LessonRead { id: "l1".into() }),
                "Opened the lesson “Partial sums”.".into()
            ))
        );
        assert_eq!(
            go(lesson(Some(LessonView::Notes))).unwrap().0,
            Plan::Go(Route::LessonNotes { id: "l1".into() })
        );
        assert_eq!(
            go(lesson(Some(LessonView::Video))),
            Ok((
                Plan::Go(Route::LessonVideo { id: "l1".into() }),
                "Opened the video of the lesson “Partial sums”.".into()
            ))
        );
        assert_eq!(
            go(NavTarget::Topic {
                topic_id: "f".into()
            }),
            Ok((
                Plan::Go(Route::TopicNotes {
                    topic_id: "f".into()
                }),
                "Opened the notes in the topic “Fourier”.".into()
            ))
        );
        assert_eq!(
            go(NavTarget::Note {
                note_id: "n2".into()
            }),
            Ok((
                Plan::Go(Route::NoteOpen { id: "n2".into() }),
                "Opened the note “Untitled note”.".into()
            ))
        );
        assert_eq!(
            go(NavTarget::AllNotes),
            Ok((Plan::Go(Route::AllNotes {}), "Opened All notes.".into()))
        );
        assert_eq!(
            go(NavTarget::Chat {
                chat_id: "c1".into()
            }),
            Ok((
                Plan::Go(Route::Chat { id: "c1".into() }),
                "Opened the chat “Fourier questions”.".into()
            ))
        );
        assert_eq!(
            go(NavTarget::Settings),
            Ok((Plan::Settings, "Opened Settings.".into()))
        );
    }

    #[test]
    fn unknown_ids_are_errors() {
        assert_eq!(
            go(NavTarget::Lesson {
                lesson_id: "nope".into(),
                view: None
            }),
            Err("There's no lesson with the id “nope”.".into())
        );
        for to in [
            NavTarget::Topic {
                topic_id: "x".into(),
            },
            NavTarget::Note {
                note_id: "x".into(),
            },
            NavTarget::Chat {
                chat_id: "x".into(),
            },
        ] {
            assert!(go(to).is_err());
        }
        let lib = library();
        assert!(
            plan(
                &lib,
                "c1",
                &DeviceToolCall::ShowPassage {
                    source_id: "x".into(),
                    page: None,
                    start_ms: None
                }
            )
            .is_err()
        );
        assert!(
            plan(
                &lib,
                "c1",
                &DeviceToolCall::ListFiles {
                    workspace_id: "w".into(),
                    path: String::new()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn open_citation_checks_the_number() {
        let lib = library();
        let open = |lesson_id: &str, n| {
            plan(
                &lib,
                "c1",
                &DeviceToolCall::OpenCitation {
                    lesson_id: lesson_id.into(),
                    n,
                },
            )
        };
        assert_eq!(
            open("l1", 1),
            Ok((
                Plan::Citation(SourceViewer {
                    lesson_id: "l1".into(),
                    n: 1
                }),
                "Opened citation 1 of “Partial sums”: “src1.pdf”.".into()
            ))
        );
        assert_eq!(
            open("l1", 5),
            Err("The lesson “Partial sums” has no citation 5; its citations are 1 to 2.".into())
        );
        assert!(open("nope", 1).is_err());
    }

    #[test]
    fn show_passage_prefers_a_passage_already_found() {
        let mut lib = library();
        let show = |lib: &LibraryState, source_id: &str, page, start_ms| {
            plan(
                lib,
                "c1",
                &DeviceToolCall::ShowPassage {
                    source_id: source_id.into(),
                    page,
                    start_ms,
                },
            )
        };
        // From a lesson's citation of that page.
        let (Plan::Passage(c), message) = show(&lib, "src1", Some(3), None).unwrap() else {
            panic!("not a passage");
        };
        assert_eq!((c.n, c.excerpt.as_str()), (1, "passage 1"));
        assert_eq!(message, "Opened “src1.pdf” at p. 3.");
        // Within a recording's cited span; it plays from the moment asked.
        let (Plan::Passage(c), message) = show(&lib, "rec", None, Some(12_000)).unwrap() else {
            panic!("not a passage");
        };
        assert_eq!(
            (c.excerpt.as_str(), c.start_ms),
            ("passage 2", Some(12_000))
        );
        assert_eq!(message, "Opened “rec.pdf” at 0:12.");
        // Nothing found there: the source itself at that place.
        let (Plan::Passage(c), message) = show(&lib, "src1", Some(9), None).unwrap() else {
            panic!("not a passage");
        };
        assert_eq!(
            (c.source_name.as_str(), c.page, c.excerpt.as_str()),
            ("paper.pdf", Some(9), "")
        );
        assert_eq!(message, "Opened “paper.pdf” at p. 9.");
        // An image shows its picture.
        let (Plan::Passage(c), _) = show(&lib, "pic", None, None).unwrap() else {
            panic!("not a passage");
        };
        assert_eq!(c.image_url.as_deref(), Some("https://m/board.png"));
        // The chat's own search results come first.
        let mut found = cite(4, "src1", Some(9), None);
        found.excerpt = "from the search".into();
        lib.chat_items.insert(
            "c1".into(),
            vec![ChatItem {
                id: "i1".into(),
                chat_id: "c1".into(),
                turn_id: "t1".into(),
                seq: 1,
                status: ItemStatus::Done,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                kind: ChatItemKind::Card {
                    call_id: "call_1".into(),
                    card: Card::Passages(PassagesCard {
                        query: "q".into(),
                        passages: vec![found],
                    }),
                },
            }],
        );
        let (Plan::Passage(c), _) = show(&lib, "src1", Some(9), None).unwrap() else {
            panic!("not a passage");
        };
        assert_eq!((c.n, c.excerpt.as_str()), (1, "from the search"));
    }

    #[test]
    fn toggle_sidebar_is_planned_as_asked() {
        assert_eq!(
            plan(
                &library(),
                "c1",
                &DeviceToolCall::ToggleSidebar {
                    visible: Some(false)
                }
            ),
            Ok((Plan::Sidebar(Some(false)), String::new()))
        );
    }

    /// What the shell does with queued calls: the app signed in on `/notes` with
    /// `library()`, the calls queued at start; each call's answer, the screen after, and
    /// the viewer state.
    async fn run_in_shell(
        calls: Vec<DeviceToolCall>,
    ) -> (
        Vec<DeviceToolResult>,
        String,
        Option<SourceViewer>,
        Option<Citation>,
    ) {
        use std::cell::RefCell;
        use std::rc::Rc;

        use dioxus::history::{History, MemoryHistory};

        use crate::api::Api;
        use crate::config::{Appearance, LocalConfig};
        use crate::state::{LiveStatus, LoadStatus, Session};

        type Seen = Rc<RefCell<Option<(Option<SourceViewer>, Option<Citation>)>>>;

        #[derive(Clone)]
        struct Props {
            history: Rc<MemoryHistory>,
            commands: Rc<RefCell<Vec<AgentCommand>>>,
            seen: Seen,
        }
        impl PartialEq for Props {
            fn eq(&self, other: &Self) -> bool {
                Rc::ptr_eq(&self.history, &other.history)
            }
        }

        #[allow(non_snake_case)]
        fn Harness(props: Props) -> Element {
            let history = props.history.clone();
            use_context_provider(move || history as Rc<dyn History>);
            let app = use_context_provider(|| AppCtx {
                config: Signal::new(LocalConfig {
                    service_url: Some("https://margin.example".into()),
                    appearance: Appearance::System,
                    sidebar_hidden: false,
                    sidebar_width: None,
                    zoom: 100,
                }),
                session: Signal::new(Some(Session {
                    api: Api::with_token("http://127.0.0.1:9", "x"),
                    me: None,
                })),
                signed_out_reason: Signal::new(None),
                lib: Signal::new(library()),
                lib_status: Signal::new(LoadStatus::Loaded),
                reload: Signal::new(0),
                live: Signal::new(LiveStatus::Live),
                search_open: Signal::new(true),
                drawer_open: Signal::new(false),
                collapsed: Signal::new(Default::default()),
                outline_drafts: Signal::new(Default::default()),
                ask_prompt: Signal::new(String::new()),
                uploads: Signal::new(Vec::new()),
                settings_open: Signal::new(Some(SettingsPane::Ai)),
                source_viewer: Signal::new(None),
                transcripts: Signal::new(Default::default()),
                shown_passage: Signal::new(None),
                device_frames: Signal::new(None),
                agent_commands: Signal::new(Default::default()),
                foreground: Signal::new(true),
            });
            let commands = props.commands.clone();
            use_hook(move || {
                let mut queue = app.agent_commands;
                queue.write().extend(commands.borrow_mut().drain(..));
            });
            let seen = props.seen.clone();
            use_effect(move || {
                let viewer = app.source_viewer.read().clone();
                let passage = app.shown_passage.read().clone();
                *seen.borrow_mut() = Some((viewer, passage));
            });
            rsx! { Router::<Route> {} }
        }

        let history = Rc::new(MemoryHistory::with_initial_path("/notes"));
        let mut answers = Vec::new();
        let mut commands = Vec::new();
        for call in calls {
            let (reply, answer) = tokio::sync::oneshot::channel();
            commands.push(AgentCommand {
                call,
                chat_id: "c1".into(),
                reply,
            });
            answers.push(answer);
        }
        let seen: Seen = Rc::default();
        let mut dom = VirtualDom::new_with_props(
            Harness,
            Props {
                history: history.clone(),
                commands: Rc::new(RefCell::new(commands)),
                seen: seen.clone(),
            },
        );
        dom.rebuild_in_place();
        let mut results = Vec::new();
        for mut answer in answers {
            let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if let Ok(result) = answer.try_recv() {
                        return result;
                    }
                    tokio::select! {
                        _ = dom.wait_for_work() => {}
                        _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
                    }
                    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
                }
            })
            .await
            .expect("the shell answered");
            results.push(result);
        }
        // Let the screen settle (a parked citation opens once its lesson is the screen).
        for _ in 0..20 {
            tokio::select! {
                _ = dom.wait_for_work() => {}
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
            }
            dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        }
        let (viewer, passage) = seen.borrow().clone().unwrap_or_default();
        (results, history.current_route(), viewer, passage)
    }

    fn done(message: &str) -> DeviceToolResult {
        DeviceToolResult::Ok {
            output: DeviceToolOutput::Done {
                message: message.into(),
            },
        }
    }

    #[tokio::test]
    async fn the_shell_navigates_and_answers() {
        let (results, route, viewer, _) = run_in_shell(vec![
            DeviceToolCall::Navigate {
                to: NavTarget::Lesson {
                    lesson_id: "nope".into(),
                    view: None,
                },
            },
            DeviceToolCall::Navigate {
                to: NavTarget::Lesson {
                    lesson_id: "l1".into(),
                    view: Some(LessonView::Notes),
                },
            },
        ])
        .await;
        assert_eq!(
            results,
            vec![
                DeviceToolResult::Err {
                    message: "There's no lesson with the id “nope”.".into()
                },
                done("Opened the notes of the lesson “Partial sums”."),
            ]
        );
        assert_eq!(route, "/lesson/l1/notes");
        assert_eq!(viewer, None);
    }

    #[tokio::test]
    async fn the_shell_opens_a_citation_in_its_lesson() {
        let (results, route, viewer, passage) = run_in_shell(vec![DeviceToolCall::OpenCitation {
            lesson_id: "l1".into(),
            n: 2,
        }])
        .await;
        assert_eq!(
            results,
            vec![done("Opened citation 2 of “Partial sums”: “rec.pdf”.")]
        );
        assert_eq!(route, "/lesson/l1");
        assert_eq!(
            viewer,
            Some(SourceViewer {
                lesson_id: "l1".into(),
                n: 2
            })
        );
        assert_eq!(passage, None);
    }

    #[tokio::test]
    async fn the_shell_shows_a_passage_where_it_is() {
        let (results, route, _, passage) = run_in_shell(vec![DeviceToolCall::ShowPassage {
            source_id: "src1".into(),
            page: Some(3),
            start_ms: None,
        }])
        .await;
        assert_eq!(results, vec![done("Opened “src1.pdf” at p. 3.")]);
        assert_eq!(route, "/notes");
        assert_eq!(passage.map(|c| c.excerpt), Some("passage 1".to_string()));
    }

    #[test]
    fn every_target_but_settings_is_a_route() {
        assert_eq!(nav_route(&NavTarget::Settings), None);
        assert_eq!(
            nav_route(&NavTarget::Lesson {
                lesson_id: "l".into(),
                view: Some(LessonView::Read)
            }),
            Some(Route::LessonRead { id: "l".into() })
        );
    }
}
