//! BM25 (Best Matching 25) keyword search algorithm
//!
//! BM25 is a probabilistic ranking function used for keyword-based search.
//! It considers term frequency, inverse document frequency, and document length
//! normalization to rank documents.

use crate::types::SearchResult;
use std::collections::HashMap;
use std::sync::Mutex;

/// BM25 retriever for keyword-based search
///
/// # Algorithm
/// ```text
/// BM25(D,Q) = Σ IDF(qi) × (f(qi,D) × (k1 + 1)) / (f(qi,D) + k1 × (1 - b + b × |D| / avgdl))
/// ```
///
/// Where:
/// - D = document
/// - Q = query
/// - qi = query terms
/// - f(qi,D) = frequency of term qi in document D
/// - |D| = length of document D in tokens
/// - avgdl = average document length in collection
/// - k1 = term frequency saturation (default: 1.2)
/// - b = length normalization (default: 0.75)
/// - IDF(qi) = log((N - n(qi) + 0.5) / (n(qi) + 0.5))
///
/// # Example
/// ```rust,no_run
/// use yanshi_rag::retrieval::bm25::BM25Retriever;
/// use yanshi_rag::types::SearchResult;
///
/// let mut retriever = BM25Retriever::new();
/// retriever.add_document("doc1", "the quick brown fox");
/// retriever.add_document("doc2", "the lazy brown dog");
///
/// let results = retriever.search("brown fox", 10);
/// ```
pub struct BM25Retriever {
  /// Documents indexed by ID
  documents: HashMap<String, DocumentIndex>,

  /// Total number of documents
  num_docs: usize,

  /// Q3.9.5: derived statistics (IDF table + avg_doc_length) stored
  /// behind a Mutex with a `dirty` flag. `add_document` /
  /// `remove_document` flip the flag in O(1) instead of running a
  /// full O(N) recompute every call; `search` does a lazy recompute
  /// on its first call after the last mutation. This collapses
  /// batch indexing from O(N²) to O(N + N×T_search). Mutex is
  /// chosen over `&mut self` so `search(&self)` stays compatible
  /// with `HybridRetriever::search(&self)`.
  derived: Mutex<DerivedStats>,

  /// Term frequency saturation parameter (typically 1.2-2.0)
  k1: f32,

  /// Length normalization parameter (typically 0.75)
  b: f32,
}

/// Q3.9.5: derived per-corpus statistics computed lazily from
/// `BM25Retriever::documents`. Kept in one struct so the search
/// path only has to grab one lock to read both IDF and the
/// length normaliser.
#[derive(Debug, Default)]
struct DerivedStats {
  idf: HashMap<String, f32>,
  avg_doc_length: f32,
  dirty: bool,
}

/// Document index containing term frequencies and metadata
#[derive(Debug, Clone)]
struct DocumentIndex {
  /// Document ID
  id: String,

  /// Original content
  content: String,

  /// Term frequencies (term -> count)
  term_freq: HashMap<String, usize>,

  /// Document length in tokens
  doc_length: usize,

  /// Document metadata
  metadata: HashMap<String, crate::types::MetadataValue>,
}

impl BM25Retriever {
  /// Create a new BM25 retriever with default parameters
  pub fn new() -> Self {
    Self {
      documents: HashMap::new(),
      num_docs: 0,
      derived: Mutex::new(DerivedStats::default()),
      k1: 1.2,
      b: 0.75,
    }
  }

  /// Create a new BM25 retriever with custom parameters
  ///
  /// # Arguments
  /// * `k1` - Term frequency saturation (typically 1.2-2.0)
  /// * `b` - Length normalization (typically 0.75)
  pub fn with_params(k1: f32, b: f32) -> Self {
    Self {
      documents: HashMap::new(),
      num_docs: 0,
      derived: Mutex::new(DerivedStats::default()),
      k1,
      b: b.clamp(0.0, 1.0),
    }
  }

  /// Tokenize text into lowercase terms
  fn tokenize(text: &str) -> Vec<String> {
    tokenize(text)
  }

  /// Compute term frequencies for a document
  fn compute_term_freq(tokens: &[String]) -> HashMap<String, usize> {
    let mut freq = HashMap::new();
    for token in tokens {
      *freq.entry(token.clone()).or_insert(0) += 1;
    }
    freq
  }

  /// Add a document to the index
  ///
  /// # Arguments
  /// * `id` - Unique document ID
  /// * `content` - Document text content
  pub fn add_document(&mut self, id: impl Into<String>, content: impl Into<String>) {
    self.add_document_with_metadata(id, content, HashMap::new());
  }

  /// Add a document with metadata to the index.
  ///
  /// Q3.9.5: O(1) — only marks the derived stats dirty. The IDF
  /// recompute happens lazily on the next `search()` call. To
  /// pre-warm the index after a batch insert, call
  /// [`Self::finalize`] explicitly (e.g. before serving live
  /// traffic so the first user-visible search isn't slow).
  pub fn add_document_with_metadata(
    &mut self,
    id: impl Into<String>,
    content: impl Into<String>,
    metadata: HashMap<String, crate::types::MetadataValue>,
  ) {
    let id = id.into();
    let content = content.into();
    let tokens = Self::tokenize(&content);
    let term_freq = Self::compute_term_freq(&tokens);
    let doc_length = tokens.len();

    let doc_index = DocumentIndex {
      id: id.clone(),
      content,
      term_freq,
      doc_length,
      metadata,
    };

    self.documents.insert(id, doc_index);
    self.num_docs = self.documents.len();
    self.mark_dirty();
  }

  /// Q3.9.5: convenience batch-insert. Marks the index dirty exactly
  /// once at the end so a 10k-document load is O(N) for ingestion
  /// instead of O(N²). The first `search` after the batch pays for
  /// the recompute; subsequent searches read the cached IDF for
  /// free.
  pub fn add_documents<I, S1, S2>(&mut self, docs: I)
  where
    I: IntoIterator<Item = (S1, S2)>,
    S1: Into<String>,
    S2: Into<String>,
  {
    for (id, content) in docs {
      let id = id.into();
      let content = content.into();
      let tokens = Self::tokenize(&content);
      let term_freq = Self::compute_term_freq(&tokens);
      let doc_length = tokens.len();
      self.documents.insert(
        id.clone(),
        DocumentIndex {
          id,
          content,
          term_freq,
          doc_length,
          metadata: HashMap::new(),
        },
      );
    }
    self.num_docs = self.documents.len();
    self.mark_dirty();
  }

  /// Remove a document from the index. Q3.9.5: O(1) — defers
  /// recompute to next search.
  pub fn remove_document(&mut self, id: &str) -> bool {
    let removed = self.documents.remove(id).is_some();
    if removed {
      self.num_docs = self.documents.len();
      self.mark_dirty();
    }
    removed
  }

  /// Q3.9.5: pre-warm the IDF cache so the first search after a
  /// batch insert doesn't pay the recompute cost on a hot path.
  /// Equivalent to `let _ = self.search("", 0);` but doesn't
  /// allocate a result vector or run the search loop.
  pub fn finalize(&self) {
    // Mutex poison recovery: the derived-stats counter is monotonically
    // rebuilt by `refresh_derived_if_dirty`, so a poisoned guard's inner
    // state is still safe to mutate. Same recovery pattern as
    // `supervisor::blackboard::write_internal` (yanshi-agents Q3.12.2).
    let mut derived = match self.derived.lock() {
      Ok(g) => g,
      Err(poisoned) => poisoned.into_inner(),
    };
    self.refresh_derived_if_dirty(&mut derived);
  }

  fn mark_dirty(&self) {
    if let Ok(mut derived) = self.derived.lock() {
      derived.dirty = true;
    }
  }

  /// Q3.9.5: shared recompute path used by `finalize` and by every
  /// `search` on the first call after a mutation. Caller holds the
  /// mutex; we just refresh the contents in place.
  fn refresh_derived_if_dirty(&self, derived: &mut DerivedStats) {
    if !derived.dirty {
      return;
    }
    if self.documents.is_empty() {
      derived.idf.clear();
      derived.avg_doc_length = 0.0;
      derived.dirty = false;
      return;
    }

    let total_length: usize = self.documents.values().map(|doc| doc.doc_length).sum();
    derived.avg_doc_length = total_length as f32 / self.num_docs as f32;

    let mut doc_freq: HashMap<String, usize> = HashMap::new();
    for doc in self.documents.values() {
      for term in doc.term_freq.keys() {
        *doc_freq.entry(term.clone()).or_insert(0) += 1;
      }
    }

    derived.idf.clear();
    for (term, df) in doc_freq {
      let idf = ((self.num_docs as f32 - df as f32 + 0.5) / (df as f32 + 0.5) + 1.0).ln();
      derived.idf.insert(term, idf);
    }
    derived.dirty = false;
  }

  /// Calculate BM25 score for a document given query terms.
  /// Q3.9.5: takes derived stats by reference so the caller can
  /// hold the mutex once across a batch of `calculate_score` calls
  /// instead of locking per-document.
  fn calculate_score(
    &self,
    doc: &DocumentIndex,
    query_terms: &[String],
    derived: &DerivedStats,
  ) -> f32 {
    let mut score = 0.0;

    for term in query_terms {
      // Get IDF for term (0.0 if term not in corpus)
      let idf = derived.idf.get(term).copied().unwrap_or(0.0);

      // Get term frequency in document
      let tf = doc.term_freq.get(term).copied().unwrap_or(0) as f32;

      if tf > 0.0 {
        // Calculate BM25 component for this term
        let numerator = tf * (self.k1 + 1.0);
        let denominator =
          tf + self.k1 * (1.0 - self.b + self.b * doc.doc_length as f32 / derived.avg_doc_length);

        score += idf * (numerator / denominator);
      }
    }

    score
  }

  /// Search for documents matching the query
  ///
  /// # Arguments
  /// * `query` - Search query text
  /// * `top_k` - Maximum number of results to return
  ///
  /// # Returns
  /// Vector of search results sorted by BM25 score (descending)
  pub fn search(&self, query: &str, top_k: usize) -> Vec<SearchResult> {
    if self.documents.is_empty() {
      return Vec::new();
    }

    let query_terms = Self::tokenize(query);
    if query_terms.is_empty() {
      return Vec::new();
    }

    // Q3.9.5: lazy IDF recompute on first search after a mutation.
    // Holding the mutex across the score loop is fine — this method
    // takes `&self` so concurrent searches serialize at the mutex,
    // not at the recompute (which is a one-time cost).
    // Mutex poison recovery: see `finalize()` above for rationale (Q5.1).
    let mut derived = match self.derived.lock() {
      Ok(g) => g,
      Err(poisoned) => poisoned.into_inner(),
    };
    self.refresh_derived_if_dirty(&mut derived);

    // Calculate scores for all documents
    let mut scores: Vec<(String, f32)> = self
      .documents
      .values()
      .map(|doc| {
        let score = self.calculate_score(doc, &query_terms, &derived);
        (doc.id.clone(), score)
      })
      .filter(|(_, score)| *score > 0.0) // Only return documents with non-zero score
      .collect();

    // Sort by score descending
    scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Take top k and convert to SearchResult
    scores
      .into_iter()
      .take(top_k)
      .filter_map(|(id, score)| {
        self.documents.get(&id).map(|doc| SearchResult {
          id: doc.id.clone(),
          content: doc.content.clone(),
          score,
          metadata: doc.metadata.clone(),
        })
      })
      .collect()
  }

  /// Get the number of indexed documents
  pub fn num_documents(&self) -> usize {
    self.num_docs
  }

  /// Clear all documents from the index
  pub fn clear(&mut self) {
    self.documents.clear();
    self.num_docs = 0;
    if let Ok(mut derived) = self.derived.lock() {
      derived.idf.clear();
      derived.avg_doc_length = 0.0;
      derived.dirty = false;
    }
  }
}

impl Default for BM25Retriever {
  fn default() -> Self {
    Self::new()
  }
}

/// Han ideographs and Japanese kana: scripts written without spaces
/// between words. Hangul is space-delimited and stays a word character.
fn is_unspaced_script(c: char) -> bool {
  matches!(c,
    '\u{3040}'..='\u{30FF}'
    | '\u{3400}'..='\u{4DBF}'
    | '\u{4E00}'..='\u{9FFF}'
    | '\u{F900}'..='\u{FAFF}'
    | '\u{20000}'..='\u{2A6DF}')
}

/// Lowercased terms for indexing and querying. Whitespace-separated words
/// keep their inner punctuation; runs of Han/kana, which have no spaces to
/// split on, become overlapping character bigrams (a lone character stays
/// a unigram), so "人工智能" matches inside "机器学习是人工智能的一个分支".
pub(crate) fn tokenize(text: &str) -> Vec<String> {
  let mut tokens = Vec::new();
  for word in text.to_lowercase().split_whitespace() {
    if !word.chars().any(is_unspaced_script) {
      let word = word.trim_matches(|c: char| !c.is_alphanumeric());
      if !word.is_empty() {
        tokens.push(word.to_string());
      }
      continue;
    }
    let mut segment = String::new();
    let mut run: Vec<char> = Vec::new();
    for c in word.chars() {
      if is_unspaced_script(c) {
        push_segment(&mut tokens, &mut segment);
        run.push(c);
      } else {
        push_bigrams(&mut tokens, &mut run);
        if c.is_alphanumeric() {
          segment.push(c);
        } else {
          push_segment(&mut tokens, &mut segment);
        }
      }
    }
    push_segment(&mut tokens, &mut segment);
    push_bigrams(&mut tokens, &mut run);
  }
  tokens
}

fn push_segment(tokens: &mut Vec<String>, segment: &mut String) {
  if !segment.is_empty() {
    tokens.push(std::mem::take(segment));
  }
}

fn push_bigrams(tokens: &mut Vec<String>, run: &mut Vec<char>) {
  match run.len() {
    0 => {}
    1 => tokens.push(run[0].to_string()),
    _ => tokens.extend(run.windows(2).map(|pair| pair.iter().collect::<String>())),
  }
  run.clear();
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn chinese_queries_match_chinese_documents() {
    let mut retriever = BM25Retriever::new();
    retriever.add_document("ml", "机器学习是人工智能的一个分支。");
    retriever.add_document("weather", "今天天气很好，适合出门散步。");
    retriever.add_document("mixed", "Rust 语言的所有权模型");

    let results = retriever.search("人工智能", 3);
    assert_eq!(
      results.first().map(|r| r.id.as_str()),
      Some("ml"),
      "{results:?}"
    );
    assert!(results.iter().all(|r| r.id != "weather"), "{results:?}");

    let results = retriever.search("rust 所有权", 3);
    assert_eq!(
      results.first().map(|r| r.id.as_str()),
      Some("mixed"),
      "{results:?}"
    );
  }

  #[test]
  fn test_tokenize() {
    let tokens = BM25Retriever::tokenize("The quick, brown fox!");
    assert_eq!(tokens, vec!["the", "quick", "brown", "fox"]);

    let tokens2 = BM25Retriever::tokenize("  spaces   everywhere  ");
    assert_eq!(tokens2, vec!["spaces", "everywhere"]);

    let tokens3 = BM25Retriever::tokenize("");
    assert_eq!(tokens3.len(), 0);

    assert_eq!(tokenize("人工智能"), vec!["人工", "工智", "智能"]);
    assert_eq!(tokenize("用Rust写，好"), vec!["用", "rust", "写", "好"]);
    assert_eq!(
      tokenize("e-mail"),
      vec!["e-mail"],
      "non-CJK words unchanged"
    );
  }

  #[test]
  fn test_add_document() {
    let mut retriever = BM25Retriever::new();
    assert_eq!(retriever.num_documents(), 0);

    retriever.add_document("doc1", "the quick brown fox");
    assert_eq!(retriever.num_documents(), 1);

    retriever.add_document("doc2", "the lazy dog");
    assert_eq!(retriever.num_documents(), 2);
  }

  #[test]
  fn test_remove_document() {
    let mut retriever = BM25Retriever::new();
    retriever.add_document("doc1", "content");
    retriever.add_document("doc2", "other");

    assert!(retriever.remove_document("doc1"));
    assert_eq!(retriever.num_documents(), 1);

    assert!(!retriever.remove_document("nonexistent"));
    assert_eq!(retriever.num_documents(), 1);
  }

  #[test]
  fn test_simple_search() {
    let mut retriever = BM25Retriever::new();
    retriever.add_document("doc1", "the quick brown fox jumps over the lazy dog");
    retriever.add_document("doc2", "the brown fox is quick and clever");
    retriever.add_document("doc3", "the lazy dog sleeps all day");

    let results = retriever.search("brown fox", 10);
    assert!(results.len() >= 2);

    // Both doc1 and doc2 contain "brown fox"
    // doc2 should score higher because it has shorter length
    assert_eq!(results[0].id, "doc2");
    assert!(results[0].score > 0.0);
  }

  #[test]
  fn test_search_with_limit() {
    let mut retriever = BM25Retriever::new();
    retriever.add_document("doc1", "machine learning");
    retriever.add_document("doc2", "deep learning");
    retriever.add_document("doc3", "machine intelligence");

    let results = retriever.search("machine learning", 2);
    assert_eq!(results.len(), 2);
  }

  #[test]
  fn test_search_no_matches() {
    let mut retriever = BM25Retriever::new();
    retriever.add_document("doc1", "machine learning algorithms");
    retriever.add_document("doc2", "deep neural networks");

    let results = retriever.search("quantum computing", 10);
    assert_eq!(results.len(), 0);
  }

  #[test]
  fn test_search_empty_query() {
    let mut retriever = BM25Retriever::new();
    retriever.add_document("doc1", "content");

    let results = retriever.search("", 10);
    assert_eq!(results.len(), 0);
  }

  #[test]
  fn test_search_empty_index() {
    let retriever = BM25Retriever::new();
    let results = retriever.search("query", 10);
    assert_eq!(results.len(), 0);
  }

  #[test]
  fn test_clear() {
    let mut retriever = BM25Retriever::new();
    retriever.add_document("doc1", "content");
    retriever.add_document("doc2", "other");

    retriever.clear();
    assert_eq!(retriever.num_documents(), 0);

    let results = retriever.search("content", 10);
    assert_eq!(results.len(), 0);
  }

  #[test]
  fn test_custom_parameters() {
    let retriever1 = BM25Retriever::with_params(2.0, 0.5);
    assert_eq!(retriever1.k1, 2.0);
    assert_eq!(retriever1.b, 0.5);

    // Test clamping of b parameter
    let retriever2 = BM25Retriever::with_params(1.5, 1.5);
    assert_eq!(retriever2.b, 1.0); // Should be clamped to 1.0
  }

  #[test]
  fn test_term_frequency() {
    let tokens = vec!["the".to_string(), "quick".to_string(), "the".to_string()];
    let tf = BM25Retriever::compute_term_freq(&tokens);

    assert_eq!(*tf.get("the").unwrap(), 2);
    assert_eq!(*tf.get("quick").unwrap(), 1);
  }

  #[test]
  fn test_idf_calculation() {
    let mut retriever = BM25Retriever::new();
    retriever.add_document("doc1", "the quick brown fox");
    retriever.add_document("doc2", "the lazy dog");
    retriever.add_document("doc3", "quick brown fox");

    // Q3.9.5: IDF is now lazily computed; pre-warm via finalize().
    retriever.finalize();
    let derived = retriever.derived.lock().unwrap();

    // "the" appears in 2 out of 3 documents
    let idf_the = derived.idf.get("the").copied().unwrap();
    assert!(idf_the > 0.0);

    // "fox" appears in 2 out of 3 documents
    let idf_fox = derived.idf.get("fox").copied().unwrap();
    assert!(idf_fox > 0.0);

    // "lazy" appears in 1 out of 3 documents (more rare, higher IDF)
    let idf_lazy = derived.idf.get("lazy").copied().unwrap();
    assert!(idf_lazy > idf_the);
  }

  #[test]
  fn test_document_length_normalization() {
    let mut retriever = BM25Retriever::new();

    // Short document with query term
    retriever.add_document("short", "machine learning");

    // Long document with same query term
    retriever.add_document(
      "long",
      "machine learning is a fascinating field of artificial intelligence that involves algorithms and statistical models",
    );

    let results = retriever.search("machine learning", 10);

    // Short document should score higher due to length normalization
    assert_eq!(results[0].id, "short");
    assert!(results[0].score > results[1].score);
  }

  /// Q3.9.5 — `add_document` must NOT recompute IDF eagerly. The
  /// derived stats stay `dirty = true` until the next `search`
  /// (or explicit `finalize`) triggers a one-shot recompute.
  #[test]
  fn add_document_defers_idf_recompute() {
    let mut retriever = BM25Retriever::new();
    retriever.add_document("doc1", "alpha beta gamma");
    retriever.add_document("doc2", "alpha delta");
    retriever.add_document("doc3", "beta gamma");
    // Right after add_document the derived stats must be empty + dirty.
    {
      let derived = retriever.derived.lock().unwrap();
      assert!(
        derived.dirty,
        "derived stats must be marked dirty after add_document"
      );
      assert!(
        derived.idf.is_empty(),
        "IDF must NOT be recomputed eagerly; got {:?}",
        derived.idf
      );
      assert_eq!(derived.avg_doc_length, 0.0);
    }
    // First search triggers the lazy recompute.
    let _ = retriever.search("alpha", 10);
    {
      let derived = retriever.derived.lock().unwrap();
      assert!(!derived.dirty, "first search must clear the dirty flag");
      assert!(
        !derived.idf.is_empty(),
        "IDF must be populated after first search"
      );
      assert!(derived.avg_doc_length > 0.0);
    }
  }

  /// Q3.9.5 — `add_documents` batch convenience must mark dirty
  /// exactly once at the end (vs. N times for N individual adds),
  /// so batch ingestion is O(N) instead of O(N²).
  #[test]
  fn add_documents_batch_marks_dirty_once() {
    let mut retriever = BM25Retriever::new();
    retriever.add_documents([
      ("doc1", "the quick brown fox"),
      ("doc2", "the lazy dog"),
      ("doc3", "quick brown fox"),
    ]);
    {
      let derived = retriever.derived.lock().unwrap();
      assert!(derived.dirty);
      assert!(derived.idf.is_empty());
    }
    assert_eq!(retriever.num_documents(), 3);
    // After search, IDF for "the" is populated.
    let results = retriever.search("the", 10);
    assert_eq!(results.len(), 2);
  }

  /// Q3.9.5 — `finalize()` pre-warms the IDF without running a
  /// search. Useful for hot-path serving where the first user
  /// request shouldn't pay the recompute cost.
  #[test]
  fn finalize_prewarms_idf() {
    let mut retriever = BM25Retriever::new();
    retriever.add_document("doc1", "alpha beta");
    retriever.add_document("doc2", "alpha gamma");
    retriever.finalize();
    let derived = retriever.derived.lock().unwrap();
    assert!(!derived.dirty);
    assert!(derived.idf.contains_key("alpha"));
    assert!(derived.idf.contains_key("beta"));
    assert!(derived.idf.contains_key("gamma"));
    assert!(derived.avg_doc_length > 0.0);
  }

  /// Q3.9.5 — `remove_document` marks dirty without recomputing.
  #[test]
  fn remove_document_defers_recompute() {
    let mut retriever = BM25Retriever::new();
    retriever.add_document("doc1", "alpha");
    retriever.add_document("doc2", "beta");
    retriever.finalize();
    // Confirm not dirty after finalize.
    assert!(!retriever.derived.lock().unwrap().dirty);
    retriever.remove_document("doc1");
    assert!(
      retriever.derived.lock().unwrap().dirty,
      "remove_document must mark dirty"
    );
    let _ = retriever.search("beta", 10);
    assert!(!retriever.derived.lock().unwrap().dirty);
  }
}
