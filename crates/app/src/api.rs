//! HTTP client for the Margin service, coded against `margin_shared::routes`.
//! Every failure comes back as an [`ApiFailure`] whose `message` is fit to show in the UI.

use std::fmt;
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};

use margin_shared::{
    AcceptProposal, ApiError, Chat, ChatDetail, ChatPatch, ConfirmRequest, Device, EmbeddingModel,
    Id, Lesson, LessonPatch, Library, LoginState, Me, MessageSent, NewChat, NewMessage, NewNote,
    NewTopic, NewWorkspace, Note, NotePatch, PollLoginRequest, Proposal, SearchHit, Settings,
    SettingsPatch, Source, SourceKind, SourceUpload, StartLogin, StartLoginRequest, SyncManifest,
    SyncPlan, ToolOutputText, ToolStatus, Topic, TopicPatch, TranscriptSegment, Video, Workspace,
    routes,
};
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;

#[derive(Debug, Clone, PartialEq)]
pub struct ApiFailure {
    /// HTTP status, `None` for network or decoding failures.
    pub status: Option<u16>,
    /// `ApiError.code` from the service, or "network" / "decode" / "http".
    pub code: String,
    pub message: String,
}

impl ApiFailure {
    pub fn is_unauthorized(&self) -> bool {
        self.status == Some(401) || self.code == "unauthorized"
    }

    pub fn is_no_api_key(&self) -> bool {
        self.code == "no_api_key"
    }

    /// The Settings pane that fixes this error, if one does.
    pub fn settings_pane(&self) -> Option<crate::ui::settings::SettingsPane> {
        use crate::ui::settings::SettingsPane;
        match self.code.as_str() {
            "no_api_key" | "no_model" => Some(SettingsPane::Ai),
            "no_embedding_model" => Some(SettingsPane::Sources),
            c if c.contains("tool") => Some(SettingsPane::Tools),
            _ => None,
        }
    }

    pub fn local(message: impl Into<String>) -> Self {
        Self {
            status: None,
            code: "local".into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for ApiFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

pub type ApiResult<T> = Result<T, ApiFailure>;

#[derive(Clone)]
pub struct Api {
    base: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl PartialEq for Api {
    fn eq(&self, other: &Self) -> bool {
        self.base == other.base && self.token == other.token
    }
}

impl fmt::Debug for Api {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Api")
            .field("base", &self.base)
            .field("signed_in", &self.token.is_some())
            .finish()
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap_or_default()
}

impl Api {
    /// A client without a token, for `/health` and `/auth/*`.
    pub fn anonymous(base: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            token: None,
            http: client(),
        }
    }

    pub fn with_token(base: &str, token: &str) -> Self {
        Self {
            token: Some(token.to_string()),
            ..Self::anonymous(base)
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        let builder = self.http.request(method, self.url(path));
        match &self.token {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        }
    }

    async fn send(&self, builder: RequestBuilder) -> ApiResult<Response> {
        let response = builder.send().await.map_err(|e| self.network_failure(e))?;
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(failure_from(response).await)
        }
    }

    fn network_failure(&self, err: reqwest::Error) -> ApiFailure {
        let what = if err.is_timeout() {
            "timed out".to_string()
        } else if err.is_connect() {
            "couldn't connect".to_string()
        } else {
            root_cause(&err)
        };
        ApiFailure {
            status: None,
            code: "network".into(),
            message: format!("Couldn't reach the service at {} ({what}).", self.base),
        }
    }

    async fn json<T: DeserializeOwned>(&self, builder: RequestBuilder) -> ApiResult<T> {
        let response = self.send(builder).await?;
        response.json::<T>().await.map_err(|e| ApiFailure {
            status: None,
            code: "decode".into(),
            message: format!(
                "The service sent a response this app doesn't understand ({}).",
                root_cause(&e)
            ),
        })
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> ApiResult<T> {
        self.json(self.request(Method::GET, path)).await
    }

    async fn post<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> ApiResult<T> {
        self.json(self.request(Method::POST, path).json(body)).await
    }

    async fn post_empty<T: DeserializeOwned>(&self, path: &str) -> ApiResult<T> {
        self.json(self.request(Method::POST, path)).await
    }

    async fn patch<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> ApiResult<T> {
        self.json(self.request(Method::PATCH, path).json(body))
            .await
    }

    async fn delete(&self, path: &str) -> ApiResult<()> {
        self.send(self.request(Method::DELETE, path))
            .await
            .map(|_| ())
    }

    // ------------------------------------------------------------------
    // Routes
    // ------------------------------------------------------------------

    pub async fn health(&self) -> ApiResult<()> {
        self.send(self.request(Method::GET, routes::HEALTH))
            .await
            .map(|_| ())
    }

    pub async fn start_login(&self, device_name: &str) -> ApiResult<StartLogin> {
        let body = StartLoginRequest {
            device_name: device_name.to_string(),
        };
        self.post(routes::AUTH_START, &body).await
    }

    pub async fn poll_login(&self, login: &StartLogin) -> ApiResult<LoginState> {
        let body = PollLoginRequest {
            login_id: login.login_id.clone(),
            poll_secret: login.poll_secret.clone(),
        };
        self.post(routes::AUTH_POLL, &body).await
    }

    pub async fn me(&self) -> ApiResult<Me> {
        self.get(routes::ME).await
    }

    pub async fn library(&self) -> ApiResult<Library> {
        self.get(routes::LIBRARY).await
    }

    pub async fn create_topic(&self, body: &NewTopic) -> ApiResult<Topic> {
        self.post(routes::TOPICS, body).await
    }

    pub async fn patch_topic(&self, id: &str, body: &TopicPatch) -> ApiResult<Topic> {
        self.patch(&routes::topic(id), body).await
    }

    pub async fn delete_topic(&self, id: &str) -> ApiResult<()> {
        self.delete(&routes::topic(id)).await
    }

    pub async fn patch_lesson(&self, id: &str, body: &LessonPatch) -> ApiResult<Lesson> {
        self.patch(&routes::lesson(id), body).await
    }

    pub async fn delete_lesson(&self, id: &str) -> ApiResult<()> {
        self.delete(&routes::lesson(id)).await
    }

    pub async fn regenerate_lesson(&self, id: &str) -> ApiResult<Lesson> {
        self.post_empty(&routes::lesson_regenerate(id)).await
    }

    pub async fn create_note(&self, body: &NewNote) -> ApiResult<Note> {
        self.post(routes::NOTES, body).await
    }

    pub async fn patch_note(&self, id: &str, body: &NotePatch) -> ApiResult<Note> {
        self.patch(&routes::note(id), body).await
    }

    pub async fn delete_note(&self, id: &str) -> ApiResult<()> {
        self.delete(&routes::note(id)).await
    }

    pub async fn proposal(&self, id: &str) -> ApiResult<Proposal> {
        self.get(&routes::proposal(id)).await
    }

    pub async fn accept_proposal(&self, id: &str, body: &AcceptProposal) -> ApiResult<Topic> {
        self.post(&routes::proposal_accept(id), body).await
    }

    pub async fn make_video(&self, lesson_id: &str) -> ApiResult<Video> {
        self.post_empty(&routes::lesson_video(lesson_id)).await
    }

    pub async fn video(&self, id: &str) -> ApiResult<Video> {
        self.get(&routes::video(id)).await
    }

    pub async fn search(&self, query: &str) -> ApiResult<Vec<SearchHit>> {
        self.json(
            self.request(Method::GET, routes::SEARCH)
                .query(&[("q", query)]),
        )
        .await
    }

    pub async fn settings(&self) -> ApiResult<Settings> {
        self.get(routes::SETTINGS).await
    }

    pub async fn patch_settings(&self, body: &SettingsPatch) -> ApiResult<Settings> {
        self.patch(routes::SETTINGS, body).await
    }

    pub async fn tools(&self) -> ApiResult<Vec<ToolStatus>> {
        self.get(routes::TOOLS).await
    }

    pub async fn devices(&self) -> ApiResult<Vec<Device>> {
        self.get(routes::DEVICES).await
    }

    pub async fn revoke_device(&self, id: &Id) -> ApiResult<()> {
        self.delete(&routes::device(id)).await
    }

    /// Upload the file at `path` as a source, streaming it from disk (never all of it in
    /// memory). `name` is the file name, or its path inside the folder it was added with;
    /// without `topic_id` it waits to be attached (to a chat's message or a topic). A file over its
    /// kind's limit is refused here, before anything is sent; one over
    /// `routes::UPLOAD_PART_BYTES` goes up in parts, `progress` hearing how many bytes
    /// the service has after each.
    pub async fn upload_source(
        &self,
        path: &Path,
        name: &str,
        folder: Option<&str>,
        topic_id: Option<&str>,
        progress: impl Fn(u64),
    ) -> ApiResult<Source> {
        let bytes = tokio::fs::metadata(path)
            .await
            .map_err(|e| ApiFailure::local(format!("Couldn't read {name} ({e}).")))?
            .len();
        if let Some(kind) = SourceKind::for_name(name)
            && bytes > routes::max_source_bytes(kind)
        {
            return Err(ApiFailure::local(routes::too_large_text(name, kind)));
        }
        let mut query = vec![("name", name.to_string())];
        if let Some(folder) = folder {
            query.push(("folder", folder.to_string()));
        }
        if let Some(topic_id) = topic_id {
            query.push(("topic_id", topic_id.to_string()));
        }
        if bytes <= routes::UPLOAD_PART_BYTES {
            let builder = self
                .request(Method::POST, routes::SOURCES)
                .query(&query)
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .header(reqwest::header::CONTENT_LENGTH, bytes)
                // Large files over a slow link take longer than the default 60 s.
                .timeout(UPLOAD_TIMEOUT)
                .body(file_body(path, 0, bytes).await?);
            return self.json(builder).await;
        }

        query.push(("bytes", bytes.to_string()));
        let upload: SourceUpload = self
            .json(
                self.request(Method::POST, routes::SOURCE_UPLOADS)
                    .query(&query),
            )
            .await?;
        match self.send_parts(path, &upload, &progress).await {
            Ok(()) => {
                self.post_empty(&routes::source_upload_finish(&upload.id))
                    .await
            }
            Err(e) => {
                // Best effort: the service drops unfinished uploads after a day anyway.
                let _ = self.delete(&routes::source_upload(&upload.id)).await;
                Err(e)
            }
        }
    }

    /// Sends `path` from where the service's copy ends, part by part. A part that fails
    /// on the way is retried (from wherever the service says it got to) a few times.
    async fn send_parts(
        &self,
        path: &Path,
        upload: &SourceUpload,
        progress: &impl Fn(u64),
    ) -> ApiResult<()> {
        let mut received = upload.received;
        let mut failures = 0;
        while received < upload.bytes {
            let len = routes::UPLOAD_PART_BYTES.min(upload.bytes - received);
            let builder = self
                .request(Method::PUT, &routes::source_upload(&upload.id))
                .query(&[("offset", received)])
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .header(reqwest::header::CONTENT_LENGTH, len)
                .timeout(UPLOAD_TIMEOUT)
                .body(file_body(path, received, len).await?);
            match self.json::<SourceUpload>(builder).await {
                Ok(state) => {
                    received = state.received;
                    failures = 0;
                    progress(received);
                }
                // A broken connection or a part the service has (some of) already:
                // ask where it got to and go on from there.
                Err(e) if retry_part(&e) && failures < PART_RETRIES => {
                    failures += 1;
                    tokio::time::sleep(Duration::from_secs(1 << failures)).await;
                    if let Ok(state) = self
                        .get::<SourceUpload>(&routes::source_upload(&upload.id))
                        .await
                    {
                        received = state.received;
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    pub async fn delete_source(&self, id: &str) -> ApiResult<()> {
        self.delete(&routes::source(id)).await
    }

    /// "Read again": the service reads the source anew, in place.
    pub async fn reprocess_source(&self, id: &str) -> ApiResult<Source> {
        self.post_empty(&routes::source_reprocess(id)).await
    }

    /// An audio or video source's whole transcript.
    pub async fn source_transcript(&self, id: &str) -> ApiResult<Vec<TranscriptSegment>> {
        self.get(&routes::source_transcript(id)).await
    }

    pub async fn embedding(&self) -> ApiResult<EmbeddingModel> {
        self.get(routes::EMBEDDING).await
    }

    /// Start downloading the embedding model on the service Mac; progress arrives as
    /// `EmbeddingModelChanged` events.
    pub async fn download_embedding(&self) -> ApiResult<EmbeddingModel> {
        self.post_empty(routes::EMBEDDING_DOWNLOAD).await
    }

    /// Delete the downloaded model files.
    pub async fn delete_embedding(&self) -> ApiResult<EmbeddingModel> {
        self.json(self.request(Method::DELETE, routes::EMBEDDING))
            .await
    }

    /// Every chat, newest first.
    pub async fn chats(&self) -> ApiResult<Vec<Chat>> {
        self.get(routes::CHATS).await
    }

    /// Create a chat; with `body.message` its first turn starts (the answer streams in
    /// as events).
    pub async fn create_chat(&self, body: &NewChat) -> ApiResult<ChatDetail> {
        self.post(routes::CHATS, body).await
    }

    /// A chat and its whole transcript.
    pub async fn chat(&self, id: &str) -> ApiResult<ChatDetail> {
        self.get(&routes::chat(id)).await
    }

    pub async fn patch_chat(&self, id: &str, body: &ChatPatch) -> ApiResult<Chat> {
        self.patch(&routes::chat(id), body).await
    }

    /// Delete a chat (stopping its answer, if one is running).
    pub async fn delete_chat(&self, id: &str) -> ApiResult<()> {
        self.delete(&routes::chat(id)).await
    }

    /// Send a message: it starts a turn, or waits as the chat's pending message while
    /// one is running.
    pub async fn send_message(&self, chat_id: &str, body: &NewMessage) -> ApiResult<MessageSent> {
        self.post(&routes::chat_messages(chat_id), body).await
    }

    /// Drop the message waiting for the running answer.
    pub async fn drop_pending(&self, chat_id: &str) -> ApiResult<Chat> {
        self.json(self.request(Method::DELETE, &routes::chat_pending(chat_id)))
            .await
    }

    pub async fn stop_chat(&self, chat_id: &str) -> ApiResult<Chat> {
        self.post_empty(&routes::chat_stop(chat_id)).await
    }

    /// Resume an interrupted or failed answer.
    pub async fn continue_chat(&self, chat_id: &str) -> ApiResult<Chat> {
        self.post_empty(&routes::chat_continue(chat_id)).await
    }

    /// Answer the confirmation card the chat's turn is waiting on.
    pub async fn confirm(&self, chat_id: &str, body: &ConfirmRequest) -> ApiResult<Chat> {
        self.post(&routes::chat_confirm(chat_id), body).await
    }

    /// What a tool returned, in full (loaded when its row is expanded).
    pub async fn chat_item_output(&self, chat_id: &str, item_id: &str) -> ApiResult<String> {
        self.get::<ToolOutputText>(&routes::chat_item_output(chat_id, item_id))
            .await
            .map(|o| o.text)
    }

    /// The folders every device has attached.
    pub async fn workspaces(&self) -> ApiResult<Vec<Workspace>> {
        self.get(routes::WORKSPACES).await
    }

    /// Register a folder this device holds.
    pub async fn create_workspace(&self, body: &NewWorkspace) -> ApiResult<Workspace> {
        self.post(routes::WORKSPACES, body).await
    }

    pub async fn delete_workspace(&self, id: &str) -> ApiResult<()> {
        self.delete(&routes::workspace(id)).await
    }

    /// Send a folder's manifest (this device holds it); the answer says what to upload.
    pub async fn sync_workspace(&self, id: &str, manifest: &SyncManifest) -> ApiResult<SyncPlan> {
        self.post(&routes::workspace_sync(id), manifest).await
    }

    /// A folder's files as the service has them (sources), newest first.
    pub async fn workspace_files(&self, id: &str) -> ApiResult<Vec<Source>> {
        self.get(&routes::workspace_files(id)).await
    }

    /// Upload one file of a folder this device holds, in one request (at most
    /// `routes::UPLOAD_PART_BYTES`). `path` is its path inside the folder; an earlier copy
    /// of the same path is replaced on the service. No size limit applies to folder files
    /// beyond the part size per request.
    pub async fn upload_folder_file(
        &self,
        workspace_id: &str,
        path: &str,
        modified: Option<DateTime<Utc>>,
        bytes: Vec<u8>,
    ) -> ApiResult<Source> {
        let len = bytes.len() as u64;
        let builder = self
            .request(Method::POST, routes::SOURCES)
            .query(&folder_query(workspace_id, path, modified))
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .header(reqwest::header::CONTENT_LENGTH, len)
            .timeout(UPLOAD_TIMEOUT)
            .body(bytes);
        self.json(builder).await
    }

    /// Start uploading a folder file in parts (`routes::SOURCE_UPLOADS`).
    pub async fn start_folder_upload(
        &self,
        workspace_id: &str,
        path: &str,
        modified: Option<DateTime<Utc>>,
        bytes: u64,
    ) -> ApiResult<SourceUpload> {
        let mut query = folder_query(workspace_id, path, modified);
        query.push(("bytes", bytes.to_string()));
        self.json(
            self.request(Method::POST, routes::SOURCE_UPLOADS)
                .query(&query),
        )
        .await
    }

    /// How much of an upload in parts the service has.
    pub async fn source_upload_state(&self, id: &str) -> ApiResult<SourceUpload> {
        self.get(&routes::source_upload(id)).await
    }

    /// One part of an upload in parts, at `offset`.
    pub async fn put_upload_part(
        &self,
        id: &str,
        offset: u64,
        bytes: Vec<u8>,
    ) -> ApiResult<SourceUpload> {
        let len = bytes.len() as u64;
        self.json(
            self.request(Method::PUT, &routes::source_upload(id))
                .query(&[("offset", offset)])
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .header(reqwest::header::CONTENT_LENGTH, len)
                .timeout(UPLOAD_TIMEOUT)
                .body(bytes),
        )
        .await
    }

    /// Every part arrived: the source is made.
    pub async fn finish_source_upload(&self, id: &str) -> ApiResult<Source> {
        self.post_empty(&routes::source_upload_finish(id)).await
    }

    /// Give up on an upload in parts.
    pub async fn cancel_source_upload(&self, id: &str) -> ApiResult<()> {
        self.delete(&routes::source_upload(id)).await
    }

    /// Fetch a text file from an absolute (signed media) URL, without the bearer token.
    pub async fn fetch_text(&self, url: &str) -> ApiResult<String> {
        let response = self.send(self.http.get(url)).await?;
        response.text().await.map_err(|e| ApiFailure {
            status: None,
            code: "decode".into(),
            message: format!("Couldn't read {url} ({}).", root_cause(&e)),
        })
    }

    /// `ws(s)://…/api/events` for this service.
    pub fn events_url(&self) -> String {
        let url = self.url(routes::EVENTS);
        if let Some(rest) = url.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = url.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            url
        }
    }
}

/// How long one upload request (a whole small file, or one part) may take.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(600);
/// Tries per part after a failure on the way.
pub const PART_RETRIES: u32 = 3;

/// The query of a folder file's upload: its path, the folder and its modification time.
fn folder_query(
    workspace_id: &str,
    path: &str,
    modified: Option<DateTime<Utc>>,
) -> Vec<(&'static str, String)> {
    let mut query = vec![
        ("name", path.to_string()),
        ("workspace_id", workspace_id.to_string()),
    ];
    if let Some(modified) = modified {
        query.push(("modified", modified.to_rfc3339()));
    }
    query
}

/// Whether a failed part is worth sending again: the connection broke or timed out, the
/// service had a hiccup, or it has a different amount of the file than we thought.
pub fn retry_part(e: &ApiFailure) -> bool {
    e.status.is_none() && e.code == "network"
        || e.status.is_some_and(|s| s >= 500)
        || e.code == "wrong_offset"
        || e.code == "upload_busy"
}

/// `len` bytes of the file at `path` from `offset`, as a streamed request body read in
/// 256 KB pieces.
async fn file_body(path: &Path, offset: u64, len: u64) -> ApiResult<reqwest::Body> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let fail = |e: std::io::Error| ApiFailure::local(format!("Couldn't read {name} ({e})."));
    let mut file = tokio::fs::File::open(path).await.map_err(fail)?;
    if offset > 0 {
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(fail)?;
    }
    let stream = futures_util::stream::try_unfold((file, len), |(mut file, left)| async move {
        if left == 0 {
            return Ok(None);
        }
        let mut buf = vec![0u8; left.min(256 * 1024) as usize];
        let n = file.read(&mut buf).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the file got shorter while it was being uploaded",
            ));
        }
        buf.truncate(n);
        Ok::<_, std::io::Error>(Some((buf, (file, left - n as u64))))
    });
    Ok(reqwest::Body::wrap_stream(stream))
}

async fn failure_from(response: Response) -> ApiFailure {
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    match serde_json::from_str::<ApiError>(&text) {
        Ok(err) => ApiFailure {
            status: Some(status.as_u16()),
            code: err.code,
            message: err.message,
        },
        Err(_) => ApiFailure {
            status: Some(status.as_u16()),
            code: if status == StatusCode::UNAUTHORIZED {
                "unauthorized".into()
            } else {
                "http".into()
            },
            message: match status {
                StatusCode::UNAUTHORIZED => "This device is no longer signed in.".into(),
                StatusCode::NOT_FOUND => "The service doesn't know that item any more.".into(),
                s if s.is_server_error() => {
                    format!("The service had a problem ({}).", s.as_u16())
                }
                s => format!("The service refused the request ({}).", s.as_u16()),
            },
        },
    }
}

fn root_cause(err: &(dyn std::error::Error + 'static)) -> String {
    let mut current = err;
    while let Some(next) = current.source() {
        current = next;
    }
    current.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeService, sample_library};
    use margin_shared::{ProposalStatus, SearchKind};

    #[tokio::test]
    async fn sign_in_flow_pending_then_approved() {
        let fake = FakeService::start().await;
        let api = Api::anonymous(&fake.url);
        api.health().await.unwrap();
        let login = api.start_login("test-mac").await.unwrap();
        assert_eq!(fake.state.lock().unwrap().device_name, "test-mac");
        assert!(login.authorize_url.starts_with("https://github.com/"));
        assert_eq!(api.poll_login(&login).await.unwrap(), LoginState::Pending);
        fake.state.lock().unwrap().approve = true;
        match api.poll_login(&login).await.unwrap() {
            LoginState::Approved {
                token,
                github_login,
            } => {
                assert_eq!(token, crate::fake::TOKEN);
                assert_eq!(github_login, "Nkr1shna");
            }
            other => panic!("expected approval, got {other:?}"),
        }
        // Only once.
        assert_eq!(api.poll_login(&login).await.unwrap(), LoginState::Expired);
    }

    #[tokio::test]
    async fn folder_manifests_files_and_uploads() {
        use margin_shared::{
            FileStamp, FolderFileType, ManifestFile, NewWorkspace, Platform, SyncManifest,
        };
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        let ws = api
            .create_workspace(&NewWorkspace {
                name: "notes".into(),
                platform: Platform::Macos,
                location: "~/notes".into(),
            })
            .await
            .unwrap();
        let modified = chrono::Utc::now();
        let small = b"# Intro\n".to_vec();
        let big = vec![7u8; routes::UPLOAD_PART_BYTES as usize + 10];
        let file = |path: &str, kind, bytes: usize| ManifestFile {
            path: path.into(),
            kind,
            stamp: FileStamp {
                bytes: bytes as u64,
                modified: Some(modified),
                sha256: "ab".repeat(32),
            },
        };
        let plan = api
            .sync_workspace(
                &ws.id,
                &SyncManifest {
                    files: vec![
                        file("intro.md", FolderFileType::Text, small.len()),
                        file("scan.pdf", FolderFileType::Pdf, big.len()),
                    ],
                },
            )
            .await
            .unwrap();
        assert_eq!(plan.upload, vec!["intro.md", "scan.pdf"]);

        // One request for a small file, parts for a large one (no size limit).
        let source = api
            .upload_folder_file(&ws.id, "intro.md", Some(modified), small)
            .await
            .unwrap();
        assert_eq!(source.workspace_id.as_deref(), Some(ws.id.as_str()));
        assert_eq!(source.folder.as_deref(), Some("notes"));
        assert_eq!(source.topic_id, None);
        assert!(source.modified.is_some());
        let upload = api
            .start_folder_upload(&ws.id, "scan.pdf", Some(modified), big.len() as u64)
            .await
            .unwrap();
        let part = routes::UPLOAD_PART_BYTES as usize;
        let state = api
            .put_upload_part(&upload.id, 0, big[..part].to_vec())
            .await
            .unwrap();
        assert_eq!(state.received, part as u64);
        assert_eq!(
            api.source_upload_state(&upload.id).await.unwrap().received,
            part as u64
        );
        api.put_upload_part(&upload.id, part as u64, big[part..].to_vec())
            .await
            .unwrap();
        let pdf = api.finish_source_upload(&upload.id).await.unwrap();
        assert_eq!(pdf.bytes, big.len() as u64);

        let files = api.workspace_files(&ws.id).await.unwrap();
        let mut names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["intro.md", "scan.pdf"]);
        assert!(files.iter().all(|f| f.media_url.is_some()));
        // Folder files never join the library's sources.
        assert!(
            !api.library()
                .await
                .unwrap()
                .sources
                .iter()
                .any(|s| s.workspace_id.is_some())
        );

        // The folder takes only its kinds of files, inside it.
        let err = api
            .upload_folder_file(&ws.id, "main.rs", None, b"fn main(){}".to_vec())
            .await
            .unwrap_err();
        assert_eq!(err.code, "unsupported");
        let err = api
            .upload_folder_file(&ws.id, "../x.md", None, b"x".to_vec())
            .await
            .unwrap_err();
        assert_eq!(err.code, "bad_path");
        let gone = api.sync_workspace("nope", &SyncManifest::default()).await;
        assert_eq!(gone.unwrap_err().status, Some(404));
        // An abandoned upload in parts can be cancelled.
        let upload = api
            .start_folder_upload(&ws.id, "again.pdf", None, 10)
            .await
            .unwrap();
        api.cancel_source_upload(&upload.id).await.unwrap();
        assert_eq!(
            api.source_upload_state(&upload.id)
                .await
                .unwrap_err()
                .status,
            Some(404)
        );
    }

    #[tokio::test]
    async fn library_and_crud_with_bearer() {
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        let lib = api.library().await.unwrap();
        let mut expected = sample_library();
        crate::fake::rebase_media(&mut expected, |path| format!("{}{path}", fake.url));
        assert_eq!(lib, expected);
        let me = api.me().await.unwrap();
        assert_eq!(me.github_login, "Nkr1shna");
        assert!(me.device.current);

        let topic = api
            .create_topic(&NewTopic {
                parent_id: None,
                title: "Linear Algebra".into(),
            })
            .await
            .unwrap();
        assert_eq!(topic.title, "Linear Algebra");
        let renamed = api
            .patch_topic(
                &topic.id,
                &TopicPatch {
                    title: Some("Linear algebra".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(renamed.title, "Linear algebra");
        api.delete_topic(&topic.id).await.unwrap();
        let err = api.delete_topic(&topic.id).await.unwrap_err();
        assert_eq!(err.status, Some(404));
        assert_eq!(err.code, "not_found");

        let note = api
            .create_note(&NewNote {
                topic_id: "fourier".into(),
                lesson_id: Some("l1".into()),
                title: "Gibbs".into(),
                body_md: "9%".into(),
                quote: Some("overshoot".into()),
            })
            .await
            .unwrap();
        let patched = api
            .patch_note(
                &note.id,
                &NotePatch {
                    body_md: Some("about 9%".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(patched.body_md, "about 9%");
        assert_eq!(patched.title, "Gibbs");
        api.delete_note(&note.id).await.unwrap();
    }

    #[tokio::test]
    async fn unauthorized_without_token() {
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, "wrong");
        let err = api.library().await.unwrap_err();
        assert!(err.is_unauthorized());
        assert_eq!(err.message, "Sign in again.");
    }

    #[tokio::test]
    async fn chats_propose_a_course_and_create_it_with_chips() {
        use margin_shared::{Card, ChatItemKind, Level, OutlineChips};
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        let id = api
            .create_chat(&NewChat {
                message: Some(message(
                    "Make me a course on control theory with videos, 3 lessons",
                )),
                workspace_ids: vec![],
            })
            .await
            .unwrap()
            .chat
            .id;
        let detail = api.chat(&id).await.unwrap();
        assert_eq!(
            kinds(&detail),
            ["user", "reasoning", "call", "result", "card", "text"]
        );
        let proposal_id = detail
            .items
            .iter()
            .find_map(|i| match &i.kind {
                ChatItemKind::Card {
                    card: Card::Outline(card),
                    ..
                } => Some(card.proposal_id.clone()),
                _ => None,
            })
            .expect("an outline card");
        // The proposal belongs to the chat, with what was said and what was inferred.
        let proposal = api.proposal(&proposal_id).await.unwrap();
        assert_eq!(proposal.status, ProposalStatus::Ready);
        assert_eq!(proposal.chat_id.as_deref(), Some(id.as_str()));
        assert_eq!(proposal.request.level, Level::Intro);
        assert!(proposal.request.with_videos && proposal.request.with_quiz);
        let mut outline = proposal.outline.unwrap();
        assert_eq!(outline.topic_title, "Control theory");
        assert_eq!(outline.lessons.len(), 3);

        // Without a key the course can't be written.
        fake.state.lock().unwrap().has_api_key = false;
        let body = |outline: &margin_shared::Outline| AcceptProposal {
            outline: outline.clone(),
            chips: OutlineChips {
                level: Some(Level::Advanced),
                with_videos: Some(false),
                with_quiz: None,
            },
        };
        let err = api
            .accept_proposal(&proposal_id, &body(&outline))
            .await
            .unwrap_err();
        assert!(err.is_no_api_key());
        fake.state.lock().unwrap().has_api_key = true;

        // Edited on the card (one lesson removed, one renamed), the chips changed.
        outline.lessons.remove(1);
        outline.lessons[0].title = "Feedback loops".into();
        let topic = api
            .accept_proposal(&proposal_id, &body(&outline))
            .await
            .unwrap();
        assert_eq!(topic.title, "Control theory");
        let lib = api.library().await.unwrap();
        let lessons: Vec<_> = lib
            .lessons
            .iter()
            .filter(|l| l.topic_id == topic.id)
            .collect();
        assert_eq!(lessons.len(), 2);
        assert_eq!(lessons[0].title, "Feedback loops");
        assert!(lessons.iter().all(|l| l.level == Level::Advanced));
        // The chip turned the videos off.
        assert!(
            !lib.videos
                .iter()
                .any(|v| lessons.iter().any(|l| l.id == v.lesson_id))
        );
        assert_eq!(
            api.proposal(&proposal_id).await.unwrap().status,
            ProposalStatus::Accepted {
                topic_id: topic.id.clone()
            }
        );
        // The chat knows, for the agent's next answer.
        let detail = api.chat(&id).await.unwrap();
        assert!(matches!(
            &detail.items.last().unwrap().kind,
            ChatItemKind::Action { text } if text == "Created the course “Control theory” with 2 lessons."
        ));
        // Created once: another device's Create lessons is refused.
        let again = api
            .accept_proposal(&proposal_id, &body(&outline))
            .await
            .unwrap_err();
        assert_eq!(
            (again.status, again.code.as_str()),
            (Some(409), "already_accepted")
        );
    }

    #[tokio::test]
    async fn chats_accept_the_outline_when_told_to_go_ahead() {
        use margin_shared::{Card, ChatItemKind, ResultTarget};
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        let id = api
            .create_chat(&NewChat {
                message: Some(message("Teach me about sampling, advanced")),
                workspace_ids: vec![],
            })
            .await
            .unwrap()
            .chat
            .id;
        let sent = api.send_message(&id, &message("Go ahead")).await.unwrap();
        assert!(sent.item.is_some());
        let detail = api.chat(&id).await.unwrap();
        assert!(
            kinds(&detail).ends_with(&["user", "reasoning", "call", "result", "card", "text"]),
            "{:?}",
            kinds(&detail)
        );
        let topic_id = detail
            .items
            .iter()
            .find_map(|i| match &i.kind {
                ChatItemKind::Card {
                    card: Card::Result(card),
                    ..
                } => match &card.target {
                    ResultTarget::Topic { topic_id } => Some(topic_id.clone()),
                    _ => None,
                },
                _ => None,
            })
            .expect("a result card for the topic");
        let lib = api.library().await.unwrap();
        let topic = lib.topics.iter().find(|t| t.id == topic_id).unwrap();
        assert_eq!(topic.title, "Sampling");
        let lessons = lib.lessons.iter().filter(|l| l.topic_id == topic_id);
        assert!(lessons.clone().count() == 4);
        assert!(
            lessons
                .into_iter()
                .all(|l| l.level == margin_shared::Level::Advanced)
        );
        // The tool's own result tells the agent; no separate note of it.
        assert!(
            !detail
                .items
                .iter()
                .any(|i| matches!(i.kind, ChatItemKind::Action { .. }))
        );
    }

    #[tokio::test]
    async fn search_encodes_query() {
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        let hits = api.search("sines & cosines").await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, SearchKind::Lesson);
        assert_eq!(hits[0].id, "l1");
        assert!(hits[0].snippet.contains("sines & cosines"));
    }

    #[tokio::test]
    async fn settings_tools_devices_and_video() {
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        let settings = api
            .patch_settings(&SettingsPatch {
                openai_api_key: Some("sk-test".into()),
                render_fps: Some(60),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(settings.has_api_key);
        assert_eq!(settings.render_fps, 60);
        assert_eq!(api.settings().await.unwrap(), settings);
        let tools = api.tools().await.unwrap();
        assert!(
            tools
                .iter()
                .any(|t| !t.found && !t.install_commands.is_empty())
        );
        let devices = api.devices().await.unwrap();
        assert_eq!(devices.len(), 2);
        api.revoke_device(&devices[1].id).await.unwrap();
        assert_eq!(api.devices().await.unwrap().len(), 1);
        let video = api.make_video("l1").await.unwrap();
        assert_eq!(video.status, margin_shared::VideoStatus::Queued);
        assert_eq!(api.video(&video.id).await.unwrap().id, video.id);
        let lesson = api.regenerate_lesson("l1").await.unwrap();
        assert_eq!(lesson.status, margin_shared::LessonStatus::Queued);
    }

    /// A scratch file removed when dropped.
    struct TempFile(std::path::PathBuf);

    impl TempFile {
        fn new(tag: &str, contents: &[u8]) -> Self {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "margin-api-{tag}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::write(&path, contents).unwrap();
            Self(path)
        }

        /// A file of `len` bytes that takes no room (sparse), for size limits.
        fn sized(tag: &str, len: u64) -> Self {
            let file = Self::new(tag, b"");
            std::fs::File::options()
                .write(true)
                .open(&file.0)
                .unwrap()
                .set_len(len)
                .unwrap();
            file
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[tokio::test]
    async fn sources_upload_with_query_and_attach_on_accept() {
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        let file = TempFile::new("gibbs", b"# Gibbs & friends\n");
        let source = api
            .upload_source(
                &file.0,
                "notes/ch 1.md",
                Some("Fourier notes"),
                None,
                |_| {},
            )
            .await
            .unwrap();
        assert_eq!(source.name, "notes/ch 1.md");
        assert_eq!(source.folder.as_deref(), Some("Fourier notes"));
        assert_eq!(source.topic_id, None);
        assert_eq!(source.kind, margin_shared::SourceKind::Text);
        assert_eq!(source.bytes, 18);
        assert_eq!(source.status, margin_shared::SourceStatus::Queued);
        // Its original can be viewed at its signed URL.
        let original = source.media_url.clone().expect("a media URL");
        assert_eq!(
            api.fetch_text(&original).await.unwrap(),
            "# Gibbs & friends\n"
        );

        let code = TempFile::new("main", b"fn main() {}");
        let in_topic = api
            .upload_source(&code.0, "main.rs", None, Some("fourier"), |_| {})
            .await
            .unwrap();
        assert_eq!(in_topic.topic_id.as_deref(), Some("fourier"));
        assert!(api.library().await.unwrap().sources.contains(&in_topic));
        let err = api
            .upload_source(&code.0, "archive.zip", None, None, |_| {})
            .await
            .unwrap_err();
        assert_eq!(err.status, Some(400));
        assert!(
            err.message.contains(margin_shared::SOURCE_KINDS),
            "{}",
            err.message
        );

        // A course proposed in a chat the file was sent with draws on it; creating the
        // course attaches the file to the new topic.
        let chat = api
            .create_chat(&NewChat {
                message: Some(NewMessage {
                    text: "A course on Fourier series, 1 lesson".into(),
                    source_ids: vec![source.id.clone()],
                }),
                workspace_ids: vec![],
            })
            .await
            .unwrap();
        let proposal = fake
            .state
            .lock()
            .unwrap()
            .proposals
            .values()
            .find(|p| p.chat_id.as_deref() == Some(chat.chat.id.as_str()))
            .cloned()
            .expect("a proposal from the chat");
        assert_eq!(proposal.request.source_ids, vec![source.id.clone()]);
        let topic = api
            .accept_proposal(
                &proposal.id,
                &AcceptProposal {
                    outline: proposal.outline.unwrap(),
                    chips: Default::default(),
                },
            )
            .await
            .unwrap();
        let lib = api.library().await.unwrap();
        let attached = lib.sources.iter().find(|s| s.id == source.id).unwrap();
        assert_eq!(attached.topic_id.as_deref(), Some(topic.id.as_str()));

        api.delete_source(&source.id).await.unwrap();
        assert_eq!(
            api.delete_source(&source.id).await.unwrap_err().status,
            Some(404)
        );
    }

    #[tokio::test]
    async fn upload_refuses_files_over_their_kinds_limit_locally() {
        let api = Api::with_token("http://127.0.0.1:9", "x");
        let big = TempFile::sized("big", routes::MAX_OTHER_SOURCE_BYTES + 1);
        let err = api
            .upload_source(&big.0, "big.txt", None, None, |_| {})
            .await
            .unwrap_err();
        assert_eq!(err.code, "local");
        assert_eq!(
            err.message,
            "big.txt is too big: text, code, PDF and image sources can be at most 50 MB."
        );
        let long = TempFile::sized("long", routes::MAX_MEDIA_SOURCE_BYTES + 1);
        let err = api
            .upload_source(&long.0, "long.mov", None, None, |_| {})
            .await
            .unwrap_err();
        assert_eq!(err.code, "local");
        assert!(err.message.ends_with("at most 500 MB."), "{}", err.message);
        // Under the limit, it goes to the service (unreachable here).
        let err = api
            .upload_source(&big.0, "big.mov", None, None, |_| {})
            .await
            .unwrap_err();
        assert_eq!(err.code, "network");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn large_files_go_up_in_parts_and_retry_failed_ones() {
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        // Two parts: a whole one and a bit; the ends are marked to check the order.
        let len = routes::UPLOAD_PART_BYTES + 1000;
        let file = TempFile::sized("lecture", len);
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = std::fs::File::options().write(true).open(&file.0).unwrap();
            f.write_all(b"START").unwrap();
            f.seek(SeekFrom::Start(len - 3)).unwrap();
            f.write_all(b"END").unwrap();
        }
        // The first try at a part fails; it's sent again.
        fake.state.lock().unwrap().fail_parts = 1;
        let seen = std::sync::Mutex::new(Vec::new());
        let source = api
            .upload_source(
                &file.0,
                "talks/lecture.mp4",
                Some("talks"),
                Some("fourier"),
                |n| seen.lock().unwrap().push(n),
            )
            .await
            .unwrap();
        assert_eq!(source.bytes, len);
        assert_eq!(source.kind, margin_shared::SourceKind::Video);
        assert_eq!(source.topic_id.as_deref(), Some("fourier"));
        assert_eq!(*seen.lock().unwrap(), vec![routes::UPLOAD_PART_BYTES, len]);
        let s = fake.state.lock().unwrap();
        assert_eq!(s.parts_received, 2);
        assert!(s.part_uploads.is_empty());
        let data = &s.source_files[&source.id];
        assert_eq!(data.len() as u64, len);
        assert!(data.starts_with(b"START") && data.ends_with(b"END"));
    }

    #[tokio::test]
    async fn sources_read_again_and_transcripts() {
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        let lines = api.source_transcript("s5").await.unwrap();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[2].start_ms, 3_725);
        assert!(api.source_transcript("s1").await.unwrap().is_empty());
        assert_eq!(
            api.source_transcript("nope").await.unwrap_err().status,
            Some(404)
        );
        let again = api.reprocess_source("s3").await.unwrap();
        assert_eq!(again.id, "s3");
        assert_eq!(again.status, margin_shared::SourceStatus::Queued);
        let err = api.reprocess_source("s3").await.unwrap_err();
        assert_eq!((err.status, err.code.as_str()), (Some(409), "source_busy"));
    }

    #[tokio::test]
    async fn embedding_model_download_and_delete() {
        use margin_shared::EmbeddingModelState;
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        let model = api.embedding().await.unwrap();
        assert_eq!(model.repo, "FluidInference/embeddinggemma-2-coreml");
        assert!(matches!(
            model.state,
            EmbeddingModelState::NotDownloaded { .. }
        ));
        let downloading = api.download_embedding().await.unwrap();
        assert!(matches!(
            downloading.state,
            EmbeddingModelState::Downloading { received: 0, .. }
        ));
        assert_eq!(api.embedding().await.unwrap(), downloading);
        let removed = api.delete_embedding().await.unwrap();
        assert!(matches!(
            removed.state,
            EmbeddingModelState::NotDownloaded { .. }
        ));
    }

    #[tokio::test]
    async fn lessons_carry_their_citations() {
        use margin_shared::SourceKind;
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        let lib = api.library().await.unwrap();
        let lesson = lib.lessons.iter().find(|l| l.id == "l1").unwrap();
        let kinds: Vec<(u32, SourceKind)> =
            lesson.citations.iter().map(|c| (c.n, c.kind)).collect();
        assert_eq!(
            kinds,
            vec![
                (1, SourceKind::Pdf),
                (2, SourceKind::Code),
                (3, SourceKind::Image),
                (4, SourceKind::Video),
                (5, SourceKind::Text),
                (6, SourceKind::Audio),
                (7, SourceKind::Audio),
            ]
        );
        let page = &lesson.citations[0];
        assert_eq!(page.page, Some(3));
        assert_eq!(
            page.image_url.as_deref(),
            Some(format!("{}/media/sources/s1/page-3.png?exp=1&sig=x", fake.url).as_str())
        );
        let clip = &lesson.citations[5];
        assert_eq!((clip.start_ms, clip.end_ms), (Some(3_725), Some(8_560)));
        let audio = lib.sources.iter().find(|s| s.id == clip.source_id).unwrap();
        assert!(
            audio
                .media_url
                .as_deref()
                .is_some_and(|u| u.starts_with(&fake.url))
        );
        // Every marker in the body names a citation.
        for c in &lesson.citations {
            assert!(
                lesson.body_md.contains(&format!("[^{}]", c.n)),
                "[^{}]",
                c.n
            );
        }
        // Lessons written before citations existed decode with none.
        let mut old = serde_json::to_value(lesson).unwrap();
        old.as_object_mut().unwrap().remove("citations");
        let old: margin_shared::Lesson = serde_json::from_value(old).unwrap();
        assert!(old.citations.is_empty());
    }

    fn message(text: &str) -> NewMessage {
        NewMessage {
            text: text.into(),
            source_ids: vec![],
        }
    }

    fn kinds(detail: &ChatDetail) -> Vec<&'static str> {
        use margin_shared::ChatItemKind::*;
        detail
            .items
            .iter()
            .map(|i| match &i.kind {
                UserMessage { .. } => "user",
                AssistantText { .. } => "text",
                Reasoning { .. } => "reasoning",
                ToolCall { .. } => "call",
                ToolResult { .. } => "result",
                Card { .. } => "card",
                Action { .. } => "action",
                Error { .. } => "error",
            })
            .collect()
    }

    #[tokio::test]
    async fn chats_answer_rename_and_delete() {
        use margin_shared::{ChatItemKind, ChatState, ItemStatus};
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        let before = api.chats().await.unwrap().len();
        let empty = api
            .create_chat(&NewChat {
                message: Some(message("  ")),
                workspace_ids: vec![],
            })
            .await
            .unwrap_err();
        assert_eq!(
            (empty.status, empty.code.as_str()),
            (Some(400), "empty_message")
        );

        let created = api
            .create_chat(&NewChat {
                message: Some(message("Sums of sines\nand more")),
                workspace_ids: vec![],
            })
            .await
            .unwrap();
        let id = created.chat.id.clone();
        // Named by the first line of its first message.
        assert_eq!(created.chat.title, "Sums of sines");
        assert!(matches!(
            created.items[0].kind,
            ChatItemKind::UserMessage { .. }
        ));
        // The answer is in by now (the fake answers before replying).
        let detail = api.chat(&id).await.unwrap();
        assert_eq!(detail.chat.state, ChatState::Idle);
        assert_eq!(
            kinds(&detail),
            ["user", "reasoning", "call", "result", "text"]
        );
        assert!(detail.items.iter().all(|i| i.status == ItemStatus::Done));
        assert!(detail.chat.usage.model_calls > 0);
        // The tool's whole output loads on demand.
        let result = &detail.items[3];
        let output = api.chat_item_output(&id, &result.id).await.unwrap();
        assert!(output.contains("Why sums of sines?"), "{output}");
        let not_a_result = api.chat_item_output(&id, &detail.items[0].id).await;
        assert_eq!(not_a_result.unwrap_err().status, Some(404));
        let chats = api.chats().await.unwrap();
        assert_eq!(chats.len(), before + 1);
        assert_eq!(chats[0].id, id, "newest first");

        let renamed = api
            .patch_chat(
                &id,
                &ChatPatch {
                    title: Some(" Fourier ".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(renamed.title, "Fourier");
        let blank = ChatPatch {
            title: Some(" ".into()),
            ..Default::default()
        };
        assert_eq!(
            api.patch_chat(&id, &blank).await.unwrap_err().status,
            Some(400)
        );

        let sent = api
            .send_message(&id, &message("And sources?"))
            .await
            .unwrap();
        assert!(sent.item.is_some());
        let detail = api.chat(&id).await.unwrap();
        // "source" also searches the sources, with a passages card.
        assert!(kinds(&detail).ends_with(&[
            "user",
            "reasoning",
            "call",
            "result",
            "call",
            "result",
            "card",
            "text"
        ]));

        api.delete_chat(&id).await.unwrap();
        assert_eq!(api.chat(&id).await.unwrap_err().status, Some(404));
        assert_eq!(api.delete_chat(&id).await.unwrap_err().status, Some(404));
    }

    #[tokio::test]
    async fn chats_stop_and_send_the_pending_message() {
        use margin_shared::{ChatItemKind, ChatState, ItemStatus};
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        fake.state.lock().unwrap().chat_hang = true;
        let id = api
            .create_chat(&NewChat {
                message: Some(message("Fourier")),
                workspace_ids: vec![],
            })
            .await
            .unwrap()
            .chat
            .id;
        assert!(api.chat(&id).await.unwrap().chat.state.is_busy());
        assert_eq!(
            api.continue_chat(&id).await.unwrap_err().code,
            "cannot_continue"
        );
        // While it answers, a message waits; a second one is refused.
        let waiting = api.send_message(&id, &message("Then?")).await.unwrap();
        assert_eq!(waiting.item, None);
        assert_eq!(waiting.chat.pending.as_ref().unwrap().text, "Then?");
        let again = api.send_message(&id, &message("And?")).await.unwrap_err();
        assert_eq!(
            (again.status, again.code.as_str()),
            (Some(409), "already_pending")
        );
        assert_eq!(api.drop_pending(&id).await.unwrap().pending, None);
        api.send_message(&id, &message("Then?")).await.unwrap();

        // Stop: the partial answer is kept, cancelled; the pending message starts next.
        let stopped = api.stop_chat(&id).await.unwrap();
        assert_eq!(stopped.pending, None);
        let detail = api.chat(&id).await.unwrap();
        let cut = detail
            .items
            .iter()
            .find(|i| matches!(i.kind, ChatItemKind::AssistantText { .. }))
            .unwrap();
        assert_eq!(cut.status, ItemStatus::Cancelled);
        assert!(detail.items.iter().any(
            |i| matches!(&i.kind, ChatItemKind::Action { text } if text == "Stopped by the owner.")
        ));
        assert!(
            matches!(detail.chat.state, ChatState::Running { .. }),
            "the pending message's turn"
        );
        assert_eq!(kinds(&detail).iter().filter(|k| **k == "user").count(), 2);

        fake.state.lock().unwrap().chat_hang = false;
        api.stop_chat(&id).await.unwrap();
        assert_eq!(api.stop_chat(&id).await.unwrap_err().code, "not_running");
    }

    #[tokio::test]
    async fn chats_fail_and_retry() {
        use margin_shared::{ChatItemKind, ChatState};
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        let id = api
            .create_chat(&NewChat {
                message: Some(message("make it fail")),
                workspace_ids: vec![],
            })
            .await
            .unwrap()
            .chat
            .id;
        let detail = api.chat(&id).await.unwrap();
        assert!(matches!(detail.chat.state, ChatState::Failed { .. }));
        assert!(matches!(
            detail.items.last().unwrap().kind,
            ChatItemKind::Error {
                retryable: true,
                ..
            }
        ));
        let resumed = api.continue_chat(&id).await.unwrap();
        assert!(matches!(resumed.state, ChatState::Running { .. }));
        let detail = api.chat(&id).await.unwrap();
        assert_eq!(detail.chat.state, ChatState::Idle);
        assert_eq!(kinds(&detail).last(), Some(&"text"));

        // Without a key the answer fails the same way, retryable once there is one.
        fake.state.lock().unwrap().has_api_key = false;
        api.send_message(&id, &message("again")).await.unwrap();
        let detail = api.chat(&id).await.unwrap();
        assert!(
            matches!(&detail.items.last().unwrap().kind, ChatItemKind::Error { message, .. } if message.contains("API key"))
        );
    }

    #[tokio::test]
    async fn chats_wait_for_a_confirmation() {
        use margin_shared::{Card, ChatItemKind, ChatState, Decision};
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        let notes = api.library().await.unwrap().notes.len();
        let id = api
            .create_chat(&NewChat {
                message: Some(message("Delete my first note")),
                workspace_ids: vec![],
            })
            .await
            .unwrap()
            .chat
            .id;
        let detail = api.chat(&id).await.unwrap();
        let ChatState::WaitingForUser { item_id, .. } = &detail.chat.state else {
            panic!("{:?}", detail.chat.state);
        };
        let wrong = ConfirmRequest {
            item_id: "nope".into(),
            approve: true,
        };
        assert_eq!(
            api.confirm(&id, &wrong).await.unwrap_err().code,
            "not_waiting"
        );
        let yes = ConfirmRequest {
            item_id: item_id.clone(),
            approve: true,
        };
        api.confirm(&id, &yes).await.unwrap();
        let detail = api.chat(&id).await.unwrap();
        assert_eq!(detail.chat.state, ChatState::Idle);
        let card = detail.items.iter().find(|i| &i.id == item_id).unwrap();
        assert!(matches!(
            &card.kind,
            ChatItemKind::Card { card: Card::Confirmation(c), .. } if matches!(c.decision, Decision::Approved { .. })
        ));
        assert_eq!(api.library().await.unwrap().notes.len(), notes - 1);
        // Answered: asking again is refused.
        assert_eq!(api.confirm(&id, &yes).await.unwrap_err().status, Some(409));

        // Denied: nothing is deleted, the agent is told and says so.
        let notes = api.library().await.unwrap().notes.len();
        api.send_message(&id, &message("Delete my first note"))
            .await
            .unwrap();
        let state = api.chat(&id).await.unwrap().chat.state;
        let ChatState::WaitingForUser { item_id, .. } = state else {
            panic!("{state:?}");
        };
        let no = ConfirmRequest {
            item_id: item_id.clone(),
            approve: false,
        };
        api.confirm(&id, &no).await.unwrap();
        let detail = api.chat(&id).await.unwrap();
        assert_eq!(detail.chat.state, ChatState::Idle);
        let card = detail.items.iter().find(|i| i.id == item_id).unwrap();
        assert!(matches!(
            &card.kind,
            ChatItemKind::Card { card: Card::Confirmation(c), .. }
                if c.decision == Decision::Denied { device_id: "d1".into(), by_stop: false }
        ));
        assert!(detail.items.iter().any(|i| matches!(
            &i.kind,
            ChatItemKind::ToolResult { ok: false, summary, .. } if summary == "Declined"
        )));
        assert_eq!(api.library().await.unwrap().notes.len(), notes);
    }

    #[tokio::test]
    async fn stop_denies_a_waiting_confirmation() {
        use margin_shared::{Card, ChatItemKind, ChatState, Decision};
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        let id = api
            .create_chat(&NewChat {
                message: Some(message("Delete my first note")),
                workspace_ids: vec![],
            })
            .await
            .unwrap()
            .chat
            .id;
        let ChatState::WaitingForUser { item_id, .. } = api.chat(&id).await.unwrap().chat.state
        else {
            panic!("not waiting");
        };
        api.stop_chat(&id).await.unwrap();
        let detail = api.chat(&id).await.unwrap();
        let card = detail.items.iter().find(|i| i.id == item_id).unwrap();
        assert!(matches!(
            &card.kind,
            ChatItemKind::Card { card: Card::Confirmation(c), .. }
                if matches!(c.decision, Decision::Denied { by_stop: true, .. })
        ));
        // Too late to answer it now.
        let late = ConfirmRequest {
            item_id,
            approve: true,
        };
        assert_eq!(
            api.confirm(&id, &late).await.unwrap_err().code,
            "not_waiting"
        );
    }

    #[tokio::test]
    async fn workspaces_register_and_go() {
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        assert!(api.workspaces().await.unwrap().is_empty());
        let w = api
            .create_workspace(&NewWorkspace {
                name: "margin".into(),
                platform: margin_shared::Platform::Macos,
                location: "~/Developer/margin".into(),
            })
            .await
            .unwrap();
        assert_eq!(w.device_id, "d1");
        assert_eq!(api.workspaces().await.unwrap(), vec![w.clone()]);
        api.delete_workspace(&w.id).await.unwrap();
        assert_eq!(
            api.delete_workspace(&w.id).await.unwrap_err().status,
            Some(404)
        );
    }

    #[tokio::test]
    async fn agent_settings_patch() {
        use margin_shared::ReasoningEffort;
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        let settings = api
            .patch_settings(&SettingsPatch {
                reasoning_effort: Some(ReasoningEffort::High),
                agent_model: Some(Some("gpt-6.1-sol".into())),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(settings.reasoning_effort, ReasoningEffort::High);
        assert_eq!(settings.agent_model.as_deref(), Some("gpt-6.1-sol"));
        let cleared = api
            .patch_settings(&SettingsPatch {
                agent_model: Some(None),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(cleared.agent_model, None);
        assert_eq!(cleared.reasoning_effort, ReasoningEffort::High);
    }

    /// What Settings → AI (model slots, pipeline budget, Reset to default) and Settings →
    /// Advanced (trace retention) send, through the fake service.
    #[tokio::test]
    async fn model_slots_pipeline_budget_and_trace_retention() {
        use crate::ui::settings::{
            budget_fields, parse_budget, reset_pipeline, slot_value, slots_patch,
        };
        use margin_shared::pipeline::{Budget, PipelineConfig, Route, StageCfg};
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        let settings = api.settings().await.unwrap();
        assert_eq!(settings.pipeline, PipelineConfig::default());

        // The three slots: chats on agent_model, writing and small tasks on the routes.
        let wanted = [
            "gpt-6.1-sol".to_string(),
            " gpt-6 ".to_string(),
            "gpt-6-luna".to_string(),
        ];
        let settings = api
            .patch_settings(&slots_patch(&settings, &wanted))
            .await
            .unwrap();
        assert_eq!(settings.agent_model.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(settings.pipeline.routes.get(Route::Write), Some("gpt-6"));
        assert_eq!(
            settings.pipeline.routes.get(Route::Small),
            Some("gpt-6-luna")
        );
        assert_eq!(settings.pipeline.routes.get(Route::Chat), None);
        assert_eq!(slot_value(&settings, Route::Write), "gpt-6");
        // Nothing changed: nothing to send.
        assert_eq!(slots_patch(&settings, &wanted), SettingsPatch::default());

        // The budget's numbers, saved with the rest of the pipeline kept.
        let mut fields = budget_fields(&settings.pipeline.budget);
        assert_eq!(fields, ["50", "25", "8", "0"].map(String::from));
        fields[0] = "75".into();
        fields[2] = "24".into();
        let budget = parse_budget(&fields).unwrap();
        let settings = api
            .patch_settings(&SettingsPatch {
                pipeline: Some(PipelineConfig {
                    budget,
                    ..settings.pipeline.clone()
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(settings.pipeline.budget.share_of_window, 0.75);
        assert_eq!(settings.pipeline.budget.images_max, 24);
        assert_eq!(
            settings.pipeline.routes.get(Route::Small),
            Some("gpt-6-luna")
        );
        assert_ne!(settings.pipeline.hash(), PipelineConfig::default().hash());

        // A stage the service doesn't have is refused, naming it.
        let mut bad = settings.pipeline.clone();
        bad.retriever = StageCfg::named("telepathy");
        let err = api
            .patch_settings(&SettingsPatch {
                pipeline: Some(bad),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!((err.status, err.code.as_str()), (Some(400), "bad_pipeline"));
        assert!(err.message.contains("telepathy"), "{}", err.message);

        // Reset to default: stages and budget back, the slots kept.
        let settings = api
            .patch_settings(&SettingsPatch {
                pipeline: Some(reset_pipeline(&settings.pipeline)),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(settings.pipeline.budget, Budget::default());
        assert_eq!(settings.pipeline.routes.get(Route::Write), Some("gpt-6"));
        assert_eq!(reset_pipeline(&settings.pipeline), settings.pipeline);

        // Trace retention, in range or refused.
        let settings = api
            .patch_settings(&SettingsPatch {
                trace_retention_days: Some(30),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(settings.trace_retention_days, 30);
        let err = api
            .patch_settings(&SettingsPatch {
                trace_retention_days: Some(0),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(err.status, Some(400));
        assert_eq!(api.settings().await.unwrap().trace_retention_days, 30);
    }

    /// The trace routes answer as the service's do when there are none.
    #[tokio::test]
    async fn trace_routes_are_empty() {
        let fake = FakeService::start().await;
        let http = reqwest::Client::new();
        let get = |path: &str| {
            http.get(format!("{}{path}", fake.url))
                .bearer_auth(crate::fake::TOKEN)
                .send()
        };
        let traces = get(routes::TRACES).await.unwrap();
        assert_eq!(traces.status(), 200);
        assert_eq!(
            traces.headers()["content-type"].to_str().unwrap(),
            "application/x-ndjson"
        );
        assert_eq!(traces.text().await.unwrap(), "");
        let rollup = get(routes::TRACES_ROLLUP).await.unwrap();
        assert_eq!(rollup.status(), 200);
        assert_eq!(rollup.text().await.unwrap(), "[]");
        let anonymous = http
            .get(format!("{}{}", fake.url, routes::TRACES))
            .send()
            .await
            .unwrap();
        assert_eq!(anonymous.status(), 401);
    }

    /// A text citation with lines opens on the original file's lines, fetched from its
    /// signed URL.
    #[tokio::test]
    async fn cited_lines_come_from_the_original_file() {
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        let lib = api.library().await.unwrap();
        let lesson = lib.lessons.iter().find(|l| l.id == "l1").unwrap();
        let cited = lesson
            .citations
            .iter()
            .find(|c| c.source_id == "s2")
            .unwrap();
        let source = lib.sources.iter().find(|s| s.id == "s2").unwrap();
        let view =
            crate::citations::viewer(cited, &lesson.citations, source.media_url.as_deref(), &[]);
        let crate::citations::Viewer::Lines {
            excerpt,
            start_line,
            end_line,
            original: Some(url),
            ..
        } = view
        else {
            panic!("not lines: {view:?}");
        };
        let text = api.fetch_text(&url).await.unwrap();
        let lines = crate::citations::line_window(&text, start_line, end_line, &excerpt).unwrap();
        let marked: Vec<u32> = lines.iter().filter(|l| l.cited).map(|l| l.number).collect();
        assert_eq!(marked, [3]);
        assert_eq!(lines.first().unwrap().number, 1);
        assert!(lines[2].text.starts_with("Near a jump"));
    }

    #[tokio::test]
    async fn chats_take_files_with_a_message_and_let_them_go() {
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        // Uploaded from the composer: no topic.
        let file = TempFile::new("chat-file", b"# Gibbs\n");
        let source = api
            .upload_source(&file.0, "gibbs.md", None, None, |_| {})
            .await
            .unwrap();
        assert_eq!(source.topic_id, None);
        let unknown = api
            .create_chat(&NewChat {
                message: Some(NewMessage {
                    text: "Read this".into(),
                    source_ids: vec!["nope".into()],
                }),
                workspace_ids: vec![],
            })
            .await
            .unwrap_err();
        assert_eq!(
            (unknown.status, unknown.code.as_str()),
            (Some(400), "unknown_source")
        );
        let created = api
            .create_chat(&NewChat {
                message: Some(NewMessage {
                    text: "Read this".into(),
                    source_ids: vec![source.id.clone()],
                }),
                workspace_ids: vec![],
            })
            .await
            .unwrap();
        let id = created.chat.id.clone();
        assert_eq!(created.chat.source_ids, vec![source.id.clone()]);
        assert!(matches!(
            &created.items[0].kind,
            margin_shared::ChatItemKind::UserMessage { source_ids, .. } if *source_ids == vec![source.id.clone()]
        ));
        // Taken off the chat, a file no topic has is deleted.
        let detached = api
            .patch_chat(
                &id,
                &ChatPatch {
                    source_ids: Some(vec![]),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(detached.source_ids.is_empty());
        let library = api.library().await.unwrap();
        assert!(!library.sources.iter().any(|s| s.id == source.id));
        // One a topic also has stays.
        let in_topic = api
            .upload_source(&file.0, "kept.md", None, Some("fourier"), |_| {})
            .await
            .unwrap();
        api.send_message(
            &id,
            &NewMessage {
                text: "And this".into(),
                source_ids: vec![in_topic.id.clone()],
            },
        )
        .await
        .unwrap();
        let chat = api.chat(&id).await.unwrap().chat;
        assert_eq!(chat.source_ids, vec![in_topic.id.clone()]);
        api.patch_chat(
            &id,
            &ChatPatch {
                source_ids: Some(vec![]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(
            api.library()
                .await
                .unwrap()
                .sources
                .iter()
                .any(|s| s.id == in_topic.id)
        );
        // Deleting the chat deletes the files only it had.
        let only_here = api
            .upload_source(&file.0, "gone.md", None, None, |_| {})
            .await
            .unwrap();
        api.send_message(
            &id,
            &NewMessage {
                text: "One more".into(),
                source_ids: vec![only_here.id.clone()],
            },
        )
        .await
        .unwrap();
        api.delete_chat(&id).await.unwrap();
        let library = api.library().await.unwrap();
        assert!(!library.sources.iter().any(|s| s.id == only_here.id));
        assert!(library.sources.iter().any(|s| s.id == in_topic.id));
    }

    #[tokio::test]
    async fn chats_are_named_by_the_model_until_the_owner_renames_them() {
        use margin_shared::TitleSource;
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        let created = api
            .create_chat(&NewChat {
                message: Some(message("How do Fourier series converge?\nIn detail")),
                workspace_ids: vec![],
            })
            .await
            .unwrap();
        // Until the first answer: the first line, as a stand-in.
        assert_eq!(created.chat.title, "How do Fourier series converge?");
        assert_eq!(created.chat.title_by, TitleSource::FirstLine);
        let id = created.chat.id;
        let named = api.chat(&id).await.unwrap().chat;
        assert_eq!(named.title_by, TitleSource::Model);
        assert_eq!(named.title, "Fourier series converge");
        let renamed = api
            .patch_chat(
                &id,
                &ChatPatch {
                    title: Some("Convergence".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(renamed.title_by, TitleSource::Owner);
        // Later answers keep the owner's name.
        api.send_message(&id, &message("And Gibbs?")).await.unwrap();
        let chat = api.chat(&id).await.unwrap().chat;
        assert_eq!(
            (chat.title.as_str(), chat.title_by),
            ("Convergence", TitleSource::Owner)
        );
    }

    #[tokio::test]
    async fn chats_wait_when_the_model_api_asks_and_stop_ends_the_wait() {
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        fake.state.lock().unwrap().chat_hang = true;
        let id = api
            .create_chat(&NewChat {
                message: Some(message("Please slow down")),
                workspace_ids: vec![],
            })
            .await
            .unwrap()
            .chat
            .id;
        let chat = api.chat(&id).await.unwrap().chat;
        assert!(chat.state.is_busy());
        let waiting = chat.waiting.expect("waiting before the retry");
        assert!(waiting.until > chrono::Utc::now());
        assert_eq!(waiting.reason, "The model API asked Margin to slow down.");
        let stopped = api.stop_chat(&id).await.unwrap();
        assert_eq!(stopped.waiting, None);
        assert!(!stopped.state.is_busy());
    }

    #[tokio::test]
    async fn chat_errors_say_what_kind_they_are() {
        use margin_shared::{ChatItemKind, error_code};
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        fake.state.lock().unwrap().has_api_key = false;
        let id = api
            .create_chat(&NewChat {
                message: Some(message("Hello")),
                workspace_ids: vec![],
            })
            .await
            .unwrap()
            .chat
            .id;
        let detail = api.chat(&id).await.unwrap();
        assert!(matches!(
            &detail.items.last().unwrap().kind,
            ChatItemKind::Error { code: Some(c), retryable: true, .. } if c == error_code::NO_API_KEY
        ));
        {
            let mut s = fake.state.lock().unwrap();
            s.has_api_key = true;
            s.settings.openai_model = String::new();
        }
        let resumed = api.continue_chat(&id).await;
        assert!(resumed.is_ok());
        let detail = api.chat(&id).await.unwrap();
        assert!(matches!(
            &detail.items.last().unwrap().kind,
            ChatItemKind::Error { code: Some(c), .. } if c == error_code::NO_MODEL
        ));
        let failure = ApiFailure {
            status: Some(400),
            code: "no_model".into(),
            message: "Choose a model.".into(),
        };
        assert_eq!(
            failure.settings_pane(),
            Some(crate::ui::settings::SettingsPane::Ai)
        );
    }

    #[tokio::test]
    async fn chats_write_lessons_and_make_videos_with_result_cards() {
        use margin_shared::{Card, ChatItemKind, LessonStatus, ResultTarget, VideoStatus};
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        let id = api
            .create_chat(&NewChat {
                message: Some(message("Write a lesson on the Gibbs phenomenon in Fourier")),
                workspace_ids: vec![],
            })
            .await
            .unwrap()
            .chat
            .id;
        let detail = api.chat(&id).await.unwrap();
        assert_eq!(
            kinds(&detail),
            ["user", "reasoning", "call", "result", "card", "text"]
        );
        let lesson_id = detail
            .items
            .iter()
            .find_map(|i| match &i.kind {
                ChatItemKind::Card {
                    card: Card::Result(c),
                    ..
                } => match &c.target {
                    ResultTarget::Lesson { lesson_id } => Some(lesson_id.clone()),
                    _ => None,
                },
                _ => None,
            })
            .expect("a lesson card");
        let library = api.library().await.unwrap();
        let lesson = library.lessons.iter().find(|l| l.id == lesson_id).unwrap();
        assert_eq!(lesson.title, "The Gibbs phenomenon in Fourier");
        assert_eq!(lesson.status, LessonStatus::Queued);

        api.send_message(&id, &message("Make a video of Why sums of sines"))
            .await
            .unwrap();
        let detail = api.chat(&id).await.unwrap();
        let video_lesson = detail
            .items
            .iter()
            .rev()
            .find_map(|i| match &i.kind {
                ChatItemKind::Card {
                    card: Card::Result(c),
                    ..
                } => match &c.target {
                    ResultTarget::Video { lesson_id } => Some(lesson_id.clone()),
                    _ => None,
                },
                _ => None,
            })
            .expect("a video card");
        let library = api.library().await.unwrap();
        let video = library
            .videos
            .iter()
            .find(|v| v.lesson_id == video_lesson)
            .unwrap();
        assert_eq!(video.status, VideoStatus::Queued);
    }

    #[tokio::test]
    async fn a_newer_outline_supersedes_the_chats_earlier_one() {
        use margin_shared::{Card, ChatItemKind};
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        let outlines = |detail: &ChatDetail| -> Vec<Id> {
            detail
                .items
                .iter()
                .filter_map(|i| match &i.kind {
                    ChatItemKind::Card {
                        card: Card::Outline(c),
                        ..
                    } => Some(c.proposal_id.clone()),
                    _ => None,
                })
                .collect()
        };
        let id = api
            .create_chat(&NewChat {
                message: Some(message("A course on control theory")),
                workspace_ids: vec![],
            })
            .await
            .unwrap()
            .chat
            .id;
        api.send_message(&id, &message("Make it a course on robotics instead"))
            .await
            .unwrap();
        let ids = outlines(&api.chat(&id).await.unwrap());
        assert_eq!(ids.len(), 2);
        let first = api.proposal(&ids[0]).await.unwrap();
        assert_eq!(
            first.status,
            ProposalStatus::Superseded { by: ids[1].clone() }
        );
        assert_eq!(
            api.proposal(&ids[1]).await.unwrap().status,
            ProposalStatus::Ready
        );
        let body = AcceptProposal {
            outline: first.outline.clone().unwrap(),
            chips: Default::default(),
        };
        let refused = api.accept_proposal(&ids[0], &body).await.unwrap_err();
        assert_eq!(
            (refused.status, refused.code.as_str()),
            (Some(409), "superseded")
        );
    }

    #[tokio::test]
    async fn network_failure_has_readable_message() {
        let api = Api::anonymous("http://127.0.0.1:9");
        let err = api.health().await.unwrap_err();
        assert_eq!(err.code, "network");
        assert!(
            err.message
                .starts_with("Couldn't reach the service at http://127.0.0.1:9")
        );
    }

    #[test]
    fn events_url_uses_ws_scheme() {
        assert_eq!(
            Api::anonymous("https://m.example/").events_url(),
            "wss://m.example/api/events"
        );
        assert_eq!(
            Api::anonymous("http://127.0.0.1:8787").events_url(),
            "ws://127.0.0.1:8787/api/events"
        );
    }
}
