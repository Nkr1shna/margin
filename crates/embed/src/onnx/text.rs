//! The text path: prompts and tokens as the model card makes them (`<bos>`, the
//! prefixed text's first `MAX_TOKENS - 2` tokens, `<eos>`), then real batches: inputs
//! sorted by length, packed up to `batch_tokens` padded tokens per run, right-padded
//! with mask 0, and handed back in the caller's order.

use tokenizers::Tokenizer;

use super::session::TextGraph;
use crate::{EmbedError, MAX_TOKENS, Purpose};

/// The most rows one run takes, however short they are.
pub(crate) const MAX_BATCH_ROWS: usize = 64;

/// Tokenizer plus the special tokens the text is wrapped in.
pub(crate) struct Tokens {
    pub tokenizer: Tokenizer,
    pub bos: u32,
    pub eos: u32,
    pub pad: u32,
}

impl Tokens {
    /// `<bos>`, the prefixed text's tokens (at most `MAX_TOKENS - 2`), `<eos>`.
    pub fn ids(&self, text: &str, purpose: &Purpose) -> Result<Vec<u32>, EmbedError> {
        let prompt = crate::prompt(purpose);
        let encoding = self
            .tokenizer
            .encode(format!("{prompt}{text}"), false)
            .map_err(|e| EmbedError::Embed(format!("tokenizing: {e}")))?;
        let tokens = encoding.get_ids();
        let kept = tokens.len().min(MAX_TOKENS - 2);
        let mut ids = Vec::with_capacity(kept + 2);
        ids.push(self.bos);
        ids.extend_from_slice(&tokens[..kept]);
        ids.push(self.eos);
        Ok(ids)
    }

    /// Tokens `text` takes (no prefix, no special tokens).
    pub fn count(&self, text: &str) -> usize {
        self.tokenizer
            .encode(text, false)
            .map(|e| e.get_ids().len())
            .unwrap_or(0)
    }
}

/// Groups input indices into runs: shortest first, each run as many rows as fit
/// `batch_tokens` padded tokens (rows × the run's longest) and `MAX_BATCH_ROWS`, at least
/// one row. Ties keep input order, so the plan is deterministic.
pub(crate) fn plan_batches(lens: &[usize], batch_tokens: usize) -> Vec<Vec<usize>> {
    let mut order: Vec<usize> = (0..lens.len()).collect();
    order.sort_by_key(|&i| lens[i]);
    let mut batches: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    for i in order {
        // Sorted, so this input is the run's longest.
        let fits = (current.len() + 1) * lens[i] <= batch_tokens && current.len() < MAX_BATCH_ROWS;
        if !current.is_empty() && !fits {
            batches.push(std::mem::take(&mut current));
        }
        current.push(i);
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

fn normalize(mut v: Vec<f32>) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
    v
}

/// Embeds token id lists through `graph` in planned batches; vectors in input order,
/// unit length.
pub(crate) fn run_batched(
    graph: &mut dyn TextGraph,
    inputs: &[Vec<u32>],
    batch_tokens: usize,
    pad: u32,
) -> Result<Vec<Vec<f32>>, EmbedError> {
    let lens: Vec<usize> = inputs.iter().map(Vec::len).collect();
    let mut out: Vec<Option<Vec<f32>>> = vec![None; inputs.len()];
    for batch in plan_batches(&lens, batch_tokens) {
        let rows = batch.len();
        let cols = batch.iter().map(|&i| lens[i]).max().unwrap_or(0).max(1);
        let mut ids = vec![pad as i64; rows * cols];
        let mut mask = vec![0i64; rows * cols];
        for (r, &i) in batch.iter().enumerate() {
            for (c, &id) in inputs[i].iter().enumerate() {
                ids[r * cols + c] = id as i64;
                mask[r * cols + c] = 1;
            }
        }
        let vectors = graph.run(&ids, &mask, rows, cols)?;
        if vectors.len() != rows {
            return Err(EmbedError::Embed(format!(
                "the model returned {} vectors for {rows} inputs",
                vectors.len()
            )));
        }
        for (&i, v) in batch.iter().zip(vectors) {
            if v.len() != crate::DIM || v.iter().any(|x| !x.is_finite()) {
                return Err(EmbedError::Embed(
                    "the model returned a malformed vector".into(),
                ));
            }
            out[i] = Some(normalize(v));
        }
    }
    Ok(out.into_iter().map(Option::unwrap_or_default).collect())
}

/// Embeds one group of token id lists.
pub(crate) type RunGroup<'a> = dyn FnMut(&[Vec<u32>]) -> Result<Vec<Vec<f32>>, EmbedError> + 'a;

/// Tokenizes groups of `batch` inputs on a second thread (at most one group ahead) while
/// `run` embeds each group on this one; `on_batch(first index, vectors)` per group, in
/// order. Any error stops both sides and is returned.
pub(crate) fn pipelined_groups(
    inputs: &[(String, Purpose)],
    batch: usize,
    tokens: &Tokens,
    run: &mut RunGroup<'_>,
    on_batch: &mut dyn FnMut(usize, Vec<Vec<f32>>) -> Result<(), EmbedError>,
) -> Result<(), EmbedError> {
    let batch = batch.max(1);
    std::thread::scope(|scope| {
        let (send, receive) = std::sync::mpsc::sync_channel::<Result<Vec<Vec<u32>>, EmbedError>>(1);
        scope.spawn(move || {
            for group in inputs.chunks(batch) {
                let ids: Result<Vec<Vec<u32>>, EmbedError> =
                    group.iter().map(|(t, p)| tokens.ids(t, p)).collect();
                let failed = ids.is_err();
                if send.send(ids).is_err() || failed {
                    break;
                }
            }
        });
        let mut first = 0;
        for ids in receive.iter() {
            let ids = ids?;
            let n = ids.len();
            on_batch(first, run(&ids)?)?;
            first += n;
        }
        Ok(())
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::DIM;

    /// A tokenizer splitting on whitespace over a fixed vocabulary (unknown words share
    /// `[UNK]`), with Gemma's special ids: `<pad>` 0, `<eos>` 1, `<bos>` 2.
    pub(crate) fn tokens() -> Tokens {
        let mut vocab = serde_json::Map::new();
        for (i, w) in ["<pad>", "<eos>", "<bos>", "[UNK]"].iter().enumerate() {
            vocab.insert(w.to_string(), i.into());
        }
        let words = "title: none | text: task: search result query: the a fourier series \
                     sine cosine plants light sugar water heat engine notes chapter one two \
                     three four five six seven eight nine ten word";
        for w in words.split_whitespace() {
            let n = vocab.len();
            vocab.entry(w.to_string()).or_insert(n.into());
        }
        let json = serde_json::json!({
            "version": "1.0",
            "truncation": null,
            "padding": null,
            "added_tokens": [],
            "normalizer": null,
            "pre_tokenizer": {"type": "WhitespaceSplit"},
            "post_processor": null,
            "decoder": null,
            "model": {"type": "WordLevel", "vocab": vocab, "unk_token": "[UNK]"}
        });
        let tokenizer: Tokenizer = json.to_string().parse().unwrap();
        Tokens {
            tokenizer,
            bos: 2,
            eos: 1,
            pad: 0,
        }
    }

    /// A stand-in graph: each row's vector counts its unmasked ids (one bucket per id),
    /// so padding must not change it. Records every run's shape.
    #[derive(Clone, Default)]
    pub(crate) struct StubGraph {
        pub runs: Arc<Mutex<Vec<(usize, usize)>>>,
        /// Fails runs after this many.
        pub fail_after: Option<usize>,
    }

    impl TextGraph for StubGraph {
        fn run(
            &mut self,
            ids: &[i64],
            mask: &[i64],
            rows: usize,
            cols: usize,
        ) -> Result<Vec<Vec<f32>>, EmbedError> {
            assert_eq!(ids.len(), rows * cols);
            assert_eq!(mask.len(), rows * cols);
            let mut runs = self.runs.lock().unwrap();
            if self.fail_after.is_some_and(|n| runs.len() >= n) {
                return Err(EmbedError::Embed("stub failure".into()));
            }
            runs.push((rows, cols));
            Ok((0..rows)
                .map(|r| {
                    let mut v = vec![0f32; DIM];
                    for c in 0..cols {
                        if mask[r * cols + c] == 1 {
                            // Position-weighted, so order matters a little.
                            v[ids[r * cols + c] as usize % DIM] += 1.0 + c as f32 * 1e-3;
                        } else {
                            assert_eq!(ids[r * cols + c], 0, "padding is <pad>");
                        }
                    }
                    v
                })
                .collect())
        }
    }

    #[test]
    fn ids_are_wrapped_prefixed_and_cut() {
        let t = tokens();
        let ids = t.ids("fourier series", &Purpose::DOCUMENT).unwrap();
        // <bos> title: none | text: fourier series <eos>
        assert_eq!(ids.len(), 8);
        assert_eq!((ids[0], *ids.last().unwrap()), (2, 1));
        let q = t.ids("fourier series", &Purpose::Query).unwrap();
        assert_ne!(ids, q);
        let titled = t.ids("x", &Purpose::document(Some("notes"))).unwrap();
        assert!(titled.contains(&t.tokenizer.token_to_id("notes").unwrap()));
        let long = "word ".repeat(2000);
        let ids = t.ids(&long, &Purpose::DOCUMENT).unwrap();
        assert_eq!(ids.len(), MAX_TOKENS);
        assert_eq!(*ids.last().unwrap(), 1);
        assert_eq!(t.count("fourier series sine"), 3);
        assert_eq!(t.count(""), 0);
    }

    #[test]
    fn batches_pack_short_inputs_and_respect_the_budget() {
        let lens = [10, 400, 12, 400, 50, 11];
        let plan = plan_batches(&lens, 1000);
        // Shortest first; every input exactly once.
        let mut all: Vec<usize> = plan.iter().flatten().copied().collect();
        assert_eq!(plan[0][0], 0);
        all.sort();
        assert_eq!(all, (0..lens.len()).collect::<Vec<_>>());
        for b in &plan {
            let max = b.iter().map(|&i| lens[i]).max().unwrap();
            assert!(b.len() == 1 || b.len() * max <= 1000, "{b:?}");
        }
        assert_eq!(plan.last().unwrap().len(), 2, "{plan:?}");
        // One input longer than the budget still runs, alone.
        assert_eq!(plan_batches(&[5000, 3], 1000), vec![vec![1], vec![0]]);
        // Rows are capped.
        let plan = plan_batches(&[3; 200], 100_000);
        assert!(plan.iter().all(|b| b.len() <= MAX_BATCH_ROWS));
        assert!(plan_batches(&[], 10).is_empty());
    }

    #[test]
    fn batched_vectors_match_one_by_one_in_input_order() {
        let t = tokens();
        let texts = [
            "fourier series sine cosine fourier series sine cosine",
            "plants",
            "heat engine",
            "the a word word word word word word word word word word",
            "plants light sugar water",
        ];
        let ids: Vec<Vec<u32>> = texts
            .iter()
            .map(|s| t.ids(s, &Purpose::DOCUMENT).unwrap())
            .collect();
        let graph = StubGraph::default();
        let together = run_batched(&mut graph.clone(), &ids, 64, 0).unwrap();
        assert!(
            graph.runs.lock().unwrap().len() > 1,
            "the budget splits them"
        );
        for (i, one) in ids.iter().enumerate() {
            let alone =
                run_batched(&mut StubGraph::default(), std::slice::from_ref(one), 64, 0).unwrap();
            assert_eq!(alone[0], together[i], "input {i}");
            let norm = together[i].iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-5);
        }
        for (rows, cols) in graph.runs.lock().unwrap().iter() {
            assert!(rows * cols <= 64 || *rows == 1);
        }
    }

    #[test]
    fn graph_errors_and_bad_vectors_are_errors() {
        let t = tokens();
        let ids = vec![t.ids("plants", &Purpose::Query).unwrap()];
        let mut failing = StubGraph {
            fail_after: Some(0),
            ..Default::default()
        };
        assert!(matches!(
            run_batched(&mut failing, &ids, 4096, 0),
            Err(EmbedError::Embed(_))
        ));
        struct Short;
        impl TextGraph for Short {
            fn run(
                &mut self,
                _: &[i64],
                _: &[i64],
                rows: usize,
                _: usize,
            ) -> Result<Vec<Vec<f32>>, EmbedError> {
                Ok(vec![vec![f32::NAN; DIM]; rows])
            }
        }
        assert!(run_batched(&mut Short, &ids, 4096, 0).is_err());
    }

    #[test]
    fn pipelined_groups_keep_order_and_stop_on_error() {
        let t = tokens();
        let inputs: Vec<(String, Purpose)> = (0..10)
            .map(|i| ("plants word ".repeat(i + 1), Purpose::DOCUMENT))
            .collect();
        let mut graph = StubGraph::default();
        let mut got = Vec::new();
        pipelined_groups(
            &inputs,
            4,
            &t,
            &mut |ids| run_batched(&mut graph, ids, 4096, 0),
            &mut |first, v| {
                got.push((first, v));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            got.iter().map(|(f, v)| (*f, v.len())).collect::<Vec<_>>(),
            vec![(0, 4), (4, 4), (8, 2)]
        );
        let all: Vec<Vec<u32>> = inputs.iter().map(|(s, p)| t.ids(s, p).unwrap()).collect();
        let direct = run_batched(&mut StubGraph::default(), &all, 4096, 0).unwrap();
        let flat: Vec<Vec<f32>> = got.into_iter().flat_map(|(_, v)| v).collect();
        assert_eq!(flat, direct);

        let mut calls = 0;
        let err = pipelined_groups(
            &inputs,
            4,
            &t,
            &mut |ids| run_batched(&mut StubGraph::default(), ids, 4096, 0),
            &mut |_, _| {
                calls += 1;
                Err(EmbedError::Embed("cancelled".into()))
            },
        )
        .unwrap_err();
        assert!(matches!(err, EmbedError::Embed(m) if m == "cancelled"));
        assert_eq!(calls, 1);
    }
}
