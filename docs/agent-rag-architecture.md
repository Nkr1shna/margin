# Agent + RAG pipeline: review, target architecture, evals, build plan

Written 2026-10-14 at `fd1e848` on `claude/agent-build`, on the owner's request: "re-architect the agent + RAG pipeline for making the generation efficient … for best practices with evals to track. We are a real lab optimizing its product." Read: `docs/HANDOFF.md`, `PLAN.md`, `docs/agent-architecture.md`, `docs/agent-contract.md`, `docs/agent-providers-brief.md`, `docs/agent-phase4-brief.md`; `crates/embed/src/{lib,chunk,rank,pdf,extract,image,vision,coreml,fake}.rs`; `crates/service/src/{db,sources,search,embedding,ask,agent_tools,turns,engines,video}.rs`; `crates/agent/src/{lib,turn,tool,prompt,replay}.rs`; `crates/ai/src/{lib,prompts,citations}.rs`; `Cargo.lock` (SurrealDB **3.3.4**, `kv-surrealkv` + `kv-mem`, no other features). SurrealDB facts were checked against the 3.x docs (`/docs/reference/query-language/statements/define/indexes`, `/docs/learn/data-models/vector-search/hybrid-search`, `/docs/reference/query-language/functions/database-functions/search`). No code changed; no other doc touched.

Owner decisions this design respects: folders use a file-type allow-list (prose documents, PDFs, images), filtered on the device before upload; **folder files are ingested into SurrealDB as they upload, embedded with EmbeddingGemma and searched by vector search, including inside PDFs; no file-size or picture caps, because the agent retrieves chunks rather than reading whole files**; code arrives later through a repository source; providers switch per chat (OpenAI key, ChatGPT sign-in, Claude API); keys never on devices; tests use fakes only; no placeholder UI; scope is never narrowed without asking.

**Owner answers (2026-10-14, second round)** to §5's first list, now binding: Q1 default agent scope chat + topic; Q2 three model slots; Q3 **no fixed token budget**: the context budget scales with the model's window and with what retrieval finds, budgets are config with generous defaults, measured by evals (§2.5, §2.9); Q4 outline planner; Q5 device-side hashing; Q6 cross-encoder when the eval earns it; Q7 **no eval UI in the app; evals live outside** (§3); Q8 **no human review of golden questions**: quality is shown through eval results; eval questions must be serious; Q9 office formats not now; Q10 pipeline config in Settings; Q11 **re-embed the whole library in one pass at first start** (data is small). Widened vision: Margin becomes a **Live context app for a company's projects**, an interface for humans (GUI) and agents (context) (§2.12); the service records **traces** of every model call, retrieval, tool call and job into SurrealDB time series, and evals/reports read them from outside (§2.11, §3); lesson **quizzes** must be serious and partly graded, with a code editor for coding tasks (§2.13). Then §6 is the R1 brief, ready to build.

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

**Embedding prefix**: `title: {source name or section path} | text: {chunk}` instead of `title: none` (`embed/lib.rs:76-82`). Recorded as `embedder_version`; rows with another version are re-embedded (one pass at migration, 2.2; per document on later changes, 2.7).

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

**Checked against the embedded engine (R1 first commit, `db.rs` tests on `kv-mem`, SurrealDB 3.3.4).** As written and working: `DEFINE ANALYZER … TOKENIZERS blank, class FILTERS lowercase, ascii, snowball(english)`, `FULLTEXT ANALYZER … BM25 HIGHLIGHTS`, `HNSW DIMENSION … DIST COSINE TYPE F32 EFC … M …`, the `<|K,EF|>` operator with `vector::distance::knn()` (extra `WHERE` conditions become the `KnnScan`'s pre-filter predicate in `EXPLAIN`), `@1@` with `search::score(1)`/`search::highlight`, `search::rrf(lists, limit, k)` (rows come back with `id` as a record and an `rrf_score`; the legs' other fields are merged in), array record ids with a datetime inside, record ranges with bound parameters (`trace:[$k, $from, '']..=[$k, $to, '~']`, a `RecordIdScan`), `DELETE` over such a range, `DEFINE TABLE … TYPE NORMAL AS SELECT … GROUP BY` views with `time::floor`, `count()`, `math::sum`, `math::mean` that follow inserts and range deletes, and `option<object> FLEXIBLE` fields. Deviations, now in the code:
- The array index is `FIELDS scope_ids.*` (and `access.*`): with `FIELDS scope_ids` the planner did a table scan for `$x IN scope_ids`/`CONTAINS`; with `.*` it uses an `IndexScan` (and a union of scans for `CONTAINSANY`).
- The view aggregates read `math::sum(usage.input ?? 0)` and `math::sum(cost_usd ?? 0)`: without the `?? 0`, writing a trace row that has no usage or cost (a retrieval, a tool call) fails inside the view and the row is refused. The views name their time column `bucket` (not `hour`) so the hourly and daily views share `context::TraceRollup`.
- `OVERWRITE` goes before the name for tables (`DEFINE TABLE OVERWRITE trace_hourly …`). The analyzer, the FULLTEXT/HNSW indexes and the views are applied with `OVERWRITE` only when their definition text changes (a fingerprint in `schema_meta:indexes`), not at every start, so an unchanged HNSW index is never rebuilt.
- JSON `null` is SurrealQL `NULL`, which `option<…>` fields refuse; rows leave `None` fields out when written (`skip_serializing_if`), so they are `NONE`.
- `$access` is a protected parameter name; queries bind the caller's access paths as `$paths`.
- `type::record(…)..=type::record(…)` is not a range source; the range is written as a literal id range with bound parameters.
- `meta::id` is `record::id` in 3.x.
- BM25 gives a score of 0 to a term that occurs in more than about half of the passages (its IDF bottoms out); on a real corpus this is the stop-word behaviour wanted, but tiny test tables must use rare words.

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

**Migration** (owner Q11: one pass, now): at first start of the new build, every `Ready` source is re-queued through `ingest` (extract, chunk v2, embed with the title prefix) in one pass, oldest first, with the usual progress events; `chunk`/`media` are dropped once every source has passages (recorded in `settings.migrations`). Retrieval during the pass falls back to the old rows for sources not yet migrated. Sources whose file is gone are marked `Failed` with "Read again".

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
- **Lesson**: query plan from the proposal. **No fixed small cap** (owner): the passage budget is `Budget { share_of_window: 0.5, reserve_output: model.max_output, min_score_pct: 25, images_max: 8 }` from `PipelineConfig`, resolved against the route's model window (`ModelInfo.context_window`, 200k–1M on current models), so a lesson gets as much relevant material as retrieval finds above the score threshold, up to half the window; what actually goes in is decided by relevance and the packer's dedup, and the eval reports tokens, cost and faithfulness per budget setting so the owner can tune it. Pictures go at full `WRITER_IMAGE_SIDE` unless the config says otherwise. Instructions reordered for caching (2.6): constant part first, variable part (level, minutes, quiz) last, passages last of all.
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
| `search` | `query`, `scope: {topic_id?, source_ids?, workspace_ids?, chat_files?, library?}`, `k ≤ 12`, `modalities` | Hits with `n`, locator, `section_path`, text (≤ 1 100 chars each, `TOOL_OUTPUT_PASSAGE_CHARS`, so twelve with locators fit 16 KB), `score`; a Passages card. Default scope: this chat's files and folders, then its topic, **not** every source (fixes the cross-topic leak at `agent_tools.rs:777-783`); `library: true` adds lessons and notes as a third leg. |
| `read_passage` | `document_id`, `page?` or `ord` range | Up to 4 adjacent passages (≤ 6k chars) around a hit: "show me more of that". |
| `document_outline` | `document_id` | Section paths with page/line ranges (from `passage.section_path`), ≤ 2k chars. |
| `view_picture` | `document_id`, `page?` | The stored picture in the tool output (phase-4's `ToolOutput.images`), counted against the per-turn picture budget. |

Rules: outputs carry `n` numbers the agent cites in answers (`[^n]` → `AssistantText.citations`, the shared field the providers brief adds for web citations, extended with `source`/`file` variants); each output ≤ 16 KB; the Passages card shows what the model saw. `search_library` keeps its name but runs the FULLTEXT index.

### 2.9 Cost and latency budgets (targets; the eval runner reports actuals)

| Thing | Target | Measured by |
|---|---|---|
| Ingest, text | ≥ 8 chunks/s end to end on an M-series Mac; unchanged file re-ingest ≤ 1 s | `ingest` spans |
| Retrieval | p50 ≤ 150 ms, p95 ≤ 400 ms for 100k passages, both legs + fusion (no rerank) | `retrieval` spans |
| Lesson input | no hard cap (owner); report tokens and cost per lesson by budget setting; cached prefix ≥ 1k tokens | `Usage`, traces |
| Chat step | cached share of input ≥ 60 % from step 2 on | `Usage.cached_tokens / input_tokens` |
| Side calls | title ≤ 300 tokens total; condense summary ≤ 2k output | `Usage` by route |
| 10-lesson course, no videos | reported, not capped; the eval compares cost per quality point across budget settings | cost model in the eval runner (prices in `evals/prices.toml`) over traces |

### 2.10 What changes in which crate

| Crate | Changes |
|---|---|
| `margin-embed` | Section-aware chunker with `section_path`/lines, title prefix, `chunk_hash`, image OCR text into the chunk, lazy page render API (`pdf_page_picture` as the phase-4 brief defines it), optional `Reranker` trait + Core ML impl (R3), `RecordedEmbedder` for evals. |
| `margin-shared` | `Citation` locators (`start_line/end_line`, `file`), `AssistantText.citations` variants, `Route`, Settings fields for the three model slots, `PassagesCard` with `document_id`/locators, `Event::DocumentChanged`. |
| `margin-service` | `pipeline.rs` (stage traits, registry, config → pipeline, 2.0); `trace.rs` (2.11); `context_api.rs` (2.12); `db.rs` schema + indexes + migration; `ingest.rs` (replaces the embedding half of `sources.rs`); `retrieval.rs`; `ask.rs` planner + budgets; `agent_tools.rs` new tool family; `search.rs` on the index; `workspaces.rs` manifest/refresh; `turns.rs` routes; tracing spans with usage per route. |
| `margin-ai` | `TextModel` call takes `CallOptions { route, cache_key, max_output_tokens }`; prompt layout constant-first; `prompt_version`; listwise rerank prompt; synthetic Q&A prompt (eval). |
| `margin-agent` | Tool budget per picture; `search` family tool types with the service's `Retriever` behind a trait (`NoRetriever` in the crate's tests). |
| `margin-app` | Settings → AI three model slots; Passages card locators; "Refresh" in the Folders pane; the citation viewer's line ranges. |
| `margin-evals` (new) | Section 3. |

### 2.11 Tracing in SurrealDB time series (`service/trace.rs`, R1)

The service records a trace row for every model call, retrieval, tool call and job step. Evals, reports and cost accounting read these from outside the app (§3). SurrealDB's time-series modelling (docs: `/docs/learn/data-models/time-series/overview`, `…/using-as-time-series-database`, `/docs/learn/querying/real-time/real-time-best-practices#temporal-querying-and-record-ranges`) is used as the docs describe it:

- **Array record ids with the time inside**: `trace:[<kind>, <datetime>, <ulid>]`, e.g. `trace:['model_call', d'2026-10-14T10:03:21.412Z', 'u01J…']`. Rows for one kind and time window are a **record range query** (`SELECT * FROM trace:['model_call', $from, '']..=['model_call', $to, '~']`), a partition scan over just that slice, no index needed. The ulid keeps ids unique when two calls share a millisecond.
- **Row shape** (`SCHEMAFULL`, additive fields): `kind` (`model_call | retrieval | tool_call | ingest | job_step | turn`), `at`, `duration_ms`, `ok`, `error_code?`; correlation `trace_id` (one per turn or job), `parent_id`, `chat_id?`, `turn_id?`, `job_id?` (lesson/video/proposal id), `document_id?`; config `pipeline_hash`, `prompt_version`, `route`, `provider`, `model`; for model calls `usage { input, cached, cache_write, output, reasoning, web_searches }`, `cost_usd` (priced at write time from `prices.toml` so a later price change doesn't rewrite history), `stop_reason`, `step`; for retrieval `query_hash`, `scope`, `legs { dense_ms, lex_ms, fuse_ms, rerank_ms }`, `hit_ids: array<string>` with `scores`, `packed_ids`, `packed_tokens`, `images`; for tool calls `tool`, `output_bytes`, `device_id?`; for ingest `chunks_new/kept/deleted`, `embed_ms`. Prompts and outputs are **not** stored in traces (they are in `chat_item`, `lesson`, `proposal`); traces hold ids, numbers and hashes, so a trace never leaks content or keys.
- **Rollups as pre-computed table views**: `DEFINE TABLE trace_hourly TYPE NORMAL AS SELECT kind, route, provider, model, time::floor(at, 1h) AS hour, count() AS calls, math::sum(usage.input) AS input, math::sum(usage.cached) AS cached, math::sum(usage.output) AS output, math::sum(cost_usd) AS cost, math::mean(duration_ms) AS mean_ms FROM trace GROUP BY kind, route, provider, model, hour;` and a `trace_daily` view the same way with `1d`. SurrealDB keeps views current as rows are written, so "cost today by model" and "cache-hit share this week" are one `SELECT` on the view; the chat footer's usage keeps coming from `Chat.usage` as today.
- **Retention**: raw rows are kept 90 days (`settings.trace_retention_days`, functional in Settings → Advanced); a daily job deletes by range (`DELETE trace:['model_call', d'1970-01-01', '']..=['model_call', $cutoff, '~']` per kind), which is cheap because the range is the id prefix. Hourly rollups are kept a year, daily rollups forever; both are tiny. An eval run that wants raw rows older than that must have exported them (`margin-eval export`).
- **Export and reading from outside**: `GET /api/traces?kind&from&to&trace_id` (device token; JSONL stream) and `GET /api/traces/rollup?bucket=hour|day&from&to` serve the runner and any external tool; `margin-eval` reads over HTTP from a running service or opens an exported JSONL. Live tails use the existing events socket: `Event::Trace { row }` is emitted for `model_call` and `retrieval` rows when a client subscribed with `?traces=1` (nothing is pushed by default).
- **Why not a separate database**: one embedded SurrealDB file, one backup, one query language, and the range-id pattern gives time-series performance without a second engine; if write volume ever matters, traces move to their own namespace in the same instance.

### 2.12 The Live-context vision and what R1 does now

Vision (owner): Margin becomes a **Live context app for a company's projects**, an interface for humans (the GUI) and agents (context). Not designed here in full; this is what it implies and what R1 must get right so it isn't redone.

- **Project-scoped corpora.** `scope_ids` on every passage (2.2) already makes scope a first-class filter. R1 adds a `project` record (`id, name, created_at`; today exactly one, "My library", created at migration) and puts `project_id` on `document`, `passage`, `topic`, `chat`, `workspace`, `trace`. Every retrieval `Scope` resolves inside one project. Multi-project UI is not built in R1 (the picker would be one more sidebar section when the owner asks).
- **A context API for agents.** Humans get the GUI; agents get the same `Retriever`, `document_outline` and `read_passage` over HTTP, MCP-shaped: `POST /api/context/search` (`Query` JSON → hits with locators and citations), `GET /api/context/documents/{id}/outline`, `GET /api/context/documents/{id}/passages?from&to`, `POST /api/context/ingest` (a document with an origin "agent:<name>"), all behind the device token today and a **per-agent token** with a project scope and read/ingest rights later. The MCP server shape (`tools: search_context, read_passage, document_outline, ingest_document`) is a thin adapter over these four routes, planned as its own item after providers (it needs the auth model). R1 lands the four routes with the same handlers the agent tools use, so there is one retrieval code path for Margin's own agent, external agents and the GUI.
- **"Live"**: freshness is the index generation per document (2.3) plus `Event::DocumentChanged`; an agent polling `GET /api/context/changes?since=<generation>` gets the ids that changed. Folder manifests (2.7) and, later, repository sources are the feeds; chat transcripts, notes and lessons are indexed too (`origin: library`), so what humans write in the GUI is context for agents the moment it is saved.
- **Multi-user and auth later**: today a single GitHub login; R1 keeps `author`/`device_id` on every write and adds `actor` (`owner | agent:<name>`) to `chat_item`, `note`, `document` and `trace` so attribution exists before accounts do. No permissions model in R1.
- **Access control (owner, later round: knowledge is access-controlled and hierarchical, company-wide, then department, role and team; executives and engineers see different things).** R1 lands only the seam, no enforcement: `Document.access: Vec<String>` of hierarchical paths ("company", "company/engineering", default `["company"]`), the same `access` array on every `passage` row (copied from its document at ingest, indexed `access.*`), and an optional caller filter `ContextQuery.access` (and `pipeline::Query.access`, `retrieval::Request.access`). Two rules for whoever builds enforcement: retrieval must **pre-filter by access inside the index query** (a condition in the KNN and FULLTEXT `WHERE`, as with `scope_ids`; the `db.rs` test shows it lands in the `KnnScan` predicate), **never post-filter** results, which would leak through counts, scores, highlights and cache keys; and **derived content** (lessons, summaries, outlines, the retrieval cache, traces' `hit_ids`) must carry the **intersection of its sources' audiences**, so a lesson written from an engineering document and a company-wide one is visible only to engineering.
- **What R1 avoids** so it doesn't close doors: no retrieval logic in the app, no scope assumptions in prompts (scope is data), no per-topic tables, no owner-only ids in the context API's wire shape.

### 2.13 Serious quizzes (product item, after providers)

Owner: quizzes must be serious, some must be real quizzes (an answer submitted and graded, not "show answer"), and coding or repository tasks give the learner a code editor whose code is reviewed. This is a product feature, separate from evals, not part of R1. Scope, as one later phase ("Q" in §4):

- `LessonContext.quiz` becomes a spec the planner writes per lesson: `QuizItem { kind: Reveal | Graded { answer_kind: Short | Numeric | Choice | Derivation } | Code { language, starter, tests_hint }, question, rubric, must_cite: [passage ids] }`; the writer renders `Reveal` items as today's `<details>` and the others as a `quiz` fenced block the app renders as real controls (input, choice, a code editor with syntax highlighting; Dioxus webview).
- Grading: `POST /api/lessons/{id}/quiz/{n}/answer` stores the attempt and runs a `Grade` call on the `Write` route with the rubric and the lesson's passages (faithful grading), streaming feedback; code answers are reviewed by the same call with the lesson and the repository context (when repository sources exist) and never executed in R-Q (execution is a later decision). Attempts, grades and feedback are stored on the lesson and traced (2.11).
- Difficulty is part of the plan (`level` + "serious": application and derivation questions, not recall), and the eval suite gets a `quiz` suite (judge: "is this a serious question for this level", grading agreement with a recorded rubric) so "serious" is measured, not asserted.

---

## 3. Eval suite

Principles (owner): **the app has no eval UI; evals live outside the app** and read the service's traces (2.11); **CI never calls a model API or needs the Core ML model**; it runs on fakes and *recorded* fixtures and gates regressions. A separate `margin-eval` binary (its own crate `crates/evals`, its own data folder, never the running service's database), run by hand by the owner, calls real models and the real embedder, writes its own result rows and reads the service's traces over `GET /api/traces` for cost, latency and cache numbers. Golden questions are **not reviewed by hand** (Q8): their quality is shown by the eval itself (3.2, "question quality").

### 3.1 Datasets (checked in under `evals/`)

- `evals/corpus/`: a fixture library of public-domain or self-written material, sized for CI (≤ 5 MB): six prose documents (Markdown with headings, a reST file, an HTML page), three small PDFs (one with a text layer, one scanned page rendered from text so OCR has something to find, one mixed), four images (two diagrams with legible labels, two photos), one short audio clip with a known transcript (for the existing path), and a `folder/` tree with the allow-list mix (`.md`, `.pdf`, `.png`, plus `.rs`/`.json`/`.zip` that must stay invisible).
- `evals/golden/retrieval.jsonl`: ~150 queries, each `{ id, query, scope, relevant: [{document, page?, anchor: "exact text that must be in the hit"}], kind: "lexical" | "semantic" | "cross-modal" | "negative" }`. The first 60 are written with the corpus (the anchors make grading deterministic: a hit is relevant when its text contains the anchor or its unit is the anchored picture); the rest are **generated offline** by the runner (`synth-questions`): the `Write` route is asked for *serious* questions per section (application, comparison, derivation, "which of these two passages contradicts the other", multi-hop across two documents) with the answer span quoted, never recall-of-one-sentence; a second model call (`Small`, different provider when both are configured) grades each generated question on a 0–3 seriousness rubric and checks the quoted span exists; questions under 2 are dropped. No human review (owner Q8): the committed rows carry `generated_by`, `seriousness`, `grader`, and the eval report shows the distribution and the judge-agreement numbers (3.2). Negatives are queries with no answer in the corpus (the metric is "returned nothing above threshold").
- `evals/golden/courses.jsonl`: 8 course requests over the corpus (prompt, sources, expected outline themes, per-lesson "must-cite" documents).
- `evals/recorded/embeddings.bin`: the real EmbeddingGemma vectors of every corpus chunk and golden query, recorded once by `margin-eval record-embeddings` on the owner's Mac (f16, ~2 MB for ~1 200 vectors), keyed by `sha256(prefix + text)` with `embedder_version` in the header (`margin_embed::fake::Recording`, the one format: the runner reads it through the same type, pictures under `picture:{document}#{page}:{tokens}`). `RecordedEmbedder` serves them in CI and fails loudly on an unknown text, so a chunker change that produces new chunk texts requires a re-record (a deliberate, visible step). This is what makes recall@k in CI *real* rather than bag-of-words.
- `evals/recorded/generations/`: recorded model outputs (lessons, outlines, titles) for the golden courses, per `prompt_version` and model, produced by the opt-in runner; `RecordedGenerator` replays them in CI for citation and packing checks.

### 3.2 Metrics

Retrieval (per query kind and overall): **recall@5, recall@10, MRR, nDCG@10**, picture-hit rate for cross-modal queries, false-positive rate on negatives, dedup rate (share of top-k that are overlapping neighbours), per-stage latency. Computed by `evals::metrics` (pure functions, unit-tested against hand-worked examples).

Generation:
- **Citation accuracy** (CI, deterministic): every `[^n]` resolves; the cited passage shares ≥ 2 content terms with the citing sentence (a lexical support proxy); no passage is cited that the packer didn't send; share of source-derived sentences with a marker (checked on recorded outputs against the planner's "must-cite" list).
- **Faithfulness and answer relevance** (opt-in runner only): LLM-as-judge on the `Small` route with a fixed rubric (`evals/rubrics/*.md`: faithfulness 0–3 per claim against the sent passages, relevance 0–3 against the lesson summary, outline coverage of the must-cite documents, "uses the source's terminology"), judged twice with swapped candidate order to cancel position bias, reported with the judge model id.
- **Structure checks** (CI): the lesson rules the prompt states (no `#` title, one Key idea, a worked example, the quiz block when asked) as a cheap proxy for prompt regressions.

Question quality and judge sanity (replaces human review): seriousness distribution of the golden set; inter-judge agreement (two judge models or two orderings, Cohen's κ on the 0–3 grades); a canary set of 20 deliberately bad lessons (wrong formula, uncited claims, off-topic section) that the judge must score low; a canary set of 10 trivial questions that the seriousness grader must reject. A judge whose canaries fail invalidates the run (the report says so).

Efficiency: input/output/cached tokens per call by route, cache-hit share, cost, wall-clock per stage, ingest throughput, index size, all read from the service's trace rows and rollups (2.11) by `trace_id`, so the same numbers the service recorded are the ones the report shows.

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

**Storage**: the runner's own SurrealDB file (`~/Library/Application Support/Margin/evals/db`, separate from the service's), with `eval_run` and `eval_result:[<run_id>, <datetime>, <query_id>]` rows (`variant, suite, metric, value, judge`) using the same time-series id pattern as traces, and the service's trace rows joined by `trace_id` for cost and latency; `compare` prints per-metric deltas with a paired bootstrap 95 % interval (paired per query: the question is "did the variant help").

**Report**: `margin-eval report` writes `evals/reports/<run>.md` (tables) and a self-contained HTML page (one file, charts inline) that the owner opens locally; the latest report's summary table is committed with the PR that changes the pipeline. There is no eval surface in the app (owner Q7).

### 3.5 A/B of chunkers, retrievers, prompts, models

Same corpus, same golden set, two variants, `compare`. For prompts and models the comparison runs the recorded-generation step for both and the judge once over the union (blind to variant). For chunkers, a re-record of embeddings is required and `compare` refuses to run across different embedder versions unless `--allow-embedder-change` is passed, so a "win" is never a side effect of fresh vectors.

---

## 4. Build plan and sequencing

Follows the existing workflow: a shared first commit, parallel areas with exclusive file ownership, integration + e2e, two reviewers, a fixer. The Fable architect reviews each brief first (memory rule). The RAG work is two phases, placed around phase 4, providers and phase 5 as follows:

**R1 — Retrieval core, traces, context API, evals** (§6 is the brief). Service-side areas run **in parallel with phase 4's app-side areas**; R1-app runs after phase 4's app areas merge; phase 4's service areas (4-S, 4-agent) run after R1's service areas merge.

**Phase 4 (folders)**, with these changes to its brief: 4-agent's `read_pdf`/`view_image` and the `FolderReader` trait wait for R1 and are rebuilt on `Retriever` (2.7, 2.8); 4-S's folder byte cache (D7b), `FolderReader` (D7a) and `folder_passages` rows (D7c) are replaced by manifest + ingest; `FOLDER_FETCH_MAX_BYTES`, `PDF_PAGES_PER_READ`, `FOLDER_PICTURES_MAX`, `VIEW_IMAGES_PER_TURN` go (no caps by owner decision; a per-turn picture *token* budget stays). **4-core, 4a, 4b, 4c go ahead unchanged** and in parallel with R1: handle store, `Dir`, pickers, the device transport, uploads and the Folders pane are transport and UI and don't depend on how the service indexes what it receives; the only addition is the manifest walk in `folders/tools.rs` (4-core) and "Refresh" in the pane. The SIWC spike runs in parallel as planned. §6.6 has the exact first-commit rules that keep the two phases from colliding.

**Providers**, as briefed, plus the `Route`/`CallOptions` seam and the Claude cache breakpoints on lesson/scene prefixes (2.6); `TurnDeps.summary_model` becomes `model_for(Small)`; traces gain `provider`.

**Q — Serious quizzes** (2.13), after providers: planner quiz specs, graded answers, the code editor, the `quiz` eval suite.

**R2 — Rerank, rewrite, MCP adapter** (after Q, before or inside phase 5): listwise rerank on `Small`, query rewriting, the Core ML cross-encoder behind `Reranker` if the eval earns it, the MCP server over the context API with per-agent tokens (needs the auth answer, §5.2), `compare`/`report` polish.

**Phase 5** then hardens all of it (compaction per provider, prompt-injection tests over retrieval outputs and the context API, iOS continued processing for folder walks and uploads, trace retention job under load).

Why R1 before phase 4's service area and not after: the folder decision ("ingested, embedded, searched; no caps") is a retrieval decision, and building D7's byte cache first would be throwaway work plus a migration; the app-side areas are unaffected, so phase 4 loses no calendar time.

---

## 6. R1 build brief (ready to run)

Shape as `docs/agent-phase4-brief.md`: one first commit by the orchestrator on `claude/agent-build`, areas in worktrees `agent/r1-<area>` with exclusive files, integrate, e2e, two reviewers, a fixer. Fable reviews this brief before branching (memory rule). Tests: fakes and recorded fixtures only; never the real APIs, never the running service or `~/Library/Application Support/Margin`.

### 6.1 First commit (orchestrator, before branching)

`crates/shared`:
1. `pipeline.rs`: `PipelineConfig { chunker: StageCfg, retriever: StageCfg, fusion: StageCfg, reranker: StageCfg, packer: StageCfg, prompts: String /* version */, routes: RouteTable, budget: Budget }`, `StageCfg { name: String, #[serde(default)] params: serde_json::Map }`, `Budget { share_of_window: f32 (0.5), min_score_pct: u8 (25), images_max: u32 (8), passages_min: u32 (0) }`, `Route { Chat, Write, Small }`, `RouteTable { chat: Option<String>, write: Option<String>, small: Option<String> }` (model ids; `None` = the provider's chat model), `Default` = `{"sections-v2","hybrid-rrf","rrf","none","mmr-v1","lesson-v2", …}`, `pub fn hash(&self) -> String`. Tests: round trip, default hash stable, unknown params preserved.
2. `Settings.pipeline: PipelineConfig` (`#[serde(default)]`), `Settings.trace_retention_days: u32` (90), `SettingsPatch` the same; `Event::DocumentChanged { document: Document }`, `Event::Trace { row: TraceRow }`.
3. `context.rs` (the context API's wire types): `Document { id, project_id, origin: Origin, name, kind, status, generation: u64, bytes, created_at, updated_at }`, `Origin { Source { source_id } | FolderFile { workspace_id, path } | Library { kind, id } | Agent { name } }`, `ContextQuery { text, scope: Scope, modalities, k, budget: Option<Budget> }`, `Scope`, `Hit { document_id, unit_id, passage_id, modality, section_path, page, start_line, end_line, start_ms, end_ms, text, score, highlight }`, `Project { id, name }`, `TraceRow` (2.11 fields; `serde(default)` everywhere additive), `TraceRollup`. `Citation` gains `document_id: Option<Id>`, `start_line`/`end_line` (`#[serde(default)]`); `PassagesCard.passages` keep `Citation`.
4. `routes.rs`: `CONTEXT_SEARCH = /api/context/search`, `context_document_outline(id)`, `context_document_passages(id)`, `CONTEXT_INGEST`, `CONTEXT_CHANGES`, `TRACES`, `TRACES_ROLLUP`, `PROJECTS`; table rows with errors (400 `bad_scope`, 404, 409 `not_ready`). `SearchHit` unchanged (⌘K keeps its shape; the index changes underneath).
5. Contract: §1 ownership rows for the new files, §2 an "R1" row, §3 the `passage`/`document`/`project`/`trace` tables, §5 the tool family, §6 the context routes, §7 constants (`RETRIEVAL_K_DENSE` 40, `RETRIEVAL_K_LEX` 40, `RETRIEVAL_K_FUSED` 24, `RRF_K` 60, `MMR_LAMBDA` 0.7, `TOOL_SEARCH_K_MAX` 12, `TOOL_OUTPUT_PASSAGE_CHARS` 1 200, `TRACE_RETENTION_DAYS` 90), §8 tests.

`crates/service`:
6. `pipeline.rs`: the stage traits of 2.0, `Registry` with `register(name, ctor)`, `Pipeline::build(cfg, reg)` (unknown name → a startup error naming the stage), and the trivial variants so everything compiles: `Chunker "pages-v1"` (today's `margin_embed::chunk`), `Retriever "dense-brute"` (today's Rust scoring, kept only until migration finishes, then deleted by R1-retrieve), `Fusion "first"`, `Reranker "none"`, `Packer "take-k"`; `AppState.pipeline: ArcSwap<Pipeline>` rebuilt on `SettingsChanged`.
7. `db.rs`: the `DEFINE` statements of 2.2 (with `OVERWRITE` for analyzer and indexes), `project`, `document`, `trace`, `trace_hourly`, `trace_daily`; `Store::insert_many`, `Store::range(table, from_id, to_id)`, `Store::query_rows<T>(sql, binds)`; `settings.migrations: Vec<String>`. Tests: the statements run on `kv-mem`; an HNSW query over three rows returns the nearest; `@1@` + `search::score` on a two-row table; `search::rrf` of two legs; a `trace` range query by kind and window; the hourly view updates after an insert.
8. `trace.rs`, `retrieval.rs`, `ingest.rs`, `context_api.rs`: files with the public signatures (2.3, 2.11, 2.12) and `unimplemented!("R1, <area>")` bodies; `server.rs` routes wired to them; `state.rs` fields (`tracer: Tracer`, `ingest_queue`, `retrieval_cache`) constructed with stubs. Nothing else in the service changes.

`crates/embed`:
9. `Chunk` gains `section_path: String`, `start_line`/`end_line: Option<u32>`, `hash: String` (`#[serde(default)]`-equivalent constructors so existing tests compile); `Purpose::Document { title: Option<String> }` (the `Document` variant takes a title; `None` renders `title: none`); `pub trait Reranker` placeholder; `fake::RecordedEmbedder::open(path)` stub.

`crates/evals` (new, in the workspace, `publish = false`): `Cargo.toml`, `src/lib.rs` with `metrics.rs` (recall@k, MRR, nDCG, κ: implemented and unit-tested here, they are pure), `golden.rs` (JSONL types), `src/bin/margin-eval.rs` printing usage; `evals/corpus/` (the fixture files, ≤ 5 MB), `evals/golden/retrieval.jsonl` (the first 60 hand-written serious queries with anchors), `evals/gates.toml`, `evals/prices.toml`, `evals/variants/{baseline,hybrid,dense,bm25}.toml`, `evals/README.md` (how to record, run, compare). `cargo test --workspace` must be green after this commit.

`crates/app`: nothing (R1-app's files are phase 4's until 4-core merges; see 6.6).

### 6.2 Areas (worktree each; files exclusive; all branch from the first commit)

| Area | Worktree | Builds | Owns (exclusively) |
|---|---|---|---|
| **R1-embed** | `agent/r1-embed` | Chunker `"sections-v2"` in `margin_embed` (headings for Markdown/reST/HTML/LaTeX, section paths, line ranges, sentence overlap, atomic tables/fences, `hash`), title prefix in `coreml.rs` token prep, image OCR text as the image's chunk, `pdf_page_picture` (lazy render, from the phase-4 brief D7a), the pipelined embed worker (`embed_pipelined(texts, on_batch)`), `RecordedEmbedder` (f16 file keyed by `embedder_version + sha256(prefix + text)`, fails loudly on a miss), `embedder_version()`; tests incl. the ingest invariants on `evals/corpus` | `crates/embed/src/{chunk,extract,pdf,image,vision,coreml,fake,lib}.rs`, `crates/embed/tests/*` |
| **R1-store** | `agent/r1-store` | `ingest.rs` (document diff by hash, `insert_many` ≤ 64 rows/statement, lock per statement, progress, `Origin` handling, `generation` bump, `DocumentChanged`), the one-pass migration at start (2.2), `sources.rs` reduced to upload/delete/attach + calling `ingest`, `embedding.rs` unchanged, `project` bootstrap ("My library"), `engines::SourceTools` additions (`pdf_page_picture`, `chunk` by name), `FakeSourceTools` accordingly; registry entries for chunkers | `crates/service/src/{ingest,sources,embedding,engines,state,db,library}.rs` (`db.rs` additive after the first commit), `tests/sources.rs`, `tests/common/mod.rs` |
| **R1-retrieve** | `agent/r1-retrieve` | `retrieval.rs`: `Retriever "hybrid-rrf"` (the queries of 2.2), `"dense"`, `"bm25"`, `Fusion "rrf"`/`"linear"`, `Packer "mmr-v1"` (unit dedup, adjacent merge, per-document cap, budget against the route's window, score percentile), the retrieval cache, `QueryPlan`; `search.rs` on the FULLTEXT index for ⌘K; `agent_tools.rs`: the tool family of 2.8 replacing `search_sources`, default scope chat + topic; `ask.rs`: planner call (`plan_course`), per-lesson query plans stored on the proposal, lesson budgets from `PipelineConfig`; `context_api.rs` handlers over the same functions; the `"dense-brute"` retriever deleted once migration exists | `crates/service/src/{retrieval,search,agent_tools,ask,context_api}.rs`, `tests/{courses,api}.rs`, `tests/agent_e2e.rs` (the search-tool tests only; the integrator owns the rest) |
| **R1-ai** | `agent/r1-ai` | `CallOptions { route, cache_key, max_output_tokens }` on `stream_parts`; `prompt_cache_key` sent; lesson and scene prompts reordered constant-first with `prompt_version` (`"lesson-v2"`, `"scene-v2"`); `plan_course` prompt (outline + per-lesson queries + must-cite sections, JSON strict); listwise rerank prompt (unused until R2 but versioned now); `evals` prompt module (serious-question generation, seriousness grader, faithfulness/relevance judge rubrics, canaries); the prefix-identity test against the fake Responses server | `crates/ai/src/{lib,prompts,responses}.rs`, `crates/ai/src/prompts/evals.rs` (new), `crates/ai/tests/fake_responses.rs`, `crates/ai/tests/fixtures_render.rs` |
| **R1-trace** | `agent/r1-trace` | `trace.rs`: `Tracer` (bounded channel, a writer task, `insert_many`, array ids `trace:[kind, at, ulid]`), `TraceSpan` helpers; instrumentation in `turns.rs` (per step, per tool call, condense, title) and `video.rs` (scene call, render), with `pipeline_hash`, `prompt_version`, `route`, `cost_usd` from `prices.toml` loaded at start; `/api/traces` and `/api/traces/rollup` handlers (range and view queries), `Event::Trace` on subscription, the daily retention job (`DELETE` by range per kind) and the hourly cleanup hook; `Chat.usage` unchanged | `crates/service/src/{trace,turns,video,chats}.rs` (`chats.rs` only the `?traces=1` subscription flag), `tests/chats.rs`, `tests/media.rs` |
| **R1-evals** | `agent/r1-evals` | `margin-eval` sub-commands (`record-embeddings`, `synth-questions`, `grade-questions`, `run --suite retrieval|courses|agent --variant`, `judge`, `compare`, `report`, `export`), the runner's own SurrealDB file and `eval_result` rows, trace reading over `GET /api/traces` or exported JSONL, bootstrap intervals, the Markdown + single-file HTML report, the CI gate test (`tests/gates.rs`: `RecordedEmbedder` + `kv-mem` + the real `hybrid-rrf` over `evals/corpus`, thresholds from `gates.toml` vs `evals/baseline.json`), the packing/citation/prompt-prefix gate tests, the judge canary sets | `crates/evals/**`, `evals/**` (the corpus and golden files after the first commit) |

Ownership checks: `server.rs` and `crates/shared` are frozen after the first commit (changes go through the orchestrator); `turns.rs` is R1-trace's alone (phase 4 leaves it untouched); `agent_tools.rs`/`ask.rs` are R1-retrieve's now and 4-S's after R1 merges; `crates/agent` is untouched by R1 (the tool family lives in the service as `TypedTool`s; the picture budget is 4-agent's); `crates/app` is untouched by R1's areas (6.6). R1-store and R1-retrieve both read `db.rs`; only R1-store writes it.

### 6.3 R1-app (after phase 4's app areas merge; worktree `agent/r1-app`)

Settings → AI: three model slots (chats / writing / small tasks) and a **Pipeline** section (variant picker from `GET /api/pipeline/variants` served by the registry, the `Budget` numbers, "Reset to default"), all saving through `SettingsPatch.pipeline`/`routes`; Settings → Advanced: trace retention days. Chat: the Passages card shows section path and locator (page, lines, time) and opens the viewer at the lines; tool rows for `search`/`read_passage`/`document_outline`/`view_picture` with labels and icons; the citation viewer highlights `start_line..end_line`. `fake.rs`: pipeline variants, traces routes (empty), the new card shapes; render tests and `render_screens` envs. Owns `ui/settings.rs`, `ui/chat.rs`, `ui/citations.rs`, `citations.rs`, `api.rs`, `store.rs`, `fake.rs`, `ui/test_render.rs`, `examples/render_screens.rs`, `assets/`.

### 6.4 E2E the integrator adds

1. **A course on the fixture corpus** (`tests/agent_e2e.rs`): upload six corpus documents through the real client (`FakeSourceTools` with `RecordedEmbedder`), wait for `Ready`, check `passage` rows carry `section_path`, lines, `hash`, `scope_ids`; chat → the model calls `search` (scope default = chat + topic; assert the request body's tool list and the output ≤ 16 KB with `n` numbers) → `propose_course` with `survey: true` → the planner request (fake Responses) contains the survey passages and the per-lesson queries come back stored on the proposal → accept → each lesson's fake request contains passages whose ids match a retrieval trace row (`packed_ids`) and whose count respects the budget derived from a fake model window → citations resolve to `document_id` + lines; regenerate hits the cache (one `retrieval` trace, `cache_hit: true`).
2. **Ingest diff**: re-upload an edited document (one section changed) → exactly that section's chunks re-embedded (`ingest` trace `chunks_new == n`, `kept == rest`), generation bumped, `DocumentChanged` once, `GET /api/context/changes?since` lists it.
3. **Context API**: `POST /api/context/search` from a device token returns the same hits as the tool for the same scope; `GET …/outline`, `…/passages?from&to`; `POST /api/context/ingest` with `origin: {agent: "test"}` creates a document with `actor: agent:test`; bad scope 400; unknown document 404.
4. **Traces**: a turn produces `turn`, `model_call` (with `pipeline_hash`, `prompt_version`, `route`, `cost_usd`), `tool_call`, `retrieval` rows under one `trace_id`; `GET /api/traces?kind=model_call&from&to` returns them in order; `GET /api/traces/rollup?bucket=hour` sums them; the retention job deletes rows older than the setting and leaves rollups; no row contains prompt text, an API key or a tool output.
5. **Migration**: a store pre-seeded with old `chunk`/`media` rows and a `Ready` source starts the service → the source is re-ingested in one pass, `passage` rows exist, old tables are gone, `settings.migrations` lists it, and `GET /api/library` is served meanwhile.
6. **⌘K** on the FULLTEXT index returns the same hits as before for the existing tests (snippets from `search::highlight`).
7. **Gates** (`cargo test -p margin-evals`): green against the committed baseline on the recorded embeddings.

Manual, before R1 is called done (recorded in the integration commit message): the owner runs `margin-eval record-embeddings` and `run --suite retrieval --variant baseline` on the Mac, the baseline is committed, and a real course with a real PDF is generated once with traces visible through `GET /api/traces`.

### 6.5 Review lenses

- **Seams frozen**: no area edits the stage traits, `PipelineConfig`'s top-level keys, the context wire types or `server.rs`; every variant is registered by name; `Pipeline::build` fails loudly on an unknown name.
- **Index use**: `EXPLAIN` in tests shows the HNSW and FULLTEXT indexes and the `document_id`/`scope_ids` indexes being used; no `list_where` over `passage` remains; no vector scoring in Rust except the packer's MMR over ≤ `RETRIEVAL_K_FUSED` hits.
- **Locks and batches**: `insert_many` ≤ 64 rows per statement; the write lock held per statement; no `spawn_blocking` inside a lock; the embed worker is pipelined; ingest is cancel-safe (a deleted source stops its ingest).
- **Budgets are config**: no literal token cap in a prompt builder or packer; everything reads `Budget` and the route's window; `images_max` honoured; the score threshold applied before the budget.
- **Citations resolve**: every packed passage has a locator; `cite` keeps `document_id`; lines/pages/times round-trip to the viewer.
- **Traces are safe and complete**: every model call, retrieval, tool call and job step traced with `trace_id`; no prompt text, output text, key or token in a trace; `cost_usd` computed at write time; retention by range; rollup views defined with `OVERWRITE`.
- **Context API = tool path**: the handlers call the same functions as the agent tools; scope resolution has one implementation; `actor` set on every agent write.
- **Fakes only**: no test reaches a model API, Hugging Face or the real Core ML model; `RecordedEmbedder` is the only "real" vector source and fails on a miss; the eval runner is the only thing that may call real models, by hand.
- **Every UI element works** (R1-app): model slots, pipeline picker and budgets, retention days, Passages card locators, viewer line highlight, the new tool rows.

### 6.6 Running R1 and phase 4 side by side

- **Order of first commits**: R1's first commit lands first, then phase 4's first commit on top of it, both by the orchestrator, never in parallel. Phase 4's first commit must contain exactly what its brief lists (shared `Citation.file`, `WorkspaceFile`, `device.rs` D0/frames, `FolderFileView`, the workspace routes, the app `folders/{mod,dir}.rs`, pickers, `Cargo.toml` features, the folder-upload removal) **minus** anything R1's first commit already added to `routes.rs`/`chat.rs` (they are additive, so the second commit only appends), and must **not** add `FOLDER_FETCH_MAX_BYTES`, `PDF_PAGES_PER_READ`, `FOLDER_PICTURES_MAX`, `FOLDER_EXCERPT_MAX_CHARS` or the `FetchFile.known: FileStamp` cap semantics; `FetchFile` stays (it is the upload trigger) with `stamp: FileStamp { bytes, modified, sha256 }` and no size limit, and `DeviceToolCall::Manifest { workspace_id }` → `DeviceToolOutput::Manifest { files: Vec<FileStamp + path + kind> }` is added. Its `Citation.file` sits beside R1's `document_id` on the same struct.
- **Parallel**: R1-embed, R1-store, R1-retrieve, R1-ai, R1-trace, R1-evals ∥ 4-core, 4a, 4b, 4c (and the SIWC spike). No file is shared: R1 touches `crates/{embed,service,ai,evals}`; phase 4's app areas touch `crates/app` only.
- **After R1's service areas merge**: 4-S and 4-agent start, rebriefed on `Retriever`/`ingest` (manifest diff → uploads → `ingest` with `Origin::FolderFile`; `search_text` → `Scope::Workspace`; `read_pdf`/`view_image` over stored passages/pictures; the proposal's folder spans become packed passages like any other). **After phase 4's app areas merge**: R1-app starts. Phase 4's e2e (brief D13) is rewritten by its integrator for the ingest path (no `folder_passages` row, no byte cache); R1's e2e stays as is.
- **If phase 4's app areas finish first**, they wait for nothing: their e2e against the fake service runs on the app side; the service-side folder e2e waits for 4-S by design.

---

## 5. Owner decisions

### 5.1 Decided (second round, see the header)

Q1 chat + topic scope; Q2 three model slots; Q3 no fixed budget, config with generous defaults, measured by evals; Q4 planner; Q5 device hashing; Q6 cross-encoder if the eval earns it; Q7 no eval UI in the app; Q8 no human review, serious questions, quality shown by evals; Q9 office formats not now; Q10 pipeline config in Settings; Q11 one-pass re-embed now. Quizzes: serious, partly graded, code editor for coding tasks (2.13).

**Nothing blocks starting R1.** The questions below are about the Live-context vision and are answered while R1 builds; R1's choices (2.12) stay right under every answer.

### 5.2 Vision-level questions (non-blocking, with recommendations)

1. **Who are the first external agents?** Claude Code/Codex-style coding agents reading project context over MCP (recommended as the first target: it fixes the auth shape, a per-agent token scoped to a project with read/ingest rights), or other products' agents over plain HTTP?
2. **Project = company project; what is the boundary of a corpus?** One project per codebase/team with folders, repos, chats and docs inside it (recommended), or one corpus per company with projects as tags?
3. **Multi-user**: GitHub org login with project membership (recommended when the time comes: the OAuth flow already exists), or invite-by-email accounts?
4. **Do humans and agents share chats?** An agent's ingest shows up as a document with `actor: agent:<name>` (recommended) and agents never write to chats/notes without a human seeing it as such; or should agents be able to write notes and lessons as peers?
5. **Freshness guarantee to sell**: "indexed within a minute of change" via file watchers and repo webhooks (recommended as the target the eval measures: ingest latency p95), or on-demand refresh only?
6. **Where the service runs for a company**: still one Mac per customer (Core ML embedder, PDFKit) or a Linux server with a different embedder behind the same `Embedder` trait? Recommended: keep the Mac for now, but R1's `Embedder`/`SourceTools` seams already allow a Linux build with another model; decide before selling.
7. **Quiz code execution**: review-only (recommended for the first quiz phase) or sandboxed execution of learner code (the video sandbox exists, `sandbox-exec`, but language runtimes don't)?

### 5.3 The original list, kept for the record

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
