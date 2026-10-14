//! The app's copy of the library, loaded from `GET /api/library` and kept current by
//! applying every [`Event`] from the `/api/events` socket. Pure data: no Dioxus here,
//! so the reducer is unit-tested directly.

use std::collections::{BTreeMap, HashSet};

use margin_shared::{
    Event, Id, Lesson, LessonStatus, Library, Note, Proposal, Settings, Topic, Video, VideoStatus,
};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LibraryState {
    pub topics: BTreeMap<Id, Topic>,
    pub lessons: BTreeMap<Id, Lesson>,
    pub notes: BTreeMap<Id, Note>,
    pub videos: BTreeMap<Id, Video>,
    /// Outline proposals seen in this session (they are not part of the library).
    pub proposals: BTreeMap<Id, Proposal>,
    pub settings: Option<Settings>,
    pub server_version: Option<String>,
}

/// What the sidebar dot shows for a lesson's video.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoDot {
    /// No video, or the last render failed.
    None,
    InProgress,
    Ready,
}

impl LibraryState {
    pub fn from_library(library: Library) -> Self {
        let mut state = Self::default();
        state.replace_library(library);
        state
    }

    /// Replace topics, lessons, notes and videos with a fresh library, keeping proposals,
    /// settings and the server version.
    pub fn replace_library(&mut self, library: Library) {
        self.topics = library
            .topics
            .into_iter()
            .map(|t| (t.id.clone(), t))
            .collect();
        self.lessons = library
            .lessons
            .into_iter()
            .map(|l| (l.id.clone(), l))
            .collect();
        self.notes = library
            .notes
            .into_iter()
            .map(|n| (n.id.clone(), n))
            .collect();
        self.videos = BTreeMap::new();
        for video in library.videos {
            self.upsert_video(video);
        }
    }

    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Hello { server_version } => self.server_version = Some(server_version),
            Event::TopicChanged { topic } => {
                self.topics.insert(topic.id.clone(), topic);
            }
            Event::TopicDeleted { id } => self.remove_topic(&id),
            Event::LessonChanged { lesson } => {
                self.lessons.insert(lesson.id.clone(), lesson);
            }
            Event::LessonDelta { lesson_id, text } => {
                if let Some(lesson) = self.lessons.get_mut(&lesson_id) {
                    lesson.body_md.push_str(&text);
                    if lesson.status == LessonStatus::Queued {
                        lesson.status = LessonStatus::Generating;
                    }
                }
            }
            Event::LessonDeleted { id } => self.remove_lesson(&id),
            Event::NoteChanged { note } => {
                self.notes.insert(note.id.clone(), note);
            }
            Event::NoteDeleted { id } => {
                self.notes.remove(&id);
            }
            Event::VideoChanged { video } => self.upsert_video(video),
            Event::VideoDeleted { id } => {
                self.videos.remove(&id);
            }
            Event::ProposalChanged { proposal } => {
                self.proposals.insert(proposal.id.clone(), proposal);
            }
            Event::SettingsChanged { settings } => self.settings = Some(settings),
            // Sources and the embedding model aren't shown yet.
            Event::SourceChanged { .. }
            | Event::SourceDeleted { .. }
            | Event::EmbeddingModelChanged { .. } => {}
        }
    }

    /// One video per lesson: a video for a lesson replaces any other video of that lesson.
    pub fn upsert_video(&mut self, video: Video) {
        self.videos
            .retain(|id, v| *id == video.id || v.lesson_id != video.lesson_id);
        self.videos.insert(video.id.clone(), video);
    }

    fn remove_topic(&mut self, id: &str) {
        let mut doomed: HashSet<Id> = HashSet::new();
        doomed.insert(id.to_string());
        // Collect descendants (any depth).
        loop {
            let before = doomed.len();
            for topic in self.topics.values() {
                if let Some(parent) = &topic.parent_id
                    && doomed.contains(parent)
                {
                    doomed.insert(topic.id.clone());
                }
            }
            if doomed.len() == before {
                break;
            }
        }
        self.topics.retain(|tid, _| !doomed.contains(tid));
        let lessons: Vec<Id> = self
            .lessons
            .values()
            .filter(|l| doomed.contains(&l.topic_id))
            .map(|l| l.id.clone())
            .collect();
        for lesson in lessons {
            self.remove_lesson(&lesson);
        }
        self.notes.retain(|_, n| !doomed.contains(&n.topic_id));
    }

    fn remove_lesson(&mut self, id: &str) {
        self.lessons.remove(id);
        self.notes.retain(|_, n| n.lesson_id.as_deref() != Some(id));
        self.videos.retain(|_, v| v.lesson_id != id);
    }

    // ------------------------------------------------------------------
    // Queries
    // ------------------------------------------------------------------

    /// Child topics of `parent` (`None` = roots), in sibling order.
    pub fn child_topics(&self, parent: Option<&str>) -> Vec<&Topic> {
        let mut out: Vec<&Topic> = self
            .topics
            .values()
            .filter(|t| t.parent_id.as_deref() == parent)
            .collect();
        out.sort_by(|a, b| {
            a.position
                .cmp(&b.position)
                .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
        });
        out
    }

    pub fn lessons_in(&self, topic_id: &str) -> Vec<&Lesson> {
        let mut out: Vec<&Lesson> = self
            .lessons
            .values()
            .filter(|l| l.topic_id == topic_id)
            .collect();
        out.sort_by(|a, b| {
            a.position
                .cmp(&b.position)
                .then_with(|| a.created_at.cmp(&b.created_at))
        });
        out
    }

    pub fn notes_in_topic(&self, topic_id: &str) -> Vec<&Note> {
        let mut out: Vec<&Note> = self
            .notes
            .values()
            .filter(|n| n.topic_id == topic_id)
            .collect();
        out.sort_by_key(|n| std::cmp::Reverse(n.updated_at));
        out
    }

    pub fn notes_for_lesson(&self, lesson_id: &str) -> Vec<&Note> {
        let mut out: Vec<&Note> = self
            .notes
            .values()
            .filter(|n| n.lesson_id.as_deref() == Some(lesson_id))
            .collect();
        out.sort_by_key(|n| n.created_at);
        out
    }

    pub fn video_for_lesson(&self, lesson_id: &str) -> Option<&Video> {
        self.videos.values().find(|v| v.lesson_id == lesson_id)
    }

    pub fn video_dot(&self, lesson_id: &str) -> VideoDot {
        match self.video_for_lesson(lesson_id).map(|v| &v.status) {
            None | Some(VideoStatus::Failed { .. }) => VideoDot::None,
            Some(VideoStatus::Ready) => VideoDot::Ready,
            Some(_) => VideoDot::InProgress,
        }
    }

    /// The topic and its ancestors, root first.
    pub fn topic_path(&self, topic_id: &str) -> Vec<&Topic> {
        let mut path = Vec::new();
        let mut seen = HashSet::new();
        let mut current = self.topics.get(topic_id);
        while let Some(topic) = current {
            if !seen.insert(topic.id.clone()) {
                break;
            }
            path.push(topic);
            current = topic.parent_id.as_deref().and_then(|p| self.topics.get(p));
        }
        path.reverse();
        path
    }

    /// "Subject / Topic" breadcrumb text for a topic.
    pub fn topic_breadcrumb(&self, topic_id: &str) -> String {
        self.topic_path(topic_id)
            .iter()
            .map(|t| t.title.as_str())
            .collect::<Vec<_>>()
            .join(" / ")
    }

    /// Number of lessons in a topic and all of its descendants.
    pub fn lesson_count_deep(&self, topic_id: &str) -> usize {
        let mut count = self.lessons_in(topic_id).len();
        for child in self.child_topics(Some(topic_id)) {
            count += self.lesson_count_deep(&child.id);
        }
        count
    }

    /// The lesson after `lesson_id` in its topic, if any.
    pub fn next_lesson(&self, lesson_id: &str) -> Option<&Lesson> {
        let lesson = self.lessons.get(lesson_id)?;
        let siblings = self.lessons_in(&lesson.topic_id);
        let index = siblings.iter().position(|l| l.id == lesson_id)?;
        siblings.get(index + 1).copied()
    }

    pub fn first_lesson_in(&self, topic_id: &str) -> Option<&Lesson> {
        self.lessons_in(topic_id).into_iter().next()
    }

    /// All notes, newest edit first.
    pub fn all_notes(&self) -> Vec<&Note> {
        let mut out: Vec<&Note> = self.notes.values().collect();
        out.sort_by_key(|n| std::cmp::Reverse(n.updated_at));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use margin_shared::{AskRequest, Level, ProposalStatus, VideoMedia};

    fn ts(n: i64) -> chrono::DateTime<Utc> {
        Utc.timestamp_opt(1_760_000_000 + n, 0).unwrap()
    }

    pub fn topic(id: &str, parent: Option<&str>, title: &str, position: i64) -> Topic {
        Topic {
            id: id.into(),
            parent_id: parent.map(Into::into),
            title: title.into(),
            position,
            created_at: ts(0),
            updated_at: ts(0),
        }
    }

    pub fn lesson(id: &str, topic_id: &str, position: i64, status: LessonStatus) -> Lesson {
        Lesson {
            id: id.into(),
            topic_id: topic_id.into(),
            title: format!("Lesson {id}"),
            summary: "summary".into(),
            position,
            level: Level::Intro,
            minutes: 9,
            body_md: String::new(),
            status,
            created_at: ts(position),
            updated_at: ts(position),
        }
    }

    pub fn note(id: &str, topic_id: &str, lesson_id: Option<&str>) -> Note {
        Note {
            id: id.into(),
            topic_id: topic_id.into(),
            lesson_id: lesson_id.map(Into::into),
            title: format!("Note {id}"),
            body_md: "body".into(),
            quote: None,
            created_at: ts(1),
            updated_at: ts(1),
        }
    }

    pub fn video(id: &str, lesson_id: &str, status: VideoStatus) -> Video {
        Video {
            id: id.into(),
            lesson_id: lesson_id.into(),
            status,
            scene_code: None,
            log: String::new(),
            duration_seconds: None,
            media: None,
            created_at: ts(0),
            updated_at: ts(0),
        }
    }

    fn sample() -> LibraryState {
        LibraryState::from_library(Library {
            topics: vec![
                topic("s", None, "Signals", 0),
                topic("f", Some("s"), "Fourier", 1),
                topic("c", Some("s"), "Complex", 0),
                topic("deep", Some("f"), "Deeper", 0),
            ],
            lessons: vec![
                lesson("l2", "f", 2, LessonStatus::Ready),
                lesson("l1", "f", 1, LessonStatus::Ready),
                lesson("ld", "deep", 0, LessonStatus::Ready),
            ],
            notes: vec![note("n1", "f", Some("l1")), note("n2", "f", None)],
            videos: vec![video("v1", "l1", VideoStatus::Ready)],
            sources: vec![],
        })
    }

    #[test]
    fn hello_records_server_version() {
        let mut s = sample();
        s.apply(Event::Hello {
            server_version: "0.1.0".into(),
        });
        assert_eq!(s.server_version.as_deref(), Some("0.1.0"));
    }

    #[test]
    fn topic_changed_inserts_and_updates() {
        let mut s = sample();
        s.apply(Event::TopicChanged {
            topic: topic("new", None, "Linear Algebra", 1),
        });
        assert_eq!(s.child_topics(None).len(), 2);
        s.apply(Event::TopicChanged {
            topic: topic("f", Some("s"), "Fourier series", 1),
        });
        assert_eq!(s.topics["f"].title, "Fourier series");
        // Sibling order is by position.
        let kids: Vec<_> = s
            .child_topics(Some("s"))
            .iter()
            .map(|t| t.id.clone())
            .collect();
        assert_eq!(kids, vec!["c", "f"]);
    }

    #[test]
    fn topic_deleted_cascades_to_descendants() {
        let mut s = sample();
        s.apply(Event::TopicDeleted { id: "f".into() });
        assert!(!s.topics.contains_key("f"));
        assert!(!s.topics.contains_key("deep"));
        assert!(s.topics.contains_key("c"));
        assert!(s.lessons.is_empty());
        assert!(s.notes.is_empty());
        assert!(s.videos.is_empty());
    }

    #[test]
    fn lesson_changed_replaces_body() {
        let mut s = sample();
        let mut l = lesson("l1", "f", 1, LessonStatus::Ready);
        l.body_md = "# Full".into();
        s.apply(Event::LessonChanged { lesson: l });
        assert_eq!(s.lessons["l1"].body_md, "# Full");
    }

    #[test]
    fn lesson_delta_appends_in_order() {
        let mut s = sample();
        s.apply(Event::LessonChanged {
            lesson: lesson("g", "f", 3, LessonStatus::Queued),
        });
        s.apply(Event::LessonDelta {
            lesson_id: "g".into(),
            text: "Hello ".into(),
        });
        s.apply(Event::LessonDelta {
            lesson_id: "g".into(),
            text: "world".into(),
        });
        assert_eq!(s.lessons["g"].body_md, "Hello world");
        assert_eq!(s.lessons["g"].status, LessonStatus::Generating);
        // Unknown lesson: ignored.
        s.apply(Event::LessonDelta {
            lesson_id: "missing".into(),
            text: "x".into(),
        });
        assert!(!s.lessons.contains_key("missing"));
    }

    #[test]
    fn lesson_deleted_removes_its_notes_and_video() {
        let mut s = sample();
        s.apply(Event::LessonDeleted { id: "l1".into() });
        assert!(!s.lessons.contains_key("l1"));
        assert!(!s.notes.contains_key("n1"));
        assert!(s.notes.contains_key("n2"));
        assert!(s.videos.is_empty());
    }

    #[test]
    fn note_changed_and_deleted() {
        let mut s = sample();
        let mut n = note("n3", "c", None);
        n.title = "Euler".into();
        s.apply(Event::NoteChanged { note: n });
        assert_eq!(s.notes["n3"].title, "Euler");
        s.apply(Event::NoteDeleted { id: "n3".into() });
        assert!(!s.notes.contains_key("n3"));
    }

    #[test]
    fn video_changed_tracks_status_and_keeps_one_per_lesson() {
        let mut s = sample();
        s.apply(Event::VideoChanged {
            video: video("v2", "l2", VideoStatus::Queued),
        });
        assert_eq!(s.video_dot("l2"), VideoDot::InProgress);
        s.apply(Event::VideoChanged {
            video: video("v2", "l2", VideoStatus::Voicing { done: 1, total: 4 }),
        });
        assert_eq!(
            s.video_for_lesson("l2").unwrap().status,
            VideoStatus::Voicing { done: 1, total: 4 }
        );
        let mut ready = video("v2", "l2", VideoStatus::Ready);
        ready.media = Some(VideoMedia {
            mp4_url: "https://x/video.mp4".into(),
            storyboard_url: "https://x/storyboard.png".into(),
            captions_url: None,
            poster_url: None,
            expires_at: ts(100),
        });
        s.apply(Event::VideoChanged { video: ready });
        assert_eq!(s.video_dot("l2"), VideoDot::Ready);
        // A different video id for the same lesson replaces the old one.
        s.apply(Event::VideoChanged {
            video: video(
                "v3",
                "l2",
                VideoStatus::Failed {
                    error: "boom".into(),
                },
            ),
        });
        assert_eq!(s.videos.values().filter(|v| v.lesson_id == "l2").count(), 1);
        assert_eq!(s.video_dot("l2"), VideoDot::None);
    }

    #[test]
    fn video_deleted() {
        let mut s = sample();
        s.apply(Event::VideoDeleted { id: "v1".into() });
        assert_eq!(s.video_dot("l1"), VideoDot::None);
    }

    #[test]
    fn proposal_changed_is_tracked() {
        let mut s = sample();
        let p = Proposal {
            id: "p1".into(),
            request: AskRequest {
                prompt: "Fourier".into(),
                lesson_count: 3,
                level: Level::Intro,
                with_videos: false,
                with_quiz: true,
                parent_topic_id: None,
                source_ids: vec![],
            },
            status: ProposalStatus::Drafting,
            outline: None,
            created_at: ts(0),
        };
        s.apply(Event::ProposalChanged {
            proposal: p.clone(),
        });
        assert_eq!(s.proposals["p1"].status, ProposalStatus::Drafting);
        let mut ready = p;
        ready.status = ProposalStatus::Ready;
        s.apply(Event::ProposalChanged { proposal: ready });
        assert_eq!(s.proposals["p1"].status, ProposalStatus::Ready);
    }

    #[test]
    fn settings_changed_is_stored() {
        let mut s = sample();
        let settings = Settings {
            openai_base_url: "https://api.openai.com/v1".into(),
            openai_model: "gpt-5".into(),
            has_api_key: true,
            voice: None,
            render_width: 1920,
            render_height: 1080,
            render_fps: 30,
        };
        s.apply(Event::SettingsChanged {
            settings: settings.clone(),
        });
        assert_eq!(s.settings, Some(settings));
    }

    #[test]
    fn replace_library_keeps_proposals_and_settings() {
        let mut s = sample();
        s.server_version = Some("1".into());
        s.replace_library(Library {
            topics: vec![],
            lessons: vec![],
            notes: vec![],
            videos: vec![],
            sources: vec![],
        });
        assert!(s.topics.is_empty());
        assert_eq!(s.server_version.as_deref(), Some("1"));
    }

    #[test]
    fn queries() {
        let s = sample();
        let ids: Vec<_> = s.lessons_in("f").iter().map(|l| l.id.clone()).collect();
        assert_eq!(ids, vec!["l1", "l2"]);
        assert_eq!(s.next_lesson("l1").unwrap().id, "l2");
        assert!(s.next_lesson("l2").is_none());
        assert_eq!(s.topic_breadcrumb("deep"), "Signals / Fourier / Deeper");
        assert_eq!(s.lesson_count_deep("s"), 3);
        assert_eq!(s.notes_for_lesson("l1").len(), 1);
        assert_eq!(s.first_lesson_in("f").unwrap().id, "l1");
    }
}
