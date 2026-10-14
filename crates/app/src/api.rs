//! HTTP client for the Margin service, coded against `margin_shared::routes`.
//! Every failure comes back as an [`ApiFailure`] whose `message` is fit to show in the UI.

use std::fmt;
use std::path::Path;
use std::time::Duration;

use margin_shared::{
    AcceptProposal, ApiError, AskRequest, Device, EmbeddingModel, Id, Lesson, LessonPatch, Library,
    LoginState, Me, NewNote, NewTopic, Note, NotePatch, PollLoginRequest, Proposal, SearchHit,
    Settings, SettingsPatch, Source, SourceKind, SourceUpload, StartLogin, StartLoginRequest,
    ToolStatus, Topic, TopicPatch, TranscriptSegment, Video, routes,
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
            "no_api_key" => Some(SettingsPane::Ai),
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

    pub async fn ask(&self, body: &AskRequest) -> ApiResult<Proposal> {
        self.post(routes::ASK, body).await
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
    /// without `topic_id` it waits to be named in an `AskRequest`. A file over its
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
const PART_RETRIES: u32 = 3;

/// Whether a failed part is worth sending again: the connection broke or timed out, the
/// service had a hiccup, or it has a different amount of the file than we thought.
fn retry_part(e: &ApiFailure) -> bool {
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
    use margin_shared::{Level, ProposalStatus, SearchKind};

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
    async fn ask_reports_no_api_key_and_accepts_outline() {
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        let req = AskRequest {
            prompt: "Fourier series".into(),
            lesson_count: 3,
            level: Level::Intro,
            with_videos: true,
            with_quiz: true,
            parent_topic_id: None,
            source_ids: vec![],
        };
        fake.state.lock().unwrap().has_api_key = false;
        let err = api.ask(&req).await.unwrap_err();
        assert!(err.is_no_api_key());
        fake.state.lock().unwrap().has_api_key = true;
        let proposal = api.ask(&req).await.unwrap();
        assert_eq!(proposal.status, ProposalStatus::Drafting);
        let fetched = api.proposal(&proposal.id).await.unwrap();
        assert_eq!(fetched.status, ProposalStatus::Ready);
        let topic = api
            .accept_proposal(
                &proposal.id,
                &AcceptProposal {
                    outline: fetched.outline.unwrap(),
                },
            )
            .await
            .unwrap();
        assert_eq!(topic.title, "Fourier series");
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

        // Accepting a proposal that names the source attaches it to the new topic.
        let req = AskRequest {
            prompt: "Fourier series".into(),
            lesson_count: 1,
            level: Level::Intro,
            with_videos: false,
            with_quiz: false,
            parent_topic_id: None,
            source_ids: vec![source.id.clone()],
        };
        let proposal = api.ask(&req).await.unwrap();
        let outline = api.proposal(&proposal.id).await.unwrap().outline.unwrap();
        let topic = api
            .accept_proposal(&proposal.id, &AcceptProposal { outline })
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
