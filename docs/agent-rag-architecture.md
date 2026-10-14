# Agent + RAG pipeline: review, target architecture, evals, build plan

Written 2026-10-14 at `fd1e848` on `claude/agent-build`, on the owner's request: "re-architect the agent + RAG pipeline for making the generation efficient … for best practices with evals to track. We are a real lab optimizing its product." Read: `docs/HANDOFF.md`, `PLAN.md`, `docs/agent-architecture.md`, `docs/agent-contract.md`, `docs/agent-providers-brief.md`, `docs/agent-phase4-brief.md`; `crates/embed/src/{lib,chunk,rank,pdf,extract,image,vision,coreml,fake}.rs`; `crates/service/src/{db,sources,search,embedding,ask,agent_tools,turns,engines,video}.rs`; `crates/agent/src/{lib,turn,tool,prompt,replay}.rs`; `crates/ai/src/{lib,prompts,citations}.rs`; `Cargo.lock` (SurrealDB **3.3.4**, `kv-surrealkv` + `kv-mem`, no other features). SurrealDB facts were checked against the 3.x docs (`/docs/reference/query-language/statements/define/indexes`, `/docs/learn/data-models/vector-search/hybrid-search`, `/docs/reference/query-language/functions/database-functions/search`). No code changed; no other doc touched.

Owner decisions this design respects: folders use a file-type allow-list (prose documents, PDFs, images), filtered on the device before upload; **folder files are ingested into SurrealDB as they upload, embedded with EmbeddingGemma and searched by vector search, including inside PDFs; no file-size or picture caps, because the agent retrieves chunks rather than reading whole files**; code arrives later through a repository source; providers switch per chat (OpenAI key, ChatGPT sign-in, Claude API); keys never on devices; tests use fakes only; no placeholder UI; scope is never narrowed without asking.

Note on the phase-4 brief as it stands: its D7a/D7b build a *byte cache* of folder PDFs/images ("Nothing here is a source: no chunks, no embeddings", brief line 197) with `FOLDER_FETCH_MAX_BYTES` 32 MB, `PDF_PAGES_PER_READ` 10 and `VIEW_IMAGES_PER_TURN` 8. The owner's later decision (ingest, embed, retrieve, no caps) replaces that part. Section 4 says which phase-4 areas wait for the retrieval core and which go ahead unchanged.

---

## 1. Today's pipeline, end to end

### 1.1 Ingestion

Upload → `data_dir/sources/{id}/file` → one background worker, one source at a time (`sources.rs:478-485`). `process` (`sources.rs:525-758`): extract (PDFKit text per page, Vision OCR for pages under 16 non-space chars, **every page rendered to a 2048-px PNG** `pdf.rs:24-69`), transcribe audio, chunk, embed, save.

Concrete inefficiencies and risks:

- **No real embedding batching.** `EMBED_BATCH = 16` (`sources.rs:55`) only groups the *save*; `CoreMlEmbedder::embed` maps `embed_one` per text (`coreml.rs:313-318`), each a separate Core ML `predict` on a `[1,S,512]` function. The model's compiled functions are single-sequence, so true batching needs a batched `.mlpackage`; what is cheap today is pipelining (tokenize/lookup on one thread while the ANE predicts) and skipping work (next point).
- **Re-ingest redoes everything.** `process` deletes all chunk and media rows and re-embeds every chunk (`sources.rs:634-635`), and "Read again" is the only incremental path. There is no content hash, so an unchanged file (or an unchanged page of a changed PDF) is embedded again. For folders, where files change in place, this is the dominant cost.
- **Row-at-a-time writes under the global write lock.** `save_rows` does one `UPSERT` per chunk while holding `state.write_lock` (`sources.rs:846-863`), so a 400-chunk PDF takes 400 round trips and blocks every other write for their duration.
- **Vectors are stored as JSON floats inside `doc`** (`ChunkRow.vector: Vec<f32>` → `UPSERT … CONTENT { doc: $doc }`, `db.rs:142-156`). 768 floats serialize to ~15 KB of text per chunk and come back through `serde_json::Value` on every read.
- **Page renders are kept at full size for every page** (`pdf.rs:31-34`, PNG) whether or not the page is ever cited or shown; a 300-page scan costs hundreds of MB.
- **Pictures carry no text.** `media_items` sets `text: ""` for images and PDF pages (`sources.rs:908-925`; only video frames get "said around then" text), so the term boost never applies to them and the lesson writer gets "picture attached below" with nothing to search on. The page's OCR/text layer exists as a `ChunkRow` but is not linked to the page's `MediaRow`.
- **Chunking is page- and sentence-aware but not structure-aware** (`chunk.rs:42-62`, `191-253`): no headings, no section path, no "title" field for the embedding prefix (always `title: none | text:`, `embed/lib.rs:76-82`), although EmbeddingGemma's document prompt accepts a title that measurably helps retrieval. Overlap is 10 %, and the overlap region is later not deduplicated at retrieval (adjacent chunks both rank and both get sent).

### 1.2 Storage and indexing in SurrealDB

- Every table is `SCHEMALESS` with **no index of any kind** (`db.rs:36-39`, `77`). `list_where` is `SELECT VALUE doc FROM chunk WHERE doc.source_id = $value` (`db.rs:121-139`): a **full table scan per call**.
- **Vectors use no SurrealDB vector index; ranking is brute force in Rust.** `retrieve` loads *every* chunk and media row of *every* source in scope, one `list_where` per source (`sources.rs:1084-1096`), then scores all of them with `cosine + term_boost` in `spawn_blocking` (`sources.rs:1112-1132`). Cost is O(sources × table size) for the scans plus O(chunks) for scoring, with all vectors deserialized from JSON each time. With `search_sources`' default scope of "the chat's files, then every other source" (`agent_tools.rs:777-783`) the agent reloads the whole corpus on every call. SurrealDB 3.3 has HNSW and BM25 (section 2.2); neither is used.
- No full-text index: the lexical signal is a hand-rolled term boost capped at +0.15 with a 60-word stop list (`rank.rs:8-19`), no IDF, no stemming, no phrase matching.
- `⌘K` search loads every topic, lesson and note on every keystroke and does substring matching in Rust (`search.rs:32-35`, `171-187`). It isn't RAG, but it is the same missing index.

### 1.3 Retrieval, ranking, context assembly

- **One query vector per job.** The lesson query is `title + summary + prompt` concatenated (`ask.rs:566-571`); no rewriting, no multi-query, no use of the outline neighbours to steer away from material the previous/next lesson covers.
- **Fixed k.** `PASSAGES = 8`, `MAX_IMAGES = 4` (`sources.rs:59-61`) regardless of lesson length, source count or score distribution; a 480-token chunk × 8 is ≤ 3.8k text tokens, but 4 pictures at 1536 px are each ~1–1.5k input tokens on OpenAI "high" detail, so pictures can be half the prompt while contributing least.
- **Cross-modal scores aren't calibrated.** Text↔image cosine from one joint space lives on a different scale than text↔text, and pictures have empty text (1.1), so a picture is picked on vector score alone against text chunks with a term boost; in practice pictures either flood or never appear, and nothing measures which.
- **Image bytes are read and JPEG-encoded inside the ranking loop** for every picture candidate until four are kept (`sources.rs:1149-1170`): retrieval does file I/O and image encoding, and the dedup is by full JPEG bytes equality.
- **Dedup is shallow**: exact normalized-text equality (`sources.rs:1175-1177`) and transcript overlap; adjacent overlapping chunks and near-duplicates both go in; no per-source diversity, so one source can take all 8 slots.
- **No reranker**, no score threshold: a lesson with irrelevant sources still gets 8 passages and the prompt still says "base the lesson on the sources".
- Retrieval is recomputed per lesson job (`lesson_context` → `passages`) with no caching, and `retrieve` waits up to 10 minutes for sources still being read (`SOURCE_WAIT`), which is fine for correctness but hides ingest latency.

### 1.4 The agent loop

- Good: `store: false` with full replay, `prompt_cache_key = chat_id`, a stable developer message (`prompt.rs:7-15`), `allowed_tools` gating instead of changing the tools array (`turn.rs:447-485`), server compaction with a local condense fallback, per-call and per-turn output caps (`agent/lib.rs:58-83`).
- `search_sources` is the only retrieval tool: top-8 JSON passages with full text and a Passages card (`agent_tools.rs:651`, `705-716`). There is no "get more of this source", no "search this lesson's library", no page-range read for a *source* PDF; the agent's only way to read more is to search again.
- The library tools are not retrieval: `search_library` is the substring search (1.2), `read_lesson` returns whole bodies (`agent_tools.rs:466`); a chat about a 12-lesson course reads 12 × 2k words into context.
- **Side calls run on the chat model**: the condense summary (`turn.rs:1160-1171`) and the title (`turns.rs:1560-1580`, `TITLE_MAX_OUTPUT_TOKENS` 1024 for a six-word title). Providers Q3 already recommends the cheapest model; this doc makes it a routing table (2.6).
- The system prompt says "There is no web search" (`prompt.rs:35-36`); providers D6 rewrites it. Noted so the eval's prompt-version field covers both.

### 1.5 Generation jobs

- **Lesson** (`ai/lib.rs:589-610`, `prompts.rs:108-245`): the instructions put the *variable* parts first (`Level: …`, minutes, words at `prompts.rs:115`, `148`) and append `LESSON_SOURCES` only when there are passages (`:157`), so no two lessons share a cacheable instruction prefix; `ResponsesClient::body` sends no `prompt_cache_key` (`ai/lib.rs:126-134`). Each lesson is one call with ≤ 8 passages and ≤ 4 pictures; up to 3 run concurrently (`state.rs:38`, `lesson_slots`).
- **Outline**: written by the agent inside a chat turn as `propose_course` arguments (`engines.rs:40-41`, `agent_tools.rs:1426`). The agent decides whether to search sources first; there is no structured "survey the sources" step, so outlines of source-grounded courses depend on how many `search_sources` calls the model happens to make (each returns 8 passages).
- **Video scene**: the whole lesson body plus a ~14 KB cheat sheet per call (`prompts.rs:375-395`, `MANIMGX_CHEAT_SHEET`), retried once with the failed code appended. The cheat sheet is a perfect cache prefix and is not cached.
- **Title / condense**: 1.4.

### 1.6 Citations

`cite` renumbers `[^n]` by first appearance and copies the passage's whole text as the excerpt (`sources.rs:1246-1261`, `:1240`). Nothing checks that a cited passage supports the sentence it follows, that every source-derived claim carries a marker, or that `[^n]` markers the model wrote for passages it didn't use are rare. No metric exists for any of this.

### 1.7 Tests and evals

527 tests, all on fakes: `FakeEmbedder` is a hashed bag of words (`embed/fake.rs:38-44`), `FakeSourceTools` scripts extraction, `FakeGenerator` returns fixed bodies. They prove the plumbing. **Nothing measures retrieval quality, citation accuracy, tokens, cost or latency**, and no fixture corpus or golden query set exists (`crates/ai/tests/fixtures` holds ManimGX scenes only).

---

## 2. Target architecture

### 2.0 Design stance: pluggable stages, configuration over code

The owner's framing: a start-up whose product vision keeps changing. So the pipeline is a chain of **named stages behind traits**, chosen by **configuration**, and the eval runner compares configurations, not branches. Nothing below is hard-coded where a table row will do, and details likely to move (heading detection heuristics, the cross-encoder model, prompt wording, k values) are variants, not architecture.

```rust
// crates/service/src/pipeline.rs (R1 first commit)
pub trait Chunker:  Send + Sync { fn name(&self) -> &'static str; fn chunk(&self, doc: &Extracted, kind: SourceKind, tok: &dyn Fn(&str) -> usize, cfg: &ChunkerCfg) -> Vec<Chunk>; }
pub trait Retriever: Send + Sync { fn name(&self) -> &'static str; fn search(&self, q: &Query) -> BoxFuture<'_, anyhow::Result<Vec<Hit>>>; }
pub trait Fusion:    Send + Sync { fn name(&self) -> &'static str; fn fuse(&self, legs: Vec<Vec<Hit>>, cfg: &FusionCfg) -> Vec<Hit>; }
pub trait Reranker:  Send + Sync { fn name(&self) -> &'static str; fn rerank(&self, q: &str, hits: Vec<Hit>) -> BoxFuture<'_, anyhow::Result<Vec<Hit>>>; }
pub trait Packer:    Send + Sync { fn name(&self) -> &'static str; fn pack(&self, hits: Vec<Hit>, budget: &Budget) -> Vec<Packed>; }
pub trait PromptSet: Send + Sync { fn version(&self) -> &'static str; fn lesson(&self, ctx: &LessonContext) -> Prompt; fn plan(&self, ..) -> Prompt; fn rerank(&self, ..) -> Prompt; /* … */ }

pub struct Pipeline { chunker, embedder, retriever, fusion, reranker, packer, prompts, routes: RouteTable }
pub struct PipelineConfig { /* serde: every stage by name + its cfg table */ }
impl Pipeline { pub fn build(cfg: &PipelineConfig, reg: &Registry) -> anyhow::Result<Self>; }
```

- **One config, three readers.** `PipelineConfig` (TOML; serde) lives in Settings (`settings.pipeline`, editable through `SettingsPatch` and shown in Settings → AI → Pipeline as real controls: a variant picker plus the numeric budgets), is the same shape as `evals/variants/*.toml`, and is stamped onto every proposal, lesson and eval run (`pipeline_hash`). The service, the eval runner and the tests build the same `Pipeline` from it; "switch the product to variant B" is a Settings change, not a release.
- **Registry by name.** `Registry` maps names to constructors (`"sections-v2"`, `"pages-v1"`, `"hybrid-rrf"`, `"dense"`, `"bm25"`, `"listwise-small"`, `"none"`, …). Adding a variant is one file plus one registration; removing one is deleting it. Tests register fakes under the same names they fake.
- **Stable seams, movable interiors.** What is frozen in the first commit: the stage traits, `Query`/`Hit`/`Packed`/`Citation`, the `passage` table's locator fields (2.2) and `PipelineConfig`'s top-level keys. What is expected to move: every stage's interior, the prompts (versioned), k values, budgets, the routing defaults, the allow-list, the analyzer. The indexes are defined with `OVERWRITE` from code so an analyzer or HNSW parameter change is a startup migration, not a schema project.
- **Evals are the switchboard.** Because the runner takes the same config, a product pivot ("lessons should be shorter", "cite less", "images matter more") is a variant file, a `compare`, and a Settings change. Section 3.5.
- **Don't over-build now**: no heading detection from PDF fonts, no Matryoshka index, no DISKANN, no cross-encoder until an eval asks for it; each is listed where it would go and behind which trait.

### 2.1 Ingestion and chunking (crate `margin-embed`, service `ingest.rs` new)

One ingestion pipeline for every origin: uploaded sources and folder files (phase 4), later repository files. A document is `Document { origin: Origin::Source(id) | Origin::FolderFile { workspace_id, path }, name, kind, content_hash, stamp }`.

**Chunking per type** (`embed/chunk.rs`, extended):

| Type | Unit | Rule |
|---|---|---|
| Prose (`.md .txt .rst .tex .html …`) | Section-aware | Parse headings (Markdown `#`, reST underlines, HTML `h1–h6`, LaTeX `\section`); chunk within a section; each chunk carries `section_path` ("Chapter 2 > Fourier series"), `start_line`/`end_line`, `ord`. Target 256–400 tokens, hard cap `CHUNK_TOKENS` 480, overlap one sentence (not 10 % of tokens). Tables and code fences are atomic units. |
| PDF (text layer or OCR) | Page, then section when headings are detectable | As today per page (`chunk.rs:50-60`), plus: a page is a document-level unit that can span chunks; chunks carry `page`; the page's OCR/text is also written as the page picture's `text` (fixes 1.1). Heading detection from PDFKit's font-size runs is a later refinement behind the chunker version field. |
| Image | One unit | Vision OCR text (`vision.rs` already exists) as the chunk `text`; the picture embedded with the image encoder; both rows share one `unit_id` so a hit on either returns the other. |
| Audio/video (sources only) | Transcript passage / 10-s window | Unchanged. |

**Embedding prefix**: `title: {source name or section path} | text: {chunk}` instead of `title: none` (`embed/lib.rs:76-82`). Recorded as `embedder_version` so old rows are re-embedded lazily (2.7).

**Pipeline shape** (`service/ingest.rs`): extract → chunk → `content_hash` per chunk → **diff against stored rows** (same `document_id`, same `chunk_hash`, same `embedder_version` → keep; new → embed; gone → delete) → embed new chunks in a pipelined worker (tokenize/lookup on a thread, `predict` on another, output written in one `INSERT` of up to 64 rows per statement, the write lock held per statement, not per row) → mark `Ready`. Page pictures rendered **on demand** at first retrieval or citation (cached under `media/`), not at ingest; ingest keeps only the OCR text and the image-encoder vector (the encoder reads the page at its own resolution). Progress events unchanged.

### 2.2 Storage and indexing (SurrealDB 3.3.4, `service/db.rs`)

Replace the `chunk` and `media` tables with one schemafull `passage` table whose indexed fields are top level (not under `doc`), so the indexes can name them:

```sql
DEFINE TABLE passage SCHEMAFULL;
DEFINE FIELD document_id   ON passage TYPE string;
DEFINE FIELD origin        ON passage TYPE string;        -- "source" | "folder"
DEFINE FIELD scope_ids     ON passage TYPE array<string>; -- topic id, chat ids, workspace id: what may search it
DEFINE FIELD unit_id       ON passage TYPE string;        -- shared by a page's text and its picture
DEFINE FIELD modality      ON passage TYPE string;        -- "text" | "picture" | "frame" | "sound"
DEFINE FIELD ord           ON passage TYPE int;
DEFINE FIELD page          ON passage TYPE option<int>;
DEFINE FIELD start_line    ON passage TYPE option<int>;
DEFINE FIELD end_line      ON passage TYPE option<int>;
DEFINE FIELD start_ms      ON passage TYPE option<int>;
DEFINE FIELD end_ms        ON passage TYPE option<int>;
DEFINE FIELD section_path  ON passage TYPE string;
DEFINE FIELD text          ON passage TYPE string;
DEFINE FIELD chunk_hash    ON passage TYPE string;
DEFINE FIELD embedder      ON passage TYPE string;        -- model + prefix version
DEFINE FIELD vector        ON passage TYPE array<float>;

DEFINE INDEX passage_document ON passage FIELDS document_id;
DEFINE INDEX passage_scope    ON passage FIELDS scope_ids;   -- array field: used by CONTAINS / IN
DEFINE INDEX passage_hash     ON passage FIELDS document_id, chunk_hash UNIQUE;

DEFINE ANALYZER IF NOT EXISTS prose TOKENIZERS blank, class FILTERS lowercase, ascii, snowball(english);
DEFINE INDEX passage_text   ON passage FIELDS text FULLTEXT ANALYZER prose BM25 HIGHLIGHTS;
DEFINE INDEX passage_vector ON passage FIELDS vector HNSW DIMENSION 768 DIST COSINE TYPE F32 EFC 200 M 16;
```

Facts used (3.x docs): `FULLTEXT` replaced `SEARCH` in 3.0 and `SEARCH` is a parse error; one `FULLTEXT` index covers one field; `HNSW` holds its graph in memory with persistence (fine at Margin's scale: 100k passages × 768 × 4 B ≈ 300 MB vectors, graph on top; `DISKANN` exists if that ever hurts); the KNN operator is `<|K,EF|>` and `vector::distance::knn()` returns the distance the index computed; the matches operator `@1@` pairs with `search::score(1)` / `search::highlight`; `search::rrf(lists, limit, k)` and `search::linear(...)` fuse ranked lists. `kv-mem` runs the same statements, so tests exercise the real index.

The retrieval queries (`service/retrieval.rs`, new):

```sql
-- dense leg
LET $dense = SELECT id, unit_id, vector::distance::knn() AS d FROM passage
  WHERE vector <|$k_dense, 128|> $q AND $scope IN scope_ids AND modality IN $modalities;
-- lexical leg (text passages only; pictures have OCR text so they take part)
LET $lex = SELECT id, unit_id, search::score(1) AS s FROM passage
  WHERE text @1@ $q_text AND $scope IN scope_ids ORDER BY s DESC LIMIT $k_lex;
RETURN search::rrf([$dense, $lex], $k_fused, 60);
```

`k_dense = k_lex = 40`, `k_fused = 24`; the Rust side re-fetches the fused ids' rows (one `SELECT … WHERE id IN $ids`). Filters (`scope_ids`, `modalities`, `document_id IN`) are in the `WHERE`, so KNN is pre-filtered by the optimiser rather than post-filtered in Rust. Other tables stay as they are (`doc` nesting is harmless where nothing is indexed); `lesson`, `note` and `topic` additionally get a `FULLTEXT` index on a derived `search_text` field so ⌘K and `search_library` stop scanning (same analyzer).

**Migration**: at startup, if `passage` is empty and `chunk` has rows, copy rows (vector and text), build `scope_ids` from `topic_id`, mark `embedder = "gemma2-v0"`; old chunks with `title: none` prefix are re-embedded lazily per document on first retrieval miss or next ingest, so the owner's library is searchable the moment the new build starts. `chunk`/`media` are dropped after one successful migration (recorded in `settings`).

### 2.3 Retrieval pipeline (`service/retrieval.rs`)

```rust
pub struct Query { pub text: String, pub scope: Scope, pub modalities: Modalities, pub k: usize, pub budget_tokens: usize, pub images_max: usize, pub diversity: Diversity }
pub enum Scope { Sources(Vec<Id>), Topic(Id), Chat(Id), Workspace(Id), Library, Union(Vec<Scope>) }
pub struct Hit { pub passage: PassageRow, pub score: f32, pub dense_rank: Option<u32>, pub lex_rank: Option<u32>, pub rerank: Option<f32>, pub highlight: Option<String> }
pub trait Retriever: Send + Sync { fn search(&self, q: &Query) -> BoxFuture<'_, anyhow::Result<Vec<Hit>>>; }
```

Stages, each a small pure function with its own tests and its own eval metric:

1. **Query rewriting** (`QueryPlan`): a job supplies several query strings, not one. Lessons: the lesson title + summary; the summary alone; 2–3 "sub-questions" the *outline planner* wrote at proposal time (2.5); the learner's prompt only for lesson 1. Chat `search_sources`: the model's query plus, when the query is under five words, an expansion made by a cheap-model call (`route: Rewrite`, 2.6) only on cache miss, bounded to 120 output tokens; expansion is off by default for the agent tool and on for generation jobs (eval decides).
2. **Hybrid fetch**: dense + BM25 per query string, fused with RRF (2.2); union across query strings, max score kept.
3. **Reranking** (optional, behind `trait Reranker`): see 2.4.
4. **Packing**: `pack(hits, budget_tokens, images_max, diversity)`: drop near-duplicates by **unit** (page text vs page picture: keep the text, attach the picture only when `modality == picture` wins outright), merge adjacent chunks of one document into one passage when both are in the top list (removes the overlap problem), MMR-style diversity (λ 0.7) with a per-document cap (`ceil(k / 2)`), stop at the token budget (counted with the Gemma tokenizer, available on the service), pictures counted at their provider cost (≈ 1 100 tokens at 1536 px on OpenAI, `(w×h)/750` on Claude) against the same budget. Score threshold: hits under the 25th percentile of the top list's RRF score are dropped, so weak retrieval yields fewer passages instead of padding to k.
5. **Citations**: each packed passage carries `Citation { source_id | file, page, start_line/end_line, start_ms/end_ms, excerpt (≤ 2 000 chars, the chunk, not the merged passage), image_url }`; merged passages keep the first chunk's locator.

Caching: a `retrieval_cache` (in-memory LRU, 256 entries, keyed by `hash(query strings, scope, embedder, index generation)`) so a lesson regenerate or a repeated agent query is free; invalidated by the document's `index generation` counter bumped on ingest.

### 2.4 Reranking on the service Mac

Three options, in the order to try, each measured by the eval before it ships:

1. **No model, better fusion** (phase R1): RRF + MMR + unit dedup. Baseline.
2. **Listwise LLM rerank on the cheap route** (R1, generation jobs only): one call with the 24 fused candidates' first 300 chars each, asking for the ids in order with a 0–3 relevance grade; ~3k input tokens, ~100 output; cached prefix is the instruction. Used for lessons and the outline planner, never for `search_sources` (latency).
3. **Local cross-encoder** (R3, behind `Reranker`): a small cross-encoder converted to Core ML (candidates to verify when R3 starts: `mixedbread-ai/mxbai-rerank-base-v2`, `BAAI/bge-reranker-v2-m3`; both ONNX/Core ML-convertible, ~0.3–0.6 GB, 5–20 ms per pair on Apple silicon). Downloaded by the same Settings button as the embedder, same `ModelFiles` plumbing, same "Never bundled" rule. Shipped only if the eval shows ≥ +3 points nDCG@10 over option 2 at lower latency.

EmbeddingGemma 2 itself is not a reranker; the Matryoshka truncation it supports (256-d) is an option for a cheaper first pass (`DIMENSION 256` index for the ANN leg, exact 768-d cosine for the top-200) if HNSW memory ever matters.

### 2.5 How generation jobs consume retrieval

- **Outline planner** (new `plan_course` step inside `propose_course`, service side, not the agent): when `source_ids`/workspaces are given, the service runs a **survey**: one dense query per *document* using its first section's text, plus the learner's prompt, packed to 6k tokens of passages + the documents' section paths (a table of contents costs almost nothing and tells the model what the sources cover). The planner call (`route: Plan`, strong model) returns the outline **and, per lesson, 2–3 retrieval questions and the source sections it expects to use**. Stored on the proposal; the lesson jobs use them (2.3 step 1). The agent still calls `propose_course`; the tool now takes an optional `survey: true` (default when sources or folders are attached) and returns the planned outline for the card.
- **Lesson**: query plan from the proposal; budget `12k` input tokens for passages (≈ 24 chunks) and `images_max` 2 by default, 4 when the plan marked the lesson "figure-heavy"; pictures sent at 1024 px unless the plan asked for detail. Instructions reordered for caching (2.6): constant part first, variable part (level, minutes, quiz) last, passages last of all.
- **Video scene**: `MANIMGX_CHEAT_SHEET` + rules as the cached prefix; the lesson body trimmed to its sections minus "Check yourself"; the repair call reuses the same prefix.
- **Condense, title, rewrite, rerank**: cheap route.
- **Regenerate**: hits the retrieval cache unless the index generation changed; the stored proposal's query plan is reused (consistent with phase-4 Q1's "stored excerpts" rule, which becomes "stored *plan* + current index" for sources and stays "stored passages" for folder files that may be offline).

### 2.6 Prompt caching and model routing (`crates/ai`, providers' `model.rs`)

**Caching rules** (both providers):
- OpenAI: automatic prefix caching needs a ≥ 1 024-token identical prefix; `prompt_cache_key` groups requests. Every `TextModel` call gets a `cache_key`: lessons `lesson:{prompt_version}`, scenes `scene:{prompt_version}`, titles/condense `chat:{id}` as today.
- Claude: explicit `cache_control: ephemeral` breakpoints (the providers brief already places them on system, last tool, last message). Add one after the constant instruction block of lesson and scene calls. Minimum cacheable prefix is model-dependent (1 024–4 096 tokens); the lesson instructions plus `LESSON_SOURCES` is ~1.3k tokens, the scene cheat sheet ~4k, so both clear it.
- Prompt layout invariant, enforced by a test: `instructions = CONSTANT ++ variable`, never interleaved; `prompt_version` bumps when `CONSTANT` changes and is stored on every lesson/proposal (so the eval can compare versions).

**Routing table** (`ai::Route`, resolved by `AppState::model_for(route)` from Settings' provider config; shown in Settings → AI as "Model for chats / for writing / for small tasks", all three functional):

| Route | Used by | Default OpenAI | Default Claude |
|---|---|---|---|
| `Chat` | agent steps | the chat model (Settings) | `claude-opus-5-5` |
| `Write` | lessons, scenes, outline planner | the chat model | `claude-sonnet-5-5` |
| `Small` | titles, condense, query rewrite, listwise rerank, synthetic eval Q&A | `gpt-6-luna` | `claude-haiku-5-5` |

Providers Q3 ("cheapest for titles/condense") is answered by this table; the owner may still point `Small` at any model.

### 2.7 Incremental re-indexing for folders (phase 4, reworked)

- The device walks the folder with the allow-list and sends **manifest entries** `{ path, bytes, modified, sha256 }` for visible files (the hash computed on the device, streaming; the device already streams reads). The service diffs the manifest against `document` rows for that workspace and asks the device to **upload only changed or new files** (`PUT /api/workspaces/{id}/uploads/{document_id}`; the phase-4 upload route and pending-call gate stay). Uploaded bytes go through `ingest` like a source (`Origin::FolderFile`), with `scope_ids = [workspace_id] + chats that attached it`. Deleted files → passages deleted. Re-walk on attach, on "Refresh" in the Folders pane, when a chat with the workspace starts a turn and the last walk is older than 10 min, and on the device's file-watcher where one exists (macOS `FSEvents`; mobile re-walks on foreground).
- **No per-file size cap and no picture cap** (owner): uploads stream to disk in parts as sources do (`routes::max_source_bytes` no longer applies to folder files; the 100 MB Cloudflare body limit is handled by the parts upload that already exists for sources). A 2 000-page PDF is ingested page by page with progress; the agent never reads it whole.
- The folder tools change meaning: `search_text` becomes hybrid retrieval scoped to the workspace (`Scope::Workspace`), `read_pdf` reads stored page text by page range from `passage` rows (no device round trip), `view_image` serves the stored picture; `list_files`/`find_files`/`read_file` stay device tools for the live view (a file that is on the manifest but not yet ingested is reported as "being read"). The byte cache and `FolderReader` of D7a/D7b are not built.
- Folder citations use `Citation.file` as the phase-4 first commit defines it; the viewer loads the live file from the device when it is online and the stored passage otherwise (D8 unchanged).

### 2.8 The agent's retrieval tool contract

Replace `search_sources` with a family that fits "retrieve chunks, never whole files":

| Tool | Args | Returns |
|---|---|---|
| `search` | `query`, `scope: {topic_id?, source_ids?, workspace_ids?, chat_files?, library?}`, `k ≤ 12`, `modalities` | Hits with `n`, locator, `section_path`, text (≤ 1 200 chars each), `score`; a Passages card. Default scope: this chat's files and folders, then its topic, **not** every source (fixes the cross-topic leak at `agent_tools.rs:777-783`); `library: true` adds lessons and notes as a third leg. |
| `read_passage` | `document_id`, `page?` or `ord` range | Up to 4 adjacent passages (≤ 6k chars) around a hit: "show me more of that". |
| `document_outline` | `document_id` | Section paths with page/line ranges (from `passage.section_path`), ≤ 2k chars. |
| `view_picture` | `document_id`, `page?` | The stored picture in the tool output (phase-4's `ToolOutput.images`), counted against the per-turn picture budget. |

Rules: outputs carry `n` numbers the agent cites in answers (`[^n]` → `AssistantText.citations`, the shared field the providers brief adds for web citations, extended with `source`/`file` variants); each output ≤ 16 KB; the Passages card shows what the model saw. `search_library` keeps its name but runs the FULLTEXT index.

### 2.9 Cost and latency budgets (targets; the eval runner reports actuals)

| Thing | Target | Measured by |
|---|---|---|
| Ingest, text | ≥ 8 chunks/s end to end on an M-series Mac; unchanged file re-ingest ≤ 1 s | `ingest` spans |
| Retrieval | p50 ≤ 150 ms, p95 ≤ 400 ms for 100k passages, both legs + fusion (no rerank) | `retrieval` spans |
| Lesson input | ≤ 14k tokens with ≤ 2 pictures by default; cached prefix ≥ 1k tokens | `Usage` |
| Chat step | cached share of input ≥ 60 % from step 2 on | `Usage.cached_tokens / input_tokens` |
| Side calls | title ≤ 300 tokens total; condense summary ≤ 2k output | `Usage` by route |
| 10-lesson course, no videos | ≤ $0.60 on the default OpenAI route, ≤ $1.20 on Claude Sonnet, both excluding the planner (≈ $0.05) | cost model in the eval runner (prices in `evals/prices.toml`) |

### 2.10 What changes in which crate

| Crate | Changes |
|---|---|
| `margin-embed` | Section-aware chunker with `section_path`/lines, title prefix, `chunk_hash`, image OCR text into the chunk, lazy page render API (`pdf_page_picture` as the phase-4 brief defines it), optional `Reranker` trait + Core ML impl (R3), `RecordedEmbedder` for evals. |
| `margin-shared` | `Citation` locators (`start_line/end_line`, `file`), `AssistantText.citations` variants, `Route`, Settings fields for the three model slots, `PassagesCard` with `document_id`/locators, `Event::DocumentChanged`. |
| `margin-service` | `pipeline.rs` (stage traits, registry, config → pipeline, 2.0); `db.rs` schema + indexes + migration; `ingest.rs` (replaces the embedding half of `sources.rs`); `retrieval.rs`; `ask.rs` planner + budgets; `agent_tools.rs` new tool family; `search.rs` on the index; `workspaces.rs` manifest/refresh; `turns.rs` routes; tracing spans with usage per route. |
| `margin-ai` | `TextModel` call takes `CallOptions { route, cache_key, max_output_tokens }`; prompt layout constant-first; `prompt_version`; listwise rerank prompt; synthetic Q&A prompt (eval). |
| `margin-agent` | Tool budget per picture; `search` family tool types with the service's `Retriever` behind a trait (`NoRetriever` in the crate's tests). |
| `margin-app` | Settings → AI three model slots; Passages card locators; "Refresh" in the Folders pane; the citation viewer's line ranges. |
| `margin-evals` (new) | Section 3. |

---

## 3. Eval suite

Principle: **CI never calls a model API or needs the Core ML model**; it runs on fakes and *recorded* fixtures and gates regressions. A separate `margin-eval` binary, run by hand by the owner, calls real models and the real embedder and writes results the owner can compare. Both share one crate, `crates/evals`.

### 3.1 Datasets (checked in under `evals/`)

- `evals/corpus/`: a fixture library of public-domain or self-written material, sized for CI (≤ 5 MB): six prose documents (Markdown with headings, a reST file, an HTML page), three small PDFs (one with a text layer, one scanned page rendered from text so OCR has something to find, one mixed), four images (two diagrams with legible labels, two photos), one short audio clip with a known transcript (for the existing path), and a `folder/` tree with the allow-list mix (`.md`, `.pdf`, `.png`, plus `.rs`/`.json`/`.zip` that must stay invisible).
- `evals/golden/retrieval.jsonl`: ~150 queries, each `{ id, query, scope, relevant: [{document, page?, anchor: "exact text that must be in the hit"}], kind: "lexical" | "semantic" | "cross-modal" | "negative" }`. Written by hand for the first 60 (the anchors make grading deterministic: a hit is relevant when its text contains the anchor or its unit is the anchored picture), then **synthetic generation offline**: the eval runner asks the `Small` route to write 3 questions per passage with the answer span quoted; the owner reviews a sample; accepted rows are committed with `generated_by: model id`. Negatives are queries with no answer in the corpus (the metric is "returned nothing above threshold").
- `evals/golden/courses.jsonl`: 8 course requests over the corpus (prompt, sources, expected outline themes, per-lesson "must-cite" documents).
- `evals/recorded/embeddings.bin`: the real EmbeddingGemma vectors of every corpus chunk and golden query, recorded once by `margin-eval record-embeddings` on the owner's Mac (f16, ~2 MB for ~1 200 vectors), keyed by `embedder_version + sha256(prefix + text)`. `RecordedEmbedder` serves them in CI and fails loudly on an unknown text, so a chunker change that produces new chunk texts requires a re-record (a deliberate, visible step). This is what makes recall@k in CI *real* rather than bag-of-words.
- `evals/recorded/generations/`: recorded model outputs (lessons, outlines, titles) for the golden courses, per `prompt_version` and model, produced by the opt-in runner; `RecordedGenerator` replays them in CI for citation and packing checks.

### 3.2 Metrics

Retrieval (per query kind and overall): **recall@5, recall@10, MRR, nDCG@10**, picture-hit rate for cross-modal queries, false-positive rate on negatives, dedup rate (share of top-k that are overlapping neighbours), per-stage latency. Computed by `evals::metrics` (pure functions, unit-tested against hand-worked examples).

Generation:
- **Citation accuracy** (CI, deterministic): every `[^n]` resolves; the cited passage shares ≥ 2 content terms with the citing sentence (a lexical support proxy); no passage is cited that the packer didn't send; share of source-derived sentences with a marker (checked on recorded outputs against the planner's "must-cite" list).
- **Faithfulness and answer relevance** (opt-in runner only): LLM-as-judge on the `Small` route with a fixed rubric (`evals/rubrics/*.md`: faithfulness 0–3 per claim against the sent passages, relevance 0–3 against the lesson summary, outline coverage of the must-cite documents, "uses the source's terminology"), judged twice with swapped candidate order to cancel position bias, reported with the judge model id.
- **Structure checks** (CI): the lesson rules the prompt states (no `#` title, one Key idea, a worked example, the quiz block when asked) as a cheap proxy for prompt regressions.

Efficiency: input/output/cached tokens per call by route, cache-hit share, cost from `evals/prices.toml`, wall-clock per stage, ingest throughput, index size. All from `Usage` and tracing spans, so the same numbers appear in the chat footer and the eval report.

### 3.3 Regression gates in CI (`cargo test -p margin-evals`)

Fakes and recorded fixtures only. Gates read `evals/baseline.json` (committed; updated on purpose with the PR that improves it) and `evals/gates.toml`:

- Retrieval on `RecordedEmbedder` + in-memory SurrealDB with the real indexes: recall@5 and nDCG@10 may not drop by more than 1.0 point overall or 3.0 on any query kind; negatives' false-positive rate ≤ baseline + 2 points; p95 retrieval latency ≤ 2× baseline (loose, CI machines vary).
- Ingest invariants: every corpus sentence appears in exactly one chunk's non-overlap region; `chunk_hash` stability (re-ingest of an unchanged file embeds 0 chunks); the folder manifest diff uploads exactly the changed files; the invisible types never produce passages.
- Packing: token budget never exceeded (counted with the Gemma tokenizer fixture), pictures ≤ `images_max`, no two passages from one unit, per-document cap holds.
- Citations on recorded generations: 100 % resolution, lexical support ≥ baseline − 2 points.
- Prompt caching invariant: the constant prefix of each `prompt_version` is byte-identical across the golden courses' calls (the fake Responses/Messages servers record request bodies; the test compares prefixes and asserts `prompt_cache_key`/`cache_control` placement).
- Agent: a scripted turn's retrieval tool outputs are ≤ 16 KB and the Passages card matches the tool output.

### 3.4 The opt-in runner (`margin-eval`, never in CI)

`margin-eval run --suite retrieval|courses|agent --variant <name>` uses the owner's real keys from the service Keychain (reads the config, never touches the running service's database: it opens its own `data_dir/evals/`), the real Core ML embedder and the real indexes. Sub-commands: `record-embeddings`, `record-generations`, `synth-questions`, `judge`, `compare A B`, `report`.

**Variants** (`evals/variants/*.toml`): `chunker = "sections-v2" | "pages-v1"`, `embed_prefix = "title" | "none"`, `retriever = "dense" | "bm25" | "hybrid-rrf" | "hybrid-linear"`, `reranker = "none" | "listwise-small" | "coreml-crossencoder"`, `k_*`, `pack = {...}`, `prompt_version`, `route overrides`. A run is keyed by `sha256(variant toml + corpus hash + embedder version)`.

**Storage**: results as JSONL rows (`run_id, variant, suite, query_id, metric, value, cost, latency, model, judge`) in `data_dir/evals/results.jsonl` plus a SurrealDB `eval_result` table in the evals database for queries; `compare` prints per-metric deltas with a paired bootstrap 95 % interval (the question is "did the variant help", so paired per query).

**Report/dashboard**: `margin-eval report` writes `evals/reports/<run>.md` (tables) and a self-contained HTML page (one file, charts inline) that the owner opens locally; the latest report's summary table is committed with the PR that changes the pipeline. A Settings → Lab pane in the app that lists runs and opens the report is possible and fully functional (it would read `GET /api/evals/runs`), but it is product surface, so it is an owner decision (Q7), not assumed.

### 3.5 A/B of chunkers, retrievers, prompts, models

Same corpus, same golden set, two variants, `compare`. For prompts and models the comparison runs the recorded-generation step for both and the judge once over the union (blind to variant). For chunkers, a re-record of embeddings is required and `compare` refuses to run across different embedder versions unless `--allow-embedder-change` is passed, so a "win" is never a side effect of fresh vectors.

---

## 4. Build plan and sequencing

Follows the existing workflow: a shared first commit, parallel areas with exclusive file ownership, integration + e2e, two reviewers, a fixer. The Fable architect reviews each brief first (memory rule). The RAG work is two phases, placed around phase 4, providers and phase 5 as follows:

**R1 — Retrieval core and evals** (before phase 4's service area):
- First commit: `margin-shared` types (2.10) incl. `PipelineConfig`, `db.rs` schema + indexes + migration stub, `pipeline.rs` with the stage traits, `Registry` and `Pipeline::build` (2.0; fakes registered under the real names), `evals/` layout with the corpus and the first 60 golden queries, `crates/evals` skeleton, contract updates. Areas implement stages against these traits and never change them; a needed change goes through the orchestrator.
- Areas: **R1-embed** (`crates/embed`: chunker v2, prefix, hashes, image OCR text, `RecordedEmbedder`); **R1-store** (`service/db.rs`, `ingest.rs`, migration, batched writes, lazy page renders); **R1-retrieve** (`service/retrieval.rs`: hybrid + fusion + packing + cache, `search.rs` on the index, the agent tool family in `agent_tools.rs`, `ask.rs` planner and budgets); **R1-evals** (`crates/evals`: metrics, gates, runner, synthetic-question and judge prompts in `crates/ai/src/prompts.rs` under an `evals` module the ai owner owns); **R1-app** (Passages card locators, Settings' three model slots as plain OpenAI settings for now, `search` tool rows).
- E2E: a golden course on the fixture corpus through the real client and fake model servers; recall gates green on recorded embeddings; `⌘K` and `search_library` on the index.
- Definition of done includes the owner running `margin-eval record-embeddings` and `run --suite retrieval` once by hand and the baseline being committed.

**Phase 4 (folders)**, with these changes to its brief: 4-agent's `read_pdf`/`view_image` and the `FolderReader` trait wait for R1 and are rebuilt on `Retriever` (2.7, 2.8); 4-S's folder byte cache (D7b), `FolderReader` (D7a) and `folder_passages` rows (D7c) are replaced by manifest + ingest; `FOLDER_FETCH_MAX_BYTES`, `PDF_PAGES_PER_READ`, `FOLDER_PICTURES_MAX`, `VIEW_IMAGES_PER_TURN` go (no caps by owner decision; a per-turn picture *token* budget stays). **4-core, 4a, 4b, 4c go ahead unchanged**: handle store, `Dir`, pickers, the device transport, uploads and the Folders pane are transport and UI and don't depend on how the service indexes what it receives; the only addition is the manifest walk in `folders/tools.rs` (4-core) and "Refresh" in the pane. The SIWC spike runs in parallel as planned.

**Providers**, as briefed, plus the `Route`/`CallOptions` seam and the Claude cache breakpoints on lesson/scene prefixes (2.6); `TurnDeps.summary_model` becomes `model_for(Small)`.

**R2 — Rerank, rewrite, lab** (after providers, before or inside phase 5): listwise rerank on `Small`, query rewriting, the Core ML cross-encoder behind `Reranker` if the eval earns it, `compare`/`report` polish, the optional Lab pane (Q7), prompt-version tracking of both providers' prompts in the eval.

**Phase 5** then hardens all of it (compaction per provider, prompt-injection tests over retrieval outputs, iOS continued processing for folder walks and uploads).

Why R1 before phase 4's service area and not after: the folder decision ("ingested, embedded, searched; no caps") is a retrieval decision, and building D7's byte cache first would be throwaway work plus a migration; the app-side areas are unaffected, so phase 4 loses no calendar time.

---

## 5. Owner decisions needed (one line each, with a recommendation)

1. **Default `search` scope for the agent**: this chat's files/folders then its topic (recommended), or every source as today?
2. **Three model slots in Settings** (chats / writing / small tasks) with the defaults in 2.6: yes (recommended), or one model per provider as now?
3. **Lesson passage budget**: 14k input tokens and 2 pictures by default (recommended; the planner may raise pictures to 4), or keep 8 passages / 4 pictures fixed?
4. **Outline planner as a service step** that surveys sources before the outline (one extra strong-model call per course, ≈ $0.05): yes (recommended), or leave outlines entirely to the agent's own searches?
5. **Folder manifest hashing on the device** (sha256 of visible files on attach/refresh; iCloud files download once): yes (recommended), or stamp-only (`bytes + modified`, cheaper, misses in-place edits that keep size and time)?
6. **Local cross-encoder download** (a second ~0.5 GB model behind the same Settings button) if the eval shows ≥ +3 nDCG points: yes (recommended), or stay with LLM listwise rerank only?
7. **A Settings → Lab pane** listing eval runs and opening reports in the app: no for now, CLI report only (recommended; it is product surface the lab doesn't need yet), or yes?
8. **Synthetic golden questions**: generate with the `Small` route and commit after your review of a 10 % sample (recommended), or hand-write all of them?
9. **Office/e-book formats** (`.docx .pptx .epub`) on the allow-list once an extractor exists: not now (recommended), or add in R1?
10. **Pipeline config in Settings** (a variant picker and the budgets under Settings → AI → Pipeline, all functional): yes (recommended, so a pivot is a setting, not a release), or config file on the service Mac only?
11. **Migration**: re-embed the existing library lazily with the new prefix (recommended; search works at once, improves as documents are touched) or in one blocking pass at first start?
