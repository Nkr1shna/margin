//! The app's copy of the library, loaded from `GET /api/library` and kept current by
//! applying every [`Event`] from the `/api/events` socket. Pure data: no Dioxus here,
//! so the reducer is unit-tested directly.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use margin_shared::{
    Agent, Chat, ChatDetail, ChatGptStatus, ChatItem, DeltaOutcome, Device, EmbeddingModel, Event,
    Group, Id, Invitation, ItemStatus, Lesson, LessonStatus, Library, ModelInfo, Note, Person,
    Proposal, ProposalStatus, Provider, Settings, Source, Topic, Video, VideoStatus, Workspace,
};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LibraryState {
    pub topics: BTreeMap<Id, Topic>,
    pub lessons: BTreeMap<Id, Lesson>,
    pub notes: BTreeMap<Id, Note>,
    pub videos: BTreeMap<Id, Video>,
    /// Files added as sources: a topic's, and those not attached to one yet.
    pub sources: BTreeMap<Id, Source>,
    /// Outline proposals seen in this session (they are not part of the library).
    pub proposals: BTreeMap<Id, Proposal>,
    pub settings: Option<Settings>,
    /// The service's embedding model, once fetched (`GET /api/embedding`) or announced.
    pub embedding: Option<EmbeddingModel>,
    pub server_version: Option<String>,
    /// Chats for the sidebar (`GET /api/chats`), kept current by `ChatChanged` /
    /// `ChatDeleted`.
    pub chats: BTreeMap<Id, Chat>,
    /// Whether `chats` holds the service's list yet (the sidebar says "No chats yet" only
    /// then).
    pub chats_loaded: bool,
    /// Transcripts of the chats opened this session (`GET /api/chats/{id}`), each ordered
    /// by `seq`. Item events update only transcripts that are loaded.
    pub chat_items: BTreeMap<Id, Vec<ChatItem>>,
    /// Loaded transcripts that missed something (a delta past the text they have, or for
    /// an item they don't have): the chat screen fetches them again.
    pub stale_chats: BTreeSet<Id>,
    /// Attached folders of every device (`GET /api/workspaces`).
    pub workspaces: BTreeMap<Id, Workspace>,
    /// A folder's files as the service has them (`GET /api/workspaces/{id}/files`), for
    /// the folders whose list was opened this session; kept current by source events.
    /// Folder files are never in `sources`.
    pub folder_files: BTreeMap<Id, BTreeMap<Id, Source>>,
    /// Devices once fetched (`GET /api/devices`), with presence from `DeviceChanged`.
    pub devices: BTreeMap<Id, Device>,
    /// What Settings can pick the pipeline from (`GET /api/pipeline/variants`), once
    /// fetched; fetched again each time the Pipeline section opens.
    pub pipeline_variants: Option<margin_shared::pipeline::PipelineVariants>,
    /// The models each provider offers (`GET routes::providers_models`), for the providers
    /// whose list was fetched; dropped when the provider loses its credential.
    pub provider_models: HashMap<Provider, Vec<ModelInfo>>,
    /// Moves each time a provider's model list goes stale (`forget_models`); Settings
    /// keys its slots on it, so they fetch the list again.
    pub models_epoch: HashMap<Provider, u32>,
    /// The ChatGPT sign-in as last fetched (`GET routes::PROVIDERS_CHATGPT`) or started;
    /// `None` until then (see [`LibraryState::chatgpt_status`]).
    pub chatgpt: Option<ChatGptStatus>,
    /// Everyone who may sign in (`GET /api/people`), kept current by `PersonChanged` and
    /// `MembershipChanged`.
    pub people: BTreeMap<Id, Person>,
    /// The organisation's groups by path (`GET /api/groups`); the tree is the paths.
    pub groups: BTreeMap<String, Group>,
    /// Agents the caller may see (`GET /api/agents`): their own, or everyone's for an
    /// admin.
    pub agents: BTreeMap<Id, Agent>,
    /// Invitations (`GET /api/invitations`), fetched only for admins.
    pub invitations: BTreeMap<Id, Invitation>,
    /// Whether `people`, `groups` and `agents` hold the service's lists yet.
    pub org_loaded: bool,
}

/// What the sidebar dot shows for a lesson's video.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoDot {
    /// No video (the sidebar shows nothing).
    None,
    InProgress,
    Ready,
    /// The last render failed.
    Failed,
}

impl LibraryState {
    pub fn from_library(library: Library) -> Self {
        let mut state = Self::default();
        state.replace_library(library);
        state
    }

    /// New settings: the ChatGPT sign-in follows the account they carry (signed in, or
    /// signed out where it was signed in; a sign-in still waiting or failed stays), and a
    /// provider without its credential forgets its models.
    pub fn set_settings(&mut self, settings: Settings) {
        match (&settings.chatgpt, &self.chatgpt) {
            (Some(account), current) => {
                // A failed attempt noted on the same account stays noted.
                let last_error = match current {
                    Some(ChatGptStatus::SignedIn {
                        account: was,
                        last_error,
                    }) if was.email == account.email => last_error.clone(),
                    _ => None,
                };
                self.chatgpt = Some(ChatGptStatus::SignedIn {
                    account: account.clone(),
                    last_error,
                })
            }
            (None, Some(ChatGptStatus::SignedIn { .. })) => {
                self.chatgpt = Some(ChatGptStatus::SignedOut)
            }
            (None, _) => {}
        }
        self.provider_models
            .retain(|p, _| settings.has_credential(*p));
        // Another base URL, or another ChatGPT account: another list of models.
        let email = |s: &Settings| s.chatgpt.as_ref().map(|a| a.email.clone());
        let (base_moved, account_moved) = match &self.settings {
            Some(old) => (
                old.openai_base_url != settings.openai_base_url,
                email(old) != email(&settings),
            ),
            None => (false, false),
        };
        if base_moved {
            self.forget_models(Provider::OpenAi);
        }
        if account_moved {
            self.forget_models(Provider::ChatGpt);
        }
        self.settings = Some(settings);
    }

    /// `provider`'s credential or base URL changed: its model list is stale. It is
    /// dropped and `models_epoch` moves, so Settings fetches it again.
    pub fn forget_models(&mut self, provider: Provider) {
        self.provider_models.remove(&provider);
        *self.models_epoch.entry(provider).or_default() += 1;
    }

    /// The key Settings mounts a provider's slots under: again when the credential
    /// comes or goes, and when the list went stale.
    pub fn slots_key(&self, settings: &Settings, provider: Provider) -> String {
        format!(
            "{}-{}",
            settings.has_credential(provider),
            self.models_epoch.get(&provider).copied().unwrap_or(0)
        )
    }

    /// Where the ChatGPT sign-in is: as fetched, else as the settings say.
    pub fn chatgpt_status(&self) -> ChatGptStatus {
        if let Some(status) = &self.chatgpt {
            return status.clone();
        }
        match self.settings.as_ref().and_then(|s| s.chatgpt.clone()) {
            Some(account) => ChatGptStatus::SignedIn {
                account,
                last_error: None,
            },
            None => ChatGptStatus::SignedOut,
        }
    }

    /// Whether `provider` can answer (a key, or a ChatGPT sign-in); unknown settings
    /// count as yes, so nothing is refused before they arrive.
    pub fn provider_ready(&self, provider: Provider) -> bool {
        self.settings
            .as_ref()
            .is_none_or(|s| s.has_credential(provider))
    }

    /// Replace topics, lessons, notes, videos and sources with a fresh library, keeping
    /// proposals, settings, the embedding model and the server version.
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
        self.sources = library
            .sources
            .into_iter()
            .map(|s| (s.id.clone(), s))
            .collect();
    }

    /// The service's chat list (`GET /api/chats`). Transcripts already loaded stay,
    /// unless their chat is gone.
    pub fn replace_chats(&mut self, chats: Vec<Chat>) {
        self.chats = chats.into_iter().map(|c| (c.id.clone(), c)).collect();
        self.chats_loaded = true;
        let chats = &self.chats;
        self.chat_items.retain(|id, _| chats.contains_key(id));
    }

    /// A chat as the answer to a request brought it. Events may have brought a newer copy
    /// while the request was out, so the newer `updated_at` wins.
    pub fn upsert_chat(&mut self, chat: Chat) {
        match self.chats.get(&chat.id) {
            Some(known) if known.updated_at > chat.updated_at => {}
            _ => {
                self.chats.insert(chat.id.clone(), chat);
            }
        }
    }

    /// An item as the answer to a request brought it (the message just sent), merged like
    /// a fetched transcript's. Ignored while the chat's transcript isn't loaded.
    pub fn upsert_chat_item(&mut self, item: ChatItem) {
        if let Some(items) = self.chat_items.get_mut(&item.chat_id) {
            merge_items(items, vec![item]);
        }
    }

    /// A chat and its transcript from `GET /api/chats/{id}` (or `POST /api/chats`).
    /// Items that events changed while the request was out keep their newer state, and
    /// items only events brought stay.
    pub fn load_chat(&mut self, detail: ChatDetail) {
        let id = detail.chat.id.clone();
        self.upsert_chat(detail.chat);
        let items = self.chat_items.entry(id.clone()).or_default();
        merge_items(items, detail.items);
        self.stale_chats.remove(&id);
    }

    /// Chats for the sidebar, the most recently active first.
    pub fn chats_by_recency(&self) -> Vec<&Chat> {
        let mut chats: Vec<&Chat> = self.chats.values().collect();
        chats.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.id.cmp(&b.id)));
        chats
    }

    pub fn apply(&mut self, event: Event) {
        match event {
            Event::PersonChanged { person } => self.upsert_person(person),
            Event::GroupChanged { group } => {
                self.groups.insert(group.path.clone(), group);
            }
            Event::GroupDeleted { path } => {
                self.groups.remove(&path);
            }
            Event::MembershipChanged { person_id, groups } => {
                if let Some(person) = self.people.get_mut(&person_id) {
                    person.groups = groups;
                }
                self.count_members();
            }
            Event::AgentChanged { agent } => {
                self.agents.insert(agent.id.clone(), agent);
            }
            Event::AgentDeleted { id } => {
                self.agents.remove(&id);
            }
            Event::InvitationChanged { invitation } => {
                self.invitations.insert(invitation.id.clone(), invitation);
            }
            Event::InvitationDeleted { id } => {
                self.invitations.remove(&id);
            }
            // The live loop reloads the library when it is this person's (`main.rs`).
            Event::AccessChanged { .. } => {}
            // L2: L2-app drops the revoked units and keeps the admins' audit rows.
            Event::AudienceRevoked { .. } | Event::AuditAppended { .. } => {}
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
            Event::SettingsChanged { settings } => self.set_settings(settings),
            Event::SourceChanged { source } => match &source.workspace_id {
                Some(workspace_id) => {
                    if let Some(files) = self.folder_files.get_mut(workspace_id) {
                        files.insert(source.id.clone(), source);
                    }
                }
                None => {
                    self.sources.insert(source.id.clone(), source);
                }
            },
            Event::SourceDeleted { id } => {
                self.sources.remove(&id);
                for files in self.folder_files.values_mut() {
                    files.remove(&id);
                }
            }
            Event::EmbeddingModelChanged { model } => self.embedding = Some(model),
            Event::ChatChanged { chat } => {
                self.chats.insert(chat.id.clone(), chat);
            }
            Event::ChatDeleted { id } => {
                self.chats.remove(&id);
                self.chat_items.remove(&id);
                self.stale_chats.remove(&id);
            }
            Event::ChatItemChanged { item } => {
                if let Some(items) = self.chat_items.get_mut(&item.chat_id) {
                    match items.iter_mut().find(|i| i.id == item.id) {
                        Some(existing) => *existing = item,
                        None => {
                            let at = items.partition_point(|i| i.seq < item.seq);
                            items.insert(at, item);
                        }
                    }
                }
            }
            Event::ChatItemDelta {
                chat_id,
                item_id,
                offset,
                text,
            } => {
                let Some(items) = self.chat_items.get_mut(&chat_id) else {
                    return;
                };
                let outcome = items
                    .iter_mut()
                    .find(|i| i.id == item_id)
                    .map(|item| item.kind.apply_delta(offset, &text));
                // Missed text, or an item this copy never heard of: the item's next
                // ChatItemChanged fills it in, and the open chat is fetched again so
                // the answer doesn't stall until then.
                if matches!(outcome, None | Some(DeltaOutcome::Gap)) {
                    self.stale_chats.insert(chat_id);
                }
            }
            Event::WorkspaceChanged { workspace } => {
                self.workspaces.insert(workspace.id.clone(), workspace);
            }
            Event::WorkspaceDeleted { id } => {
                self.workspaces.remove(&id);
                self.folder_files.remove(&id);
            }
            Event::DeviceChanged { mut device } => {
                // `current` is per device; broadcasts always say false.
                if let Some(known) = self.devices.get(&device.id) {
                    device.current = known.current;
                }
                self.devices.insert(device.id.clone(), device);
            }
            Event::DeviceDeleted { id } => {
                self.devices.remove(&id);
                self.workspaces.retain(|_, w| w.device_id != id);
                let workspaces = &self.workspaces;
                self.folder_files.retain(|w, _| workspaces.contains_key(w));
            }
            // Documents and traces are for agents and the eval runner; the app shows
            // neither (yet).
            Event::DocumentChanged { .. } | Event::Trace { .. } => {}
        }
    }

    /// The folders exactly as `GET /api/workspaces` lists them.
    pub fn replace_workspaces(&mut self, workspaces: Vec<Workspace>) {
        self.workspaces = workspaces.into_iter().map(|w| (w.id.clone(), w)).collect();
        let workspaces = &self.workspaces;
        self.folder_files.retain(|w, _| workspaces.contains_key(w));
    }

    /// A folder's files as `GET /api/workspaces/{id}/files` lists them.
    pub fn replace_folder_files(&mut self, workspace_id: &str, files: Vec<Source>) {
        self.folder_files.insert(
            workspace_id.to_string(),
            files.into_iter().map(|s| (s.id.clone(), s)).collect(),
        );
    }

    /// A folder's files, by path.
    pub fn folder_files_in(&self, workspace_id: &str) -> Option<Vec<&Source>> {
        let files = self.folder_files.get(workspace_id)?;
        let mut list: Vec<&Source> = files.values().collect();
        list.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        Some(list)
    }

    /// Folders in the order the screens list them: this device's first, then by device
    /// name and folder name.
    pub fn workspaces_in_order(&self, my_device: Option<&str>) -> Vec<&Workspace> {
        let mut list: Vec<&Workspace> = self.workspaces.values().collect();
        list.sort_by(|a, b| {
            let mine = |w: &Workspace| Some(w.device_id.as_str()) != my_device;
            mine(a)
                .cmp(&mine(b))
                .then(a.device_name.cmp(&b.device_name))
                .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then(a.id.cmp(&b.id))
        });
        list
    }

    /// Chats that have a folder attached.
    pub fn chats_with_workspace(&self, workspace_id: &str) -> usize {
        self.chats
            .values()
            .filter(|c| c.workspace_ids.iter().any(|w| w == workspace_id))
            .count()
    }

    /// Devices as `GET /api/devices` lists them (this device marked `current`), over any
    /// known before.
    pub fn merge_devices(&mut self, devices: Vec<Device>) {
        for device in devices {
            self.devices.insert(device.id.clone(), device);
        }
    }

    /// Devices exactly as `GET /api/devices` lists them: one revoked since the last look
    /// goes (Settings → Devices shows this list, with who is online).
    pub fn replace_devices(&mut self, devices: Vec<Device>) {
        self.devices = devices.into_iter().map(|d| (d.id.clone(), d)).collect();
    }

    /// A person as an event or a request brought them. The caller's own `settings` aren't
    /// in another device's event: a row without them keeps the ones known.
    pub fn upsert_person(&mut self, mut person: Person) {
        if let Some(known) = self.people.get(&person.id)
            && person.settings == Default::default()
        {
            person.settings = known.settings.clone();
        }
        self.people.insert(person.id.clone(), person);
        self.count_members();
    }

    /// The people exactly as `GET /api/people` lists them.
    pub fn replace_people(&mut self, people: Vec<Person>) {
        self.people = people.into_iter().map(|p| (p.id.clone(), p)).collect();
        self.count_members();
    }

    /// The groups exactly as `GET /api/groups` lists them.
    pub fn replace_groups(&mut self, groups: Vec<Group>) {
        self.groups = groups.into_iter().map(|g| (g.path.clone(), g)).collect();
        self.count_members();
    }

    /// The agents exactly as `GET /api/agents` lists them.
    pub fn replace_agents(&mut self, agents: Vec<Agent>) {
        self.agents = agents.into_iter().map(|a| (a.id.clone(), a)).collect();
    }

    /// The invitations exactly as `GET /api/invitations` lists them.
    pub fn replace_invitations(&mut self, invitations: Vec<Invitation>) {
        self.invitations = invitations.into_iter().map(|i| (i.id.clone(), i)).collect();
    }

    /// Each group's direct member count again from the people known (memberships move
    /// with `MembershipChanged`, which says nothing of the groups). Only once the people
    /// are known: before that the service's counts stand.
    fn count_members(&mut self) {
        if self.people.is_empty() {
            return;
        }
        for group in self.groups.values_mut() {
            group.members = self
                .people
                .values()
                .filter(|p| p.groups.contains(&group.path))
                .count() as u32;
        }
    }

    /// People in the order Organisation lists them: the owner, admins, members; then by
    /// handle.
    pub fn people_in_order(&self) -> Vec<&Person> {
        let mut list: Vec<&Person> = self.people.values().collect();
        list.sort_by(|a, b| a.org_role.cmp(&b.org_role).then(a.handle.cmp(&b.handle)));
        list
    }

    /// Agents of `owner_id`, by name.
    pub fn agents_of(&self, owner_id: &str) -> Vec<&Agent> {
        let mut list: Vec<&Agent> = self
            .agents
            .values()
            .filter(|a| a.owner_id == owner_id)
            .collect();
        list.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        list
    }

    /// Invitations, newest first.
    pub fn invitations_in_order(&self) -> Vec<&Invitation> {
        let mut list: Vec<&Invitation> = self.invitations.values().collect();
        list.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.id.cmp(&b.id)));
        list
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
        self.sources
            .retain(|_, s| s.topic_id.as_ref().is_none_or(|t| !doomed.contains(t)));
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
            None => VideoDot::None,
            Some(VideoStatus::Failed { .. }) => VideoDot::Failed,
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

    /// A topic's sources: loose files first, then each folder's, by name.
    pub fn sources_in_topic(&self, topic_id: &str) -> Vec<&Source> {
        let mut out: Vec<&Source> = self
            .sources
            .values()
            .filter(|s| s.topic_id.as_deref() == Some(topic_id))
            .collect();
        out.sort_by(|a, b| {
            a.folder
                .cmp(&b.folder)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        out
    }

    /// All notes, newest edit first.
    pub fn all_notes(&self) -> Vec<&Note> {
        let mut out: Vec<&Note> = self.notes.values().collect();
        out.sort_by_key(|n| std::cmp::Reverse(n.updated_at));
        out
    }

    /// Where each piece of background work stands: lessons being written, videos being
    /// made (by lesson) and outlines being drafted.
    pub fn milestones(&self) -> Milestones {
        let mut out = Milestones::new();
        for lesson in self.lessons.values() {
            let stage = match lesson.status {
                LessonStatus::Queued | LessonStatus::Generating => Stage::Working,
                LessonStatus::Ready => Stage::Done,
                LessonStatus::Failed { .. } => Stage::Failed,
            };
            out.insert(Work::Lesson(lesson.id.clone()), stage);
        }
        for video in self.videos.values() {
            let stage = match video.status {
                VideoStatus::Ready => Stage::Done,
                VideoStatus::Failed { .. } => Stage::Failed,
                _ => Stage::Working,
            };
            out.insert(Work::Video(video.lesson_id.clone()), stage);
        }
        for proposal in self.proposals.values() {
            let stage = match proposal.status {
                ProposalStatus::Drafting => Stage::Working,
                ProposalStatus::Ready
                | ProposalStatus::Accepted { .. }
                | ProposalStatus::Superseded { .. } => Stage::Done,
                ProposalStatus::Failed { .. } => Stage::Failed,
            };
            out.insert(Work::Proposal(proposal.id.clone()), stage);
        }
        out
    }

    /// What finished (or failed) since `before`, as sentences for screen readers. Work
    /// that was already finished, or that appears finished (a reload), isn't announced.
    pub fn announcements(&self, before: &Milestones) -> Vec<String> {
        let mut out = Vec::new();
        for (work, stage) in self.milestones() {
            if before.get(&work) != Some(&Stage::Working) || stage == Stage::Working {
                continue;
            }
            let done = stage == Stage::Done;
            let lesson_title = |id: &str| {
                self.lessons
                    .get(id)
                    .map(|l| l.title.clone())
                    .unwrap_or_else(|| "a lesson".into())
            };
            out.push(match work {
                Work::Lesson(id) if done => format!("Lesson written: {}.", lesson_title(&id)),
                Work::Lesson(id) => format!("Couldn't write the lesson {}.", lesson_title(&id)),
                Work::Video(id) if done => format!("Video ready: {}.", lesson_title(&id)),
                Work::Video(id) => format!("Couldn't make the video for {}.", lesson_title(&id)),
                Work::Proposal(id) if done => {
                    let n = self
                        .proposals
                        .get(&id)
                        .and_then(|p| p.outline.as_ref())
                        .map_or(0, |o| o.lessons.len());
                    match n {
                        1 => "Outline ready: 1 lesson.".to_string(),
                        n => format!("Outline ready: {n} lessons."),
                    }
                }
                Work::Proposal(_) => "Couldn't draft an outline.".to_string(),
            });
        }
        out
    }
}

/// A piece of background work the app follows (see [`LibraryState::milestones`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Work {
    Lesson(Id),
    /// A lesson's video, keyed by the lesson (a re-render replaces the video).
    Video(Id),
    Proposal(Id),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Working,
    Done,
    Failed,
}

pub type Milestones = BTreeMap<Work, Stage>;

/// Merges fetched `items` into a loaded transcript by id, kept in `seq` order. The copy
/// already there wins when it is newer, or when it is the same item still streaming with
/// more text than the fetched one (deltas that arrived after the fetch was answered).
fn merge_items(items: &mut Vec<ChatItem>, fetched: Vec<ChatItem>) {
    let streamed = |i: &ChatItem| i.kind.streaming_text().map_or(0, str::len);
    for item in fetched {
        match items.iter_mut().find(|i| i.id == item.id) {
            Some(existing) => {
                let further = existing.status == ItemStatus::InProgress
                    && item.status == ItemStatus::InProgress
                    && streamed(existing) > streamed(&item);
                if existing.updated_at <= item.updated_at && !further {
                    *existing = item;
                }
            }
            None => {
                let at = items.partition_point(|i| i.seq < item.seq);
                items.insert(at, item);
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use margin_shared::{
        AskRequest, ChatItemKind, ChatState, EmbeddingModelState, ItemStatus, Level,
        ReasoningEffort, SourceKind, SourceStatus, VideoMedia,
    };

    fn ts(n: i64) -> chrono::DateTime<Utc> {
        Utc.timestamp_opt(1_760_000_000 + n, 0).unwrap()
    }

    pub fn topic(id: &str, parent: Option<&str>, title: &str, position: i64) -> Topic {
        Topic {
            access: Vec::new(),
            deny: Vec::new(),
            owner: None,
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
            access: Vec::new(),
            deny: Vec::new(),
            owner: None,
            citations: Vec::new(),
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
            written_with: None,
            audience_narrowed: false,
        }
    }

    pub fn note(id: &str, topic_id: &str, lesson_id: Option<&str>) -> Note {
        Note {
            access: Vec::new(),
            deny: Vec::new(),
            owner: None,
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

    pub fn source(id: &str, topic_id: Option<&str>, name: &str, folder: Option<&str>) -> Source {
        Source {
            access: Vec::new(),
            deny: Vec::new(),
            owner: None,
            media_url: None,
            id: id.into(),
            topic_id: topic_id.map(Into::into),
            name: name.into(),
            folder: folder.map(Into::into),
            kind: SourceKind::for_name(name).unwrap_or(SourceKind::Text),
            bytes: 100,
            status: SourceStatus::Queued,
            created_at: ts(0),
            workspace_id: None,
            modified: None,
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
            sources: vec![
                source("src1", Some("f"), "paper.pdf", None),
                source("src2", Some("deep"), "main.rs", Some("code")),
                source("src3", Some("c"), "euler.md", None),
                source("src4", None, "draft.txt", None),
            ],
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
        // Sources of the deleted topics go too; others, and unattached ones, stay.
        let left: Vec<_> = s.sources.keys().cloned().collect();
        assert_eq!(left, vec!["src3", "src4"]);
    }

    #[test]
    fn source_changed_and_deleted() {
        let mut s = sample();
        assert_eq!(s.sources.len(), 4);
        let mut src = source("src4", None, "draft.txt", None);
        src.status = SourceStatus::Processing { done: 1, total: 3 };
        s.apply(Event::SourceChanged { source: src });
        assert_eq!(
            s.sources["src4"].status,
            SourceStatus::Processing { done: 1, total: 3 }
        );
        // Attached to a topic by accepting a proposal.
        s.apply(Event::SourceChanged {
            source: source("src4", Some("c"), "draft.txt", None),
        });
        assert_eq!(s.sources["src4"].topic_id.as_deref(), Some("c"));
        s.apply(Event::SourceDeleted { id: "src4".into() });
        assert!(!s.sources.contains_key("src4"));
        // Unknown id: ignored.
        s.apply(Event::SourceDeleted {
            id: "missing".into(),
        });
        assert_eq!(s.sources.len(), 3);
    }

    #[test]
    fn sources_in_topic_put_loose_files_before_folders() {
        let mut s = sample();
        for src in [
            source("a", Some("f"), "b/zeta.md", Some("notes")),
            source("b", Some("f"), "Alpha.txt", None),
            source("c", Some("f"), "a.md", Some("notes")),
        ] {
            s.apply(Event::SourceChanged { source: src });
        }
        let names: Vec<_> = s
            .sources_in_topic("f")
            .iter()
            .map(|src| src.name.clone())
            .collect();
        assert_eq!(names, vec!["Alpha.txt", "paper.pdf", "a.md", "b/zeta.md"]);
    }

    #[test]
    fn embedding_model_changed_is_stored() {
        let mut s = sample();
        assert_eq!(s.embedding, None);
        let model = EmbeddingModel {
            repo: "FluidInference/embeddinggemma-2-coreml".into(),
            state: EmbeddingModelState::Downloading {
                received: 10,
                total: 100,
            },
            speech: None,
            reindex: None,
            reranker: None,
        };
        s.apply(Event::EmbeddingModelChanged {
            model: model.clone(),
        });
        assert_eq!(s.embedding, Some(model));
        // A library reload keeps it.
        s.replace_library(Library {
            topics: vec![],
            lessons: vec![],
            notes: vec![],
            videos: vec![],
            sources: vec![],
        });
        assert!(s.embedding.is_some());
        assert!(s.sources.is_empty());
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
    fn what_wrote_a_lesson_arrives_with_it_and_a_regenerate_replaces_it() {
        let mut s = sample();
        let json = r#"{"type":"lesson_changed","lesson":{"id":"l1","topic_id":"f","title":"G",
            "summary":"","position":1,"level":"intro","minutes":5,"body_md":"Done.",
            "status":{"state":"ready"},"created_at":"2025-10-09T08:53:20Z",
            "updated_at":"2025-10-09T08:53:20Z","written_with":{"prompt_version":"lesson-v2",
            "pipeline_hash":"c39a244aa537f0c2","provider":"claude","model":"claude-sonnet-5-5",
            "at":"2025-10-09T08:53:20Z"}}}"#;
        s.apply(serde_json::from_str(json).unwrap());
        let w = s.lessons["l1"].written_with.clone().unwrap();
        assert_eq!(
            (w.prompt_version.as_str(), w.model.as_str()),
            ("lesson-v2", "claude-sonnet-5-5")
        );
        // The streamed body of a regenerate keeps it until the finished lesson replaces it.
        s.apply(Event::LessonDelta {
            lesson_id: "l1".into(),
            text: " More.".into(),
        });
        assert_eq!(s.lessons["l1"].written_with, Some(w.clone()));
        let mut again = s.lessons["l1"].clone();
        again.written_with = Some(margin_shared::WrittenWith {
            model: "gpt-6-astra".into(),
            provider: margin_shared::Provider::OpenAi,
            ..w
        });
        s.apply(Event::LessonChanged {
            lesson: again.clone(),
        });
        assert_eq!(s.lessons["l1"].written_with, again.written_with);
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
    fn citations_arrive_with_the_finished_lesson() {
        let mut s = sample();
        s.apply(Event::LessonChanged {
            lesson: lesson("g", "f", 3, LessonStatus::Generating),
        });
        s.apply(Event::LessonDelta {
            lesson_id: "g".into(),
            text: "Sines[^1] add up.".into(),
        });
        assert!(s.lessons["g"].citations.is_empty());
        // The final LessonChanged, as the socket sends it, carries the citations.
        let json = r#"{"type":"lesson_changed","lesson":{"id":"g","topic_id":"f","title":"G",
            "summary":"","position":3,"level":"intro","minutes":5,"body_md":"Sines[^1] add up.",
            "status":{"state":"ready"},"created_at":"2026-10-01T00:00:00Z",
            "updated_at":"2026-10-01T00:00:00Z","citations":[{"n":1,"source_id":"src1",
            "source_name":"paper.pdf","kind":"pdf","page":2,"start_ms":null,"end_ms":null,
            "excerpt":"Sines add up.","image_url":"https://m/media/sources/src1/page-2.png?exp=1&sig=a"}]}}"#;
        s.apply(serde_json::from_str::<Event>(json).unwrap());
        let cites = &s.lessons["g"].citations;
        assert_eq!(cites.len(), 1);
        assert_eq!(cites[0].kind, SourceKind::Pdf);
        assert_eq!(cites[0].page, Some(2));
        assert_eq!(
            cites[0].image_url.as_deref(),
            Some("https://m/media/sources/src1/page-2.png?exp=1&sig=a")
        );
        // Deltas after that (a regeneration) keep the list until the next full lesson.
        s.apply(Event::LessonDelta {
            lesson_id: "g".into(),
            text: " More.".into(),
        });
        assert_eq!(s.lessons["g"].citations.len(), 1);
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
        assert_eq!(s.video_dot("l2"), VideoDot::Failed);
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
            access: Vec::new(),
            deny: Vec::new(),
            owner: None,
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
            chat_id: None,
            written_with: None,
            audience_narrowed: false,
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
    fn announces_finished_work_once() {
        let mut s = sample();
        s.apply(Event::LessonChanged {
            lesson: lesson("l3", "f", 3, LessonStatus::Generating),
        });
        s.apply(Event::VideoChanged {
            video: video("v2", "l2", VideoStatus::Scripting),
        });
        let mut p = Proposal {
            access: Vec::new(),
            deny: Vec::new(),
            owner: None,
            id: "p1".into(),
            request: AskRequest {
                prompt: "Fourier".into(),
                lesson_count: 2,
                level: Level::Intro,
                with_videos: false,
                with_quiz: true,
                parent_topic_id: None,
                source_ids: vec![],
            },
            status: ProposalStatus::Drafting,
            outline: None,
            created_at: ts(0),
            chat_id: None,
            written_with: None,
            audience_narrowed: false,
        };
        s.apply(Event::ProposalChanged {
            proposal: p.clone(),
        });
        let before = s.milestones();
        // Nothing finished yet; streaming text isn't news.
        s.apply(Event::LessonDelta {
            lesson_id: "l3".into(),
            text: "More".into(),
        });
        assert!(s.announcements(&before).is_empty());

        s.apply(Event::LessonChanged {
            lesson: lesson("l3", "f", 3, LessonStatus::Ready),
        });
        s.apply(Event::VideoChanged {
            video: video("v2", "l2", VideoStatus::Failed { error: "x".into() }),
        });
        p.status = ProposalStatus::Ready;
        p.outline = Some(margin_shared::Outline {
            topic_title: "Fourier".into(),
            subject_title: "Signals".into(),
            lessons: vec![
                margin_shared::OutlineLesson {
                    title: "a".into(),
                    summary: String::new(),
                    minutes: 5,
                },
                margin_shared::OutlineLesson {
                    title: "b".into(),
                    summary: String::new(),
                    minutes: 5,
                },
            ],
        });
        s.apply(Event::ProposalChanged { proposal: p });
        assert_eq!(
            s.announcements(&before),
            vec![
                "Lesson written: Lesson l3.".to_string(),
                "Couldn't make the video for Lesson l2.".to_string(),
                "Outline ready: 2 lessons.".to_string(),
            ]
        );
        // Measured from now, it's old news; work that shows up finished isn't announced.
        let after = s.milestones();
        s.apply(Event::LessonChanged {
            lesson: lesson("l9", "f", 9, LessonStatus::Ready),
        });
        assert!(s.announcements(&after).is_empty());
    }

    fn chat_item(id: &str, seq: u64, kind: ChatItemKind) -> ChatItem {
        ChatItem {
            id: id.into(),
            chat_id: "c1".into(),
            turn_id: "t1".into(),
            seq,
            status: ItemStatus::InProgress,
            created_at: ts(seq as i64),
            updated_at: ts(seq as i64),
            kind,
        }
    }

    #[test]
    fn chat_events_update_loaded_transcripts() {
        let mut s = sample();
        let chat = Chat {
            access: Vec::new(),
            deny: Vec::new(),
            owner: None,
            id: "c1".into(),
            title: "Fourier".into(),
            title_by: Default::default(),
            state: ChatState::Idle,
            workspace_ids: vec![],
            source_ids: vec![],
            pending: None,
            usage: Default::default(),
            waiting: None,
            created_at: ts(0),
            updated_at: ts(0),
            provider: Default::default(),
        };
        s.apply(Event::ChatChanged { chat: chat.clone() });
        assert_eq!(s.chats["c1"], chat);

        // Not loaded: item events are ignored (the transcript is fetched on open).
        let text = chat_item(
            "i2",
            2,
            ChatItemKind::AssistantText {
                text: "Hi".into(),
                citations: vec![],
            },
        );
        s.apply(Event::ChatItemChanged { item: text.clone() });
        assert!(!s.chat_items.contains_key("c1"));

        s.chat_items.insert("c1".into(), vec![]);
        s.apply(Event::ChatItemChanged { item: text.clone() });
        let user = chat_item(
            "i1",
            1,
            ChatItemKind::UserMessage {
                text: "Teach me".into(),
                source_ids: vec![],
                device_id: "d1".into(),
                device_name: "Mac".into(),
            },
        );
        s.apply(Event::ChatItemChanged { item: user });
        s.apply(Event::ChatItemDelta {
            chat_id: "c1".into(),
            item_id: "i2".into(),
            offset: 2,
            text: " there".into(),
        });
        // A repeat of a delta the transcript already has changes nothing.
        s.apply(Event::ChatItemDelta {
            chat_id: "c1".into(),
            item_id: "i2".into(),
            offset: 2,
            text: " there".into(),
        });
        let items = &s.chat_items["c1"];
        assert_eq!(
            items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            ["i1", "i2"]
        );
        assert_eq!(
            items[1].kind,
            ChatItemKind::AssistantText {
                text: "Hi there".into(),
                citations: vec![],
            }
        );
        let mut done = text;
        done.status = ItemStatus::Done;
        done.kind = ChatItemKind::AssistantText {
            text: "Hi there!".into(),
            citations: vec![],
        };
        s.apply(Event::ChatItemChanged { item: done.clone() });
        assert_eq!(s.chat_items["c1"][1], done);

        s.apply(Event::ChatDeleted { id: "c1".into() });
        assert!(s.chats.is_empty() && s.chat_items.is_empty());
    }

    fn chat_at(id: &str, updated: i64) -> Chat {
        Chat {
            access: Vec::new(),
            deny: Vec::new(),
            owner: None,
            id: id.into(),
            title: id.into(),
            title_by: Default::default(),
            state: ChatState::Idle,
            workspace_ids: vec![],
            source_ids: vec![],
            pending: None,
            usage: Default::default(),
            waiting: None,
            created_at: ts(0),
            updated_at: ts(updated),
            provider: Default::default(),
        }
    }

    fn text(id: &str, seq: u64, status: ItemStatus, text: &str, updated: i64) -> ChatItem {
        let mut item = chat_item(
            id,
            seq,
            ChatItemKind::AssistantText {
                text: text.into(),
                citations: vec![],
            },
        );
        item.status = status;
        item.updated_at = ts(updated);
        item
    }

    #[test]
    fn chat_list_replaces_and_orders_by_recency() {
        let mut s = sample();
        assert!(!s.chats_loaded);
        s.chat_items.insert("gone".into(), vec![]);
        s.chat_items.insert("b".into(), vec![]);
        s.replace_chats(vec![chat_at("a", 5), chat_at("b", 9), chat_at("c", 1)]);
        assert!(s.chats_loaded);
        let order: Vec<&str> = s.chats_by_recency().iter().map(|c| c.id.as_str()).collect();
        assert_eq!(order, ["b", "a", "c"]);
        // A transcript whose chat is gone goes too; the others stay loaded.
        assert!(!s.chat_items.contains_key("gone"));
        assert!(s.chat_items.contains_key("b"));
        // An older copy from a slow request doesn't undo a newer event.
        s.upsert_chat(chat_at("b", 3));
        assert_eq!(s.chats["b"].updated_at, ts(9));
        s.upsert_chat(chat_at("b", 12));
        assert_eq!(s.chats["b"].updated_at, ts(12));
    }

    #[test]
    fn a_fetched_transcript_merges_with_what_events_brought() {
        let mut s = sample();
        // Opened: events apply while the request is out.
        s.chat_items.insert("c1".into(), vec![]);
        s.stale_chats.insert("c1".into());
        s.apply(Event::ChatItemChanged {
            item: text("i2", 2, ItemStatus::InProgress, "Hel", 5),
        });
        s.apply(Event::ChatItemDelta {
            chat_id: "c1".into(),
            item_id: "i2".into(),
            offset: 3,
            text: "lo th".into(),
        });
        s.apply(Event::ChatItemChanged {
            item: text("i3", 3, ItemStatus::Done, "later", 9),
        });
        let user = chat_item(
            "i1",
            1,
            ChatItemKind::UserMessage {
                text: "hi".into(),
                source_ids: vec![],
                device_id: "d1".into(),
                device_name: "Mac".into(),
            },
        );
        // The service's copy was taken before the last delta and before i3 existed.
        s.load_chat(ChatDetail {
            chat: chat_at("c1", 4),
            items: vec![user, text("i2", 2, ItemStatus::InProgress, "Hello", 5)],
        });
        let items = &s.chat_items["c1"];
        assert_eq!(
            items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            ["i1", "i2", "i3"]
        );
        assert_eq!(items[1].kind.streaming_text(), Some("Hello th"));
        assert!(s.stale_chats.is_empty());
        // A fetched copy that finished replaces the streaming one.
        s.load_chat(ChatDetail {
            chat: chat_at("c1", 10),
            items: vec![text("i2", 2, ItemStatus::Done, "Hello there.", 8)],
        });
        assert_eq!(
            s.chat_items["c1"][1].kind.streaming_text(),
            Some("Hello there.")
        );
        assert_eq!(s.chat_items["c1"][1].status, ItemStatus::Done);
        // The message just sent joins the transcript in place.
        let mut sent = chat_item(
            "i4",
            4,
            ChatItemKind::UserMessage {
                text: "more".into(),
                source_ids: vec![],
                device_id: "d1".into(),
                device_name: "Mac".into(),
            },
        );
        sent.status = ItemStatus::Done;
        s.upsert_chat_item(sent);
        assert_eq!(s.chat_items["c1"].len(), 4);
    }

    #[test]
    fn missed_deltas_mark_the_transcript_for_a_fetch() {
        let mut s = sample();
        let delta = |item: &str, offset| Event::ChatItemDelta {
            chat_id: "c1".into(),
            item_id: item.into(),
            offset,
            text: "x".into(),
        };
        // Not open: nothing to fetch.
        s.apply(delta("i1", 0));
        assert!(s.stale_chats.is_empty());
        s.chat_items.insert(
            "c1".into(),
            vec![text("i1", 1, ItemStatus::InProgress, "ab", 1)],
        );
        s.apply(delta("i1", 2));
        assert!(s.stale_chats.is_empty());
        // Past the text it has.
        s.apply(delta("i1", 9));
        assert!(s.stale_chats.contains("c1"));
        s.stale_chats.clear();
        // For an item it never heard of.
        s.apply(delta("i7", 0));
        assert!(s.stale_chats.contains("c1"));
        s.apply(Event::ChatDeleted { id: "c1".into() });
        assert!(s.stale_chats.is_empty());
    }

    #[test]
    fn device_presence_keeps_current() {
        let mut s = sample();
        let me = Device {
            person_id: None,
            id: "d1".into(),
            name: "Mac".into(),
            created_at: ts(0),
            last_seen_at: None,
            current: true,
            online: true,
        };
        s.devices.insert("d1".into(), me.clone());
        s.apply(Event::DeviceChanged {
            device: Device {
                current: false,
                online: false,
                ..me
            },
        });
        assert!(s.devices["d1"].current);
        assert!(!s.devices["d1"].online);
        s.workspaces.insert(
            "w1".into(),
            Workspace {
                access: Vec::new(),
                deny: Vec::new(),
                owner: None,
                id: "w1".into(),
                name: "margin".into(),
                device_id: "d1".into(),
                device_name: "Mac".into(),
                platform: margin_shared::Platform::Macos,
                location: "~/margin".into(),
                created_at: ts(0),
                sync: Default::default(),
                unreadable: Vec::new(),
            },
        );
        s.apply(Event::DeviceDeleted { id: "d1".into() });
        assert!(s.devices.is_empty() && s.workspaces.is_empty());

        // Folder files live with their folder, never with the sources.
        let ws = |id: &str, device: &str, name: &str| Workspace {
            access: Vec::new(),
            deny: Vec::new(),
            owner: None,
            id: id.into(),
            name: name.into(),
            device_id: device.into(),
            device_name: if device == "d1" {
                "Mac".into()
            } else {
                "iPhone".into()
            },
            platform: margin_shared::Platform::Macos,
            location: format!("~/{name}"),
            created_at: ts(0),
            sync: Default::default(),
            unreadable: Vec::new(),
        };
        s.replace_workspaces(vec![
            ws("w2", "d2", "Photos"),
            ws("w1", "d1", "notes"),
            ws("w3", "d1", "Papers"),
        ]);
        let order: Vec<&str> = s
            .workspaces_in_order(Some("d1"))
            .iter()
            .map(|w| w.id.as_str())
            .collect();
        assert_eq!(order, vec!["w1", "w3", "w2"]);
        let file = |id: &str, name: &str| Source {
            workspace_id: Some("w1".into()),
            ..source(id, None, name, Some("notes"))
        };
        // Not loaded: events about its files are ignored.
        s.apply(Event::SourceChanged {
            source: file("f1", "a.md"),
        });
        assert!(s.folder_files_in("w1").is_none() && !s.sources.contains_key("f1"));
        s.replace_folder_files("w1", vec![file("f2", "b.pdf")]);
        s.apply(Event::SourceChanged {
            source: file("f1", "a.md"),
        });
        let names: Vec<&str> = s
            .folder_files_in("w1")
            .unwrap()
            .iter()
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(names, vec!["a.md", "b.pdf"]);
        assert!(!s.sources.contains_key("f1"));
        s.apply(Event::SourceDeleted { id: "f2".into() });
        assert_eq!(s.folder_files_in("w1").unwrap().len(), 1);
        s.apply(Event::WorkspaceDeleted { id: "w1".into() });
        assert!(s.folder_files_in("w1").is_none());
        s.replace_workspaces(vec![]);
        assert!(s.workspaces.is_empty());

        // The service's list replaces what was known: a device revoked meanwhile goes.
        let phone = Device {
            person_id: None,
            id: "d2".into(),
            name: "iPhone".into(),
            created_at: ts(0),
            last_seen_at: None,
            current: false,
            online: true,
        };
        s.merge_devices(vec![Device {
            id: "d3".into(),
            ..phone.clone()
        }]);
        s.replace_devices(vec![phone.clone()]);
        assert_eq!(s.devices.len(), 1);
        assert!(s.devices["d2"].online);
        // It comes online and goes again as its socket opens and closes.
        s.apply(Event::DeviceChanged {
            device: Device {
                online: false,
                ..phone
            },
        });
        assert!(!s.devices["d2"].online);
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
            reasoning_effort: ReasoningEffort::Medium,
            agent_model: None,
            pipeline: Default::default(),
            trace_retention_days: margin_shared::TRACE_RETENTION_DAYS,
            default_provider: Default::default(),
            openai_models: margin_shared::RouteModels::openai_from("gpt-5", None),
            chatgpt_models: margin_shared::RouteModels::chatgpt_default(),
            claude_models: margin_shared::RouteModels::claude_default(),
            has_claude_api_key: false,
            chatgpt: None,
        };
        s.apply(Event::SettingsChanged {
            settings: settings.clone(),
        });
        assert_eq!(s.settings, Some(settings.clone()));

        // The ChatGPT sign-in follows the settings' account; a provider that loses its
        // credential forgets its models; unknown settings refuse no provider.
        assert!(LibraryState::default().provider_ready(Provider::Claude));
        assert!(s.provider_ready(Provider::OpenAi));
        assert!(!s.provider_ready(Provider::Claude));
        assert_eq!(s.chatgpt_status(), ChatGptStatus::SignedOut);
        s.chatgpt = Some(ChatGptStatus::Waiting {
            authorize_url: "https://auth.example/a".into(),
        });
        s.provider_models
            .insert(Provider::OpenAi, crate::fake::fake_models(Provider::OpenAi));
        s.apply(Event::SettingsChanged {
            settings: settings.clone(),
        });
        // Still waiting: settings without an account don't end a sign-in.
        assert!(matches!(s.chatgpt_status(), ChatGptStatus::Waiting { .. }));
        assert!(s.provider_models.contains_key(&Provider::OpenAi));
        let account = crate::fake::fake_account("owner@example.com");
        let signed_in = Settings {
            chatgpt: Some(account.clone()),
            has_api_key: false,
            ..settings.clone()
        };
        s.apply(Event::SettingsChanged {
            settings: signed_in,
        });
        assert_eq!(
            s.chatgpt_status(),
            ChatGptStatus::SignedIn {
                account: account.clone(),
                last_error: None,
            }
        );
        assert!(s.provider_ready(Provider::ChatGpt));
        assert!(!s.provider_models.contains_key(&Provider::OpenAi));
        s.apply(Event::SettingsChanged { settings });
        assert_eq!(s.chatgpt_status(), ChatGptStatus::SignedOut);
    }

    #[test]
    fn a_new_key_base_url_or_account_makes_the_model_list_stale() {
        let mut s = sample();
        let settings = Settings {
            has_api_key: true,
            has_claude_api_key: true,
            chatgpt: Some(crate::fake::fake_account("owner@example.com")),
            ..crate::fake::FakeState::new().settings
        };
        s.apply(Event::SettingsChanged {
            settings: settings.clone(),
        });
        for p in [Provider::OpenAi, Provider::Claude, Provider::ChatGpt] {
            s.provider_models.insert(p, crate::fake::fake_models(p));
        }
        let key = s.slots_key(&settings, Provider::OpenAi);

        // A key replaced (the settings don't change): the UI calls `forget_models`.
        s.forget_models(Provider::Claude);
        assert!(!s.provider_models.contains_key(&Provider::Claude));
        assert!(s.provider_models.contains_key(&Provider::OpenAi));

        // Another base URL: the OpenAI list is dropped and its slots mount again.
        let moved = Settings {
            openai_base_url: "https://proxy.example/v1".into(),
            ..settings.clone()
        };
        s.apply(Event::SettingsChanged {
            settings: moved.clone(),
        });
        assert!(!s.provider_models.contains_key(&Provider::OpenAi));
        assert_ne!(s.slots_key(&moved, Provider::OpenAi), key);
        assert!(s.provider_models.contains_key(&Provider::ChatGpt));

        // Another ChatGPT account: its list is dropped.
        let other = Settings {
            chatgpt: Some(crate::fake::fake_account("other@example.com")),
            ..moved
        };
        s.apply(Event::SettingsChanged { settings: other });
        assert!(!s.provider_models.contains_key(&Provider::ChatGpt));
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

    #[test]
    fn people_groups_agents_and_invitations_follow_events() {
        use crate::fake::{
            ALICE, BOB, sample_agents, sample_groups, sample_invitations, sample_people,
        };
        let mut s = LibraryState::default();
        s.replace_people(sample_people());
        s.replace_groups(sample_groups());
        s.replace_agents(sample_agents());
        s.replace_invitations(sample_invitations(chrono::Utc::now()));
        assert_eq!(s.groups["company/engineering"].members, 2);
        let order: Vec<&str> = s.people_in_order().iter().map(|p| p.id.as_str()).collect();
        assert_eq!(order, vec![ALICE, BOB]);

        // Someone's own settings stay when an event (which never carries them) comes.
        s.people.get_mut(ALICE).unwrap().settings.reasoning_effort =
            Some(margin_shared::ReasoningEffort::High);
        let mut renamed = s.people[ALICE].clone();
        renamed.name = "Alice M.".into();
        renamed.settings = Default::default();
        s.apply(Event::PersonChanged { person: renamed });
        assert_eq!(s.people[ALICE].name, "Alice M.");
        assert!(s.people[ALICE].settings.reasoning_effort.is_some());

        // Memberships move the counts.
        s.apply(Event::MembershipChanged {
            person_id: BOB.into(),
            groups: vec![],
        });
        assert!(s.people[BOB].groups.is_empty());
        assert_eq!(s.groups["company/engineering"].members, 1);
        let mut design = s.groups["company/engineering"].clone();
        design.path = "company/design".into();
        design.name = "Design".into();
        s.apply(Event::GroupChanged { group: design });
        assert_eq!(s.groups.len(), 3);
        s.apply(Event::GroupDeleted {
            path: "company/design".into(),
        });
        assert_eq!(s.groups.len(), 2);

        let mut agent = s.agents["a-triage"].clone();
        agent.disabled = true;
        s.apply(Event::AgentChanged { agent });
        assert!(s.agents["a-triage"].disabled);
        assert_eq!(s.agents_of(BOB).len(), 1);
        s.apply(Event::AgentDeleted {
            id: "a-triage".into(),
        });
        assert!(s.agents_of(BOB).is_empty());

        let first = s.invitations_in_order()[0].id.clone();
        assert_eq!(first, "inv-carol");
        s.apply(Event::InvitationDeleted { id: first });
        assert_eq!(s.invitations.len(), 1);
        s.apply(Event::AccessChanged {
            person_id: BOB.into(),
        });
    }
}
