//! Retrieval and agreement metrics. Pure functions over judged rankings, so they are
//! tested against hand-worked examples and shared by the CI gates and the opt-in
//! runner (`docs/agent-rag-architecture.md` §3.2).
//!
//! A ranking is judged as `&[Vec<usize>]`: for each hit, best first, the indices of the
//! query's relevant items it satisfies (its text contains the item's anchor, or it is the
//! anchored picture). A hit may satisfy several items; an item satisfied twice counts
//! once, so overlapping neighbours don't inflate a score.

/// Share of the relevant items found in the top `k`; `None` when the query has none (a
/// negative query, scored with `false_positive` instead).
pub fn recall_at_k(judged: &[Vec<usize>], n_relevant: usize, k: usize) -> Option<f64> {
    if n_relevant == 0 {
        return None;
    }
    let mut found = vec![false; n_relevant];
    for hit in judged.iter().take(k) {
        for &i in hit {
            if i < n_relevant {
                found[i] = true;
            }
        }
    }
    Some(found.iter().filter(|f| **f).count() as f64 / n_relevant as f64)
}

/// 1 / the rank of the first relevant hit; 0 when none is relevant.
pub fn reciprocal_rank(judged: &[Vec<usize>]) -> f64 {
    judged
        .iter()
        .position(|hit| !hit.is_empty())
        .map_or(0.0, |i| 1.0 / (i + 1) as f64)
}

/// The mean of the values (0 for none): MRR is the mean of `reciprocal_rank`s.
pub fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

/// nDCG@k with binary gains: a hit gains 1 when it satisfies a relevant item no earlier
/// hit satisfied, discounted by log2(rank + 1); the ideal ranking puts one new item at
/// each of the first `min(k, n_relevant)` ranks. `None` without relevant items.
pub fn ndcg_at_k(judged: &[Vec<usize>], n_relevant: usize, k: usize) -> Option<f64> {
    if n_relevant == 0 {
        return None;
    }
    let mut credited = vec![false; n_relevant];
    let gains: Vec<f64> = judged
        .iter()
        .take(k)
        .map(|hit| {
            let new = hit
                .iter()
                .filter(|&&i| i < n_relevant && !credited[i])
                .copied()
                .collect::<Vec<_>>();
            for i in &new {
                credited[*i] = true;
            }
            if new.is_empty() { 0.0 } else { 1.0 }
        })
        .collect();
    let ideal = vec![1.0; n_relevant.min(k)];
    Some(ndcg(&gains, &ideal))
}

/// nDCG of graded `gains` (in ranked order) against the `ideal` gains (any order).
pub fn ndcg(gains: &[f64], ideal: &[f64]) -> f64 {
    let dcg = |g: &[f64]| -> f64 {
        g.iter()
            .enumerate()
            .map(|(i, g)| g / ((i + 2) as f64).log2())
            .sum()
    };
    let mut ideal = ideal.to_vec();
    ideal.sort_by(|a, b| b.total_cmp(a));
    let best = dcg(&ideal);
    if best == 0.0 { 0.0 } else { dcg(gains) / best }
}

/// For a negative query: whether anything came back scoring at or above `threshold`.
pub fn false_positive(scores: &[f32], threshold: f32) -> bool {
    scores.iter().any(|s| *s >= threshold)
}

/// Share of the top `k` that overlap an earlier hit of the same document (adjacent
/// chunks both retrieved): `overlaps[i]` says whether hit `i` does.
pub fn dedup_rate(overlaps: &[bool], k: usize) -> f64 {
    let top = &overlaps[..overlaps.len().min(k)];
    if top.is_empty() {
        0.0
    } else {
        top.iter().filter(|o| **o).count() as f64 / top.len() as f64
    }
}

/// Cohen's kappa between two raters' labels of the same items: agreement corrected for
/// chance. 1 when they agree on everything (including when only one label is used).
pub fn cohen_kappa(a: &[u8], b: &[u8]) -> f64 {
    weighted_kappa(a, b, |i, j| if i == j { 0.0 } else { 1.0 })
}

/// Cohen's kappa with quadratic weights, for ordinal grades such as the 0–3 rubrics:
/// disagreeing by two grades costs four times as much as by one.
pub fn quadratic_kappa(a: &[u8], b: &[u8]) -> f64 {
    weighted_kappa(a, b, |i, j| {
        let d = i as f64 - j as f64;
        d * d
    })
}

fn weighted_kappa(a: &[u8], b: &[u8], weight: impl Fn(usize, usize) -> f64) -> f64 {
    assert_eq!(a.len(), b.len(), "both raters must grade the same items");
    if a.is_empty() {
        return 1.0;
    }
    let k = a.iter().chain(b).copied().max().unwrap_or(0) as usize + 1;
    let n = a.len() as f64;
    let mut observed = vec![vec![0.0; k]; k];
    let mut ra = vec![0.0; k];
    let mut rb = vec![0.0; k];
    for (&x, &y) in a.iter().zip(b) {
        observed[x as usize][y as usize] += 1.0 / n;
        ra[x as usize] += 1.0 / n;
        rb[y as usize] += 1.0 / n;
    }
    let (mut po, mut pe) = (0.0, 0.0);
    for i in 0..k {
        for j in 0..k {
            po += weight(i, j) * observed[i][j];
            pe += weight(i, j) * ra[i] * rb[j];
        }
    }
    if pe == 0.0 { 1.0 } else { 1.0 - po / pe }
}

/// The `p`th percentile (0–100) of `values` by nearest rank: the smallest value with at
/// least `p` % of the values at or below it. 0 for no values.
pub fn percentile(values: &[f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    let rank = ((p / 100.0) * v.len() as f64).ceil() as usize;
    v[rank.clamp(1, v.len()) - 1]
}

/// A paired bootstrap interval for the mean of per-item differences `b − a`.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Interval {
    pub mean: f64,
    pub lo: f64,
    pub hi: f64,
    pub n: usize,
}

impl Interval {
    /// Whether the interval excludes zero: the difference is more than noise.
    pub fn significant(&self) -> bool {
        self.lo > 0.0 || self.hi < 0.0
    }
}

/// The 95 % paired bootstrap interval of `mean(b − a)`: the per-item differences are
/// resampled with replacement `iters` times (a fixed-seed generator, so a report is
/// reproducible) and the 2.5th and 97.5th percentiles of the resampled means taken.
/// Paired, because the question is whether variant `b` helped on the same queries.
pub fn paired_bootstrap(a: &[f64], b: &[f64], iters: usize, seed: u64) -> Interval {
    assert_eq!(a.len(), b.len(), "a paired bootstrap needs paired values");
    let d: Vec<f64> = a.iter().zip(b).map(|(x, y)| y - x).collect();
    let n = d.len();
    if n == 0 {
        return Interval {
            mean: 0.0,
            lo: 0.0,
            hi: 0.0,
            n,
        };
    }
    let mut state = seed | 1;
    let mut next = move || {
        // xorshift64*
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    };
    let means: Vec<f64> = (0..iters.max(1))
        .map(|_| (0..n).map(|_| d[(next() % n as u64) as usize]).sum::<f64>() / n as f64)
        .collect();
    Interval {
        mean: mean(&d),
        lo: percentile(&means, 2.5),
        hi: percentile(&means, 97.5),
        n,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-4, "{a} != {b}");
    }

    /// Hits: miss, item 0, miss, item 1, item 0 again; two relevant items.
    fn example() -> Vec<Vec<usize>> {
        vec![vec![], vec![0], vec![], vec![1], vec![0]]
    }

    #[test]
    fn recall() {
        let j = example();
        assert_eq!(recall_at_k(&j, 2, 1), Some(0.0));
        assert_eq!(recall_at_k(&j, 2, 2), Some(0.5));
        assert_eq!(recall_at_k(&j, 2, 4), Some(1.0));
        assert_eq!(recall_at_k(&j, 2, 10), Some(1.0));
        assert_eq!(recall_at_k(&j, 0, 5), None);
        // One hit satisfying both items finds both.
        assert_eq!(recall_at_k(&[vec![0, 1]], 2, 1), Some(1.0));
    }

    #[test]
    fn reciprocal_rank_and_mrr() {
        assert_eq!(reciprocal_rank(&example()), 0.5);
        assert_eq!(reciprocal_rank(&[vec![], vec![]]), 0.0);
        assert_eq!(reciprocal_rank(&[vec![3]]), 1.0);
        close(mean(&[0.5, 1.0, 0.0, 1.0 / 3.0]), 0.458333);
        assert_eq!(mean(&[]), 0.0);
    }

    #[test]
    fn ndcg_by_hand() {
        // Gains [0, 1, 0, 1, 0] (the repeat of item 0 gains nothing):
        // DCG = 1/log2(3) + 1/log2(5) = 0.630930 + 0.430677 = 1.061606
        // IDCG = 1 + 1/log2(3) = 1.630930 → 0.650916.
        close(ndcg_at_k(&example(), 2, 5).unwrap(), 0.650916);
        // Perfect ranking.
        close(ndcg_at_k(&[vec![0], vec![1]], 2, 10).unwrap(), 1.0);
        // Nothing relevant retrieved.
        close(ndcg_at_k(&[vec![], vec![]], 2, 10).unwrap(), 0.0);
        assert_eq!(ndcg_at_k(&[vec![]], 0, 10), None);
        // Graded: gains [3, 2, 0] against ideal [3, 2, 1]:
        // DCG = 3 + 2/log2(3) = 4.261860; IDCG = 4.261860 + 1/2 = 4.761860 → 0.894999.
        close(ndcg(&[3.0, 2.0, 0.0], &[1.0, 3.0, 2.0]), 0.894999);
    }

    #[test]
    fn negatives_and_dedup() {
        assert!(false_positive(&[0.2, 0.7], 0.5));
        assert!(!false_positive(&[0.2, 0.4], 0.5));
        assert!(!false_positive(&[], 0.5));
        close(dedup_rate(&[false, true, false, true], 4), 0.5);
        close(dedup_rate(&[false, true, true], 2), 0.5);
        assert_eq!(dedup_rate(&[], 5), 0.0);
    }

    #[test]
    fn percentiles_by_nearest_rank() {
        let v = [5.0, 1.0, 3.0, 2.0, 4.0];
        assert_eq!(percentile(&v, 0.0), 1.0);
        assert_eq!(percentile(&v, 20.0), 1.0);
        assert_eq!(percentile(&v, 21.0), 2.0);
        assert_eq!(percentile(&v, 50.0), 3.0);
        assert_eq!(percentile(&v, 95.0), 5.0);
        assert_eq!(percentile(&v, 100.0), 5.0);
        assert_eq!(percentile(&[], 50.0), 0.0);
    }

    #[test]
    fn paired_bootstrap_intervals() {
        // Every query improved by exactly 0.1: no spread at all.
        let a = [0.2, 0.5, 0.7, 0.1];
        let b = [0.3, 0.6, 0.8, 0.2];
        let i = paired_bootstrap(&a, &b, 1000, 7);
        close(i.mean, 0.1);
        close(i.lo, 0.1);
        close(i.hi, 0.1);
        assert!(i.significant());
        // Half better, half worse by the same amount: the interval straddles zero.
        let a = [0.0; 40];
        let b: Vec<f64> = (0..40).map(|i| if i % 2 == 0 { 1.0 } else { -1.0 }).collect();
        let i = paired_bootstrap(&a, &b, 2000, 7);
        close(i.mean, 0.0);
        assert!(i.lo < 0.0 && i.hi > 0.0, "{i:?}");
        assert!(!i.significant());
        // Reproducible with the same seed.
        assert_eq!(i, paired_bootstrap(&a, &b, 2000, 7));
        assert_eq!(paired_bootstrap(&[], &[], 10, 1).n, 0);
    }

    #[test]
    fn kappa_by_hand() {
        // po = 0.75; marginals a (0.5, 0.5), b (0.25, 0.75); pe = 0.5 → κ = 0.5.
        close(cohen_kappa(&[0, 0, 1, 1], &[0, 1, 1, 1]), 0.5);
        close(cohen_kappa(&[0, 1, 2, 3], &[0, 1, 2, 3]), 1.0);
        close(cohen_kappa(&[2, 2, 2], &[2, 2, 2]), 1.0);
        // Systematic disagreement is worse than chance.
        assert!(cohen_kappa(&[0, 1, 0, 1], &[1, 0, 1, 0]) < 0.0);
        // With two grades the quadratic weights equal the plain ones.
        close(quadratic_kappa(&[0, 0, 1, 1], &[0, 1, 1, 1]), 0.5);
        // Off by one everywhere on a 0–3 scale is mostly agreement, quadratically:
        // a = [0,1,2,3], b = [1,2,3,3]: Σw·o = (1+1+1+0)/4 = 0.75;
        // Σw·ra·rb = Σ_i Σ_j (i−j)²·(1/4)·rb_j with rb = (0, 1/4, 1/4, 1/2) = 2.5
        // → κ = 1 − 0.75/2.5 = 0.7.
        close(quadratic_kappa(&[0, 1, 2, 3], &[1, 2, 3, 3]), 0.7);
        assert!(cohen_kappa(&[0, 1, 2, 3], &[1, 2, 3, 3]) < 0.7);
    }
}
