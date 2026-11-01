//! Re-ranking strategies for search results
//!
//! Re-ranking improves search quality by reordering results based on relevance
//! and diversity criteria.

use crate::{error::Result, retrieval::bm25::tokenize, types::SearchResult};
use std::collections::HashSet;

/// Re-ranking strategy trait
pub trait ReRankingStrategy: Send + Sync {
  /// Re-rank search results
  fn rerank(&self, query: &str, results: Vec<SearchResult>) -> Result<Vec<SearchResult>>;
}

/// No-op re-ranking (keeps original order)
pub struct NoReRanking;

impl ReRankingStrategy for NoReRanking {
  fn rerank(&self, _query: &str, results: Vec<SearchResult>) -> Result<Vec<SearchResult>> {
    Ok(results)
  }
}

/// MMR (Maximal Marginal Relevance) re-ranking
///
/// MMR balances relevance and diversity in search results by penalizing
/// redundancy. It iteratively selects documents that are:
/// 1. Highly relevant to the query
/// 2. Dissimilar from already selected documents
///
/// # Algorithm
/// ```text
/// MMR = λ * Relevance(doc, query) - (1-λ) * max(Similarity(doc, selected_docs))
/// ```
///
/// Where:
/// - λ (lambda) controls the relevance-diversity tradeoff
/// - λ = 1.0: Pure relevance (no diversity)
/// - λ = 0.0: Pure diversity (no relevance consideration)
/// - λ = 0.5: Balanced
///
/// # Example
/// ```ignore
/// use yanshi_rag::reranking::{MMRReRanking, ReRankingStrategy};
/// use yanshi_rag::types::SearchResult;
///
/// let mmr = MMRReRanking::new(0.7); // Favor relevance with some diversity
/// let reranked = mmr.rerank("query", results)?;
/// ```
pub struct MMRReRanking {
  /// Diversity parameter (0.0 = pure diversity, 1.0 = pure relevance)
  lambda: f32,
}

impl MMRReRanking {
  /// Create a new MMR re-ranker with specified lambda
  ///
  /// # Arguments
  /// * `lambda` - Relevance vs diversity weight (0.0 to 1.0)
  ///   - 1.0: Pure relevance ranking
  ///   - 0.5: Balanced relevance and diversity
  ///   - 0.0: Maximum diversity
  pub fn new(lambda: f32) -> Self {
    Self {
      lambda: lambda.clamp(0.0, 1.0),
    }
  }

  /// Compute Jaccard similarity between two documents over the BM25
  /// terms (so unspaced Chinese/Japanese text compares by bigrams instead
  /// of as one whole-sentence token).
  ///
  /// Jaccard similarity = |A ∩ B| / |A ∪ B|
  fn jaccard_similarity(&self, doc1: &str, doc2: &str) -> f32 {
    let tokens1: HashSet<String> = tokenize(doc1).into_iter().collect();
    let tokens2: HashSet<String> = tokenize(doc2).into_iter().collect();

    if tokens1.is_empty() && tokens2.is_empty() {
      return 1.0; // Both empty, consider identical
    }

    let intersection = tokens1.intersection(&tokens2).count();
    let union = tokens1.union(&tokens2).count();

    if union == 0 {
      0.0
    } else {
      intersection as f32 / union as f32
    }
  }

  /// Compute maximum similarity between a candidate and selected documents
  fn max_similarity_to_selected(&self, candidate: &SearchResult, selected: &[SearchResult]) -> f32 {
    if selected.is_empty() {
      return 0.0;
    }

    selected
      .iter()
      .map(|doc| self.jaccard_similarity(&candidate.content, &doc.content))
      .fold(0.0f32, f32::max)
  }

  /// Calculate MMR score for a candidate document
  fn calculate_mmr_score(&self, candidate: &SearchResult, selected: &[SearchResult]) -> f32 {
    let relevance = candidate.score;
    let max_similarity = self.max_similarity_to_selected(candidate, selected);

    self.lambda * relevance - (1.0 - self.lambda) * max_similarity
  }
}

impl ReRankingStrategy for MMRReRanking {
  fn rerank(&self, _query: &str, mut results: Vec<SearchResult>) -> Result<Vec<SearchResult>> {
    if results.len() <= 1 {
      return Ok(results);
    }

    // Sort by score descending initially
    results.sort_by(|a, b| {
      b.score
        .partial_cmp(&a.score)
        .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Implement MMR iterative selection
    let mut selected: Vec<SearchResult> = Vec::new();
    let mut remaining = results;

    // Always select the highest scoring document first
    if !remaining.is_empty() {
      selected.push(remaining.remove(0));
    }

    // Iteratively select documents with highest MMR score
    while !remaining.is_empty() {
      // Calculate MMR score for each remaining document
      let mut best_idx = 0;
      let mut best_mmr_score = f32::NEG_INFINITY;

      for (idx, candidate) in remaining.iter().enumerate() {
        let mmr_score = self.calculate_mmr_score(candidate, &selected);
        if mmr_score > best_mmr_score {
          best_mmr_score = mmr_score;
          best_idx = idx;
        }
      }

      // Move the best document from remaining to selected
      selected.push(remaining.remove(best_idx));
    }

    Ok(selected)
  }
}

/// Score-based re-ranking (simple sort by score)
///
/// This is useful when you want to ensure results are strictly ordered by
/// relevance score, which may not always be the case depending on the
/// retrieval strategy.
pub struct ScoreReRanking {
  /// Sort order (true = descending, false = ascending)
  descending: bool,
}

impl ScoreReRanking {
  /// Create a new score-based re-ranker
  pub fn new(descending: bool) -> Self {
    Self { descending }
  }

  /// Create a new score-based re-ranker (descending order)
  pub fn descending() -> Self {
    Self::new(true)
  }

  /// Create a new score-based re-ranker (ascending order)
  pub fn ascending() -> Self {
    Self::new(false)
  }
}

impl ReRankingStrategy for ScoreReRanking {
  fn rerank(&self, _query: &str, mut results: Vec<SearchResult>) -> Result<Vec<SearchResult>> {
    if self.descending {
      results.sort_by(|a, b| {
        b.score
          .partial_cmp(&a.score)
          .unwrap_or(std::cmp::Ordering::Equal)
      });
    } else {
      results.sort_by(|a, b| {
        a.score
          .partial_cmp(&b.score)
          .unwrap_or(std::cmp::Ordering::Equal)
      });
    }
    Ok(results)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::HashMap;

  fn create_test_result(id: &str, content: &str, score: f32) -> SearchResult {
    SearchResult {
      id: id.to_string(),
      content: content.to_string(),
      score,
      metadata: HashMap::new(),
    }
  }

  #[test]
  fn mmr_demotes_near_duplicate_chinese_results() {
    let reranker = MMRReRanking::new(0.5);
    let results = vec![
      create_test_result("a", "机器学习是人工智能的一个分支", 0.9),
      create_test_result("a-dup", "机器学习是人工智能的重要分支", 0.88),
      create_test_result("other", "今天天气很好适合散步", 0.8),
    ];
    let ids: Vec<String> = reranker
      .rerank("q", results)
      .unwrap()
      .into_iter()
      .map(|r| r.id)
      .collect();
    assert_eq!(ids, ["a", "other", "a-dup"]);
  }

  #[test]
  fn test_no_reranking() {
    let reranker = NoReRanking;
    let results = vec![
      create_test_result("1", "content", 0.9),
      create_test_result("2", "other", 0.8),
    ];

    let reranked = reranker.rerank("query", results.clone()).unwrap();
    assert_eq!(reranked.len(), 2);
    assert_eq!(reranked[0].id, "1");
    assert_eq!(reranked[1].id, "2");
  }

  #[test]
  fn test_mmr_single_document() {
    let mmr = MMRReRanking::new(0.5);
    let results = vec![create_test_result("1", "content", 0.9)];

    let reranked = mmr.rerank("query", results).unwrap();
    assert_eq!(reranked.len(), 1);
    assert_eq!(reranked[0].id, "1");
  }

  #[test]
  fn test_mmr_pure_relevance() {
    // Lambda = 1.0 means pure relevance (should keep original order)
    let mmr = MMRReRanking::new(1.0);
    let results = vec![
      create_test_result("1", "the quick brown fox", 0.9),
      create_test_result("2", "the quick brown fox", 0.8), // Duplicate content
      create_test_result("3", "completely different", 0.7),
    ];

    let reranked = mmr.rerank("query", results).unwrap();
    assert_eq!(reranked.len(), 3);
    assert_eq!(reranked[0].id, "1"); // Highest score
    assert_eq!(reranked[1].id, "2"); // Second highest
    assert_eq!(reranked[2].id, "3"); // Lowest score
  }

  #[test]
  fn test_mmr_with_diversity() {
    // Lambda = 0.5 balances relevance and diversity
    let mmr = MMRReRanking::new(0.5);
    let results = vec![
      create_test_result("1", "machine learning algorithms", 0.9),
      create_test_result("2", "machine learning algorithms", 0.85), // Very similar
      create_test_result("3", "deep neural networks", 0.8),         // Different content
    ];

    let reranked = mmr.rerank("query", results).unwrap();
    assert_eq!(reranked.len(), 3);
    assert_eq!(reranked[0].id, "1"); // Highest score always first

    // With diversity, doc3 should rank higher than doc2 because it's different
    // even though doc2 has higher relevance score
    assert_eq!(reranked[1].id, "3");
    assert_eq!(reranked[2].id, "2");
  }

  #[test]
  fn test_mmr_lambda_clamping() {
    let mmr1 = MMRReRanking::new(-0.5); // Should clamp to 0.0
    assert_eq!(mmr1.lambda, 0.0);

    let mmr2 = MMRReRanking::new(1.5); // Should clamp to 1.0
    assert_eq!(mmr2.lambda, 1.0);
  }

  #[test]
  fn test_jaccard_similarity() {
    let mmr = MMRReRanking::new(0.5);

    // Identical texts
    let sim1 = mmr.jaccard_similarity("the quick brown fox", "the quick brown fox");
    assert_eq!(sim1, 1.0);

    // Completely different
    let sim2 = mmr.jaccard_similarity("abc def", "xyz uvw");
    assert_eq!(sim2, 0.0);

    // Partial overlap
    let sim3 = mmr.jaccard_similarity("the quick brown", "the lazy dog");
    assert!(sim3 > 0.0 && sim3 < 1.0);

    // Empty strings
    let sim4 = mmr.jaccard_similarity("", "");
    assert_eq!(sim4, 1.0);
  }

  #[test]
  fn test_score_reranking_descending() {
    let reranker = ScoreReRanking::descending();
    let results = vec![
      create_test_result("1", "content", 0.5),
      create_test_result("2", "other", 0.9),
      create_test_result("3", "another", 0.7),
    ];

    let reranked = reranker.rerank("query", results).unwrap();
    assert_eq!(reranked.len(), 3);
    assert_eq!(reranked[0].id, "2"); // 0.9
    assert_eq!(reranked[1].id, "3"); // 0.7
    assert_eq!(reranked[2].id, "1"); // 0.5
  }

  #[test]
  fn test_score_reranking_ascending() {
    let reranker = ScoreReRanking::ascending();
    let results = vec![
      create_test_result("1", "content", 0.5),
      create_test_result("2", "other", 0.9),
      create_test_result("3", "another", 0.7),
    ];

    let reranked = reranker.rerank("query", results).unwrap();
    assert_eq!(reranked.len(), 3);
    assert_eq!(reranked[0].id, "1"); // 0.5
    assert_eq!(reranked[1].id, "3"); // 0.7
    assert_eq!(reranked[2].id, "2"); // 0.9
  }
}
