//! BM25 ranking for caller-provided passages, without a database or persistent index.
//!
//! The scorer uses the same hybrid ASCII/identifier and CJK tokenizers as the
//! indexed stores. Scores are relative to the supplied corpus, not comparable
//! across different candidate sets or directly to backend-specific scores.
//!
//! ```
//! use agent_memory::{LexicalDocument, score_lexical_documents};
//!
//! let docs = [
//!     LexicalDocument { index: 4, title: Some("部署失敗"), body: "Check staging logs." },
//!     LexicalDocument { index: 7, title: None, body: "Notes on staging." },
//! ];
//! let hits = score_lexical_documents("部署失敗", &docs, 10);
//! assert_eq!(hits[0].index, 4);
//! assert_eq!(hits.len(), 1);
//! ```

use std::collections::{BTreeMap, HashMap};

use crate::tokenize::{SearchTerm, tokenize_hybrid_search_tokens, tokenize_search_terms};

/// One in-memory document with a caller-assigned index.
#[derive(Clone, Copy, Debug)]
pub struct LexicalDocument<'a> {
    /// Index returned in the hit; use unique values to identify documents.
    pub index: usize,
    /// Optional short title or other searchable metadata.
    pub title: Option<&'a str>,
    /// The passage to search.
    pub body: &'a str,
}

/// Tuning for BM25 and its optional field and phrase boosts.
///
/// Non-finite or out-of-range values fall back to the default for that field.
#[derive(Clone, Copy, Debug)]
pub struct LexicalConfig {
    /// Nonnegative BM25 term frequency saturation parameter (default: 1.2).
    pub k1: f64,
    /// BM25 length normalization strength, in [0, 1] (default: 0.75).
    pub b: f64,
    /// Nonnegative weight on title term frequency (default: 2.0).
    /// Set to zero to ignore titles entirely.
    pub title_boost: f64,
    /// Nonnegative bonus added once per hit if either field contains the
    /// trimmed query, comparing Unicode-lowercased strings (default: 1.0).
    /// Set to zero to disable the bonus.
    pub phrase_boost: f64,
}

impl Default for LexicalConfig {
    fn default() -> Self {
        Self {
            k1: 1.2,
            b: 0.75,
            title_boost: 2.0,
            phrase_boost: 1.0,
        }
    }
}

/// The caller-assigned index and positive relevance score of a matching document.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LexicalHit {
    pub index: usize,
    pub score: f64,
}

/// Rank in-memory passages with the default [`LexicalConfig`].
///
/// Returns no hits for empty or non-searchable queries (including
/// punctuation-only queries), empty corpora, or a zero limit. See
/// [`score_lexical_documents_with_config`] for matching and ordering details.
pub fn score_lexical_documents(
    query: &str,
    docs: &[LexicalDocument<'_>],
    limit: usize,
) -> Vec<LexicalHit> {
    score_lexical_documents_with_config(query, docs, limit, LexicalConfig::default())
}

/// Rank in-memory passages with the supplied [`LexicalConfig`].
///
/// Each distinct term is an alternative match. A multi-character CJK run is
/// one term whose ordered bigrams must occur contiguously in a single field;
/// single CJK characters and ASCII identifier tokens/sub-tokens match on their
/// own. A literal phrase bonus only applies after a term matches.
///
/// BM25 uses the entire supplied corpus for document frequencies and average
/// length. Length counts hybrid tokens in the searched fields without title
/// weighting; term frequency combines body matches with weighted title matches.
/// Results contain only positive scores, ordered by descending score then
/// ascending index, and are truncated to at most `limit`. Empty and
/// non-searchable queries (including punctuation-only queries) return no hits.
pub fn score_lexical_documents_with_config(
    query: &str,
    docs: &[LexicalDocument<'_>],
    limit: usize,
    config: LexicalConfig,
) -> Vec<LexicalHit> {
    if limit == 0 || docs.is_empty() {
        return Vec::new();
    }
    let query = query.trim();
    let mut terms = tokenize_search_terms(query);
    terms.sort_unstable();
    terms.dedup();
    if terms.is_empty() {
        return Vec::new();
    }

    let defaults = LexicalConfig::default();
    let k1 = nonnegative_or(config.k1, defaults.k1);
    let b = if (0.0..=1.0).contains(&config.b) {
        config.b
    } else {
        defaults.b
    };
    let title_boost = nonnegative_or(config.title_boost, defaults.title_boost);
    let phrase_boost = nonnegative_or(config.phrase_boost, defaults.phrase_boost);

    // Index only the query's first tokens. Visit candidate field tokens once,
    // retaining sparse per-document counts rather than tokenized documents.
    let mut starts = TermStarts::new();
    for (i, term) in terms.iter().enumerate() {
        let (first, rest) = match term {
            SearchTerm::Token(token) => (token, &[][..]),
            SearchTerm::CjkPhrase(bigrams) => (&bigrams[0], &bigrams[1..]),
        };
        starts.entry(first).or_default().push((i, rest));
    }

    let mut df = vec![0; terms.len()];
    let mut summed_len = 0.0;
    let mut matching = Vec::new();
    for doc in docs {
        let mut counts = BTreeMap::new();
        let title = doc.title.filter(|_| title_boost > 0.0);
        let title_len = title.map_or(0, |title| {
            count_terms(title, &starts, title_boost, &mut counts)
        });
        let len = title_len + count_terms(doc.body, &starts, 1.0, &mut counts);
        summed_len += len as f64;
        if !counts.is_empty() {
            for &i in counts.keys() {
                df[i] += 1;
            }
            matching.push((doc, title, len, counts));
        }
    }
    if matching.is_empty() {
        return Vec::new();
    }

    let n = docs.len() as f64;
    let avg_len = summed_len / n;
    let idf: Vec<_> = df
        .into_iter()
        .map(|df| {
            let df = df as f64;
            // Positive even when every document (including a singleton) matches.
            ((n - df + 0.5) / (df + 0.5)).ln_1p()
        })
        .collect();
    let phrase = (phrase_boost > 0.0).then(|| query.to_lowercase());
    let mut hits: Vec<_> = matching
        .into_iter()
        .map(|(doc, title, len, counts)| {
            let norm = (1.0 - b) + b * (len as f64 / avg_len);
            let mut score = 0.0;
            for (i, tf) in counts {
                // BM25: divide numerator and denominator by TF * (k1 + 1)
                // without forming that product, which can overflow.
                score += idf[i] / (1.0 / (k1 + 1.0) + (k1 / (k1 + 1.0)) * norm / tf);
            }
            if let Some(phrase) = &phrase
                && (title.is_some_and(|title| title.to_lowercase().contains(phrase))
                    || doc.body.to_lowercase().contains(phrase))
            {
                score += phrase_boost;
            }
            LexicalHit {
                index: doc.index,
                score: score.min(f64::MAX),
            }
        })
        .filter(|hit| hit.score > 0.0)
        .collect();
    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.index.cmp(&b.index))
    });
    hits.truncate(limit);
    hits
}

fn nonnegative_or(value: f64, default: f64) -> f64 {
    if value.is_finite() && value >= 0.0 {
        value
    } else {
        default
    }
}

// First token -> (term index, required suffix). The suffix is empty for
// ordinary tokens and one-bigram phrases.
type TermStarts<'a> = HashMap<&'a str, Vec<(usize, &'a [String])>>;

fn count_terms(
    field: &str,
    starts: &TermStarts<'_>,
    weight: f64,
    counts: &mut BTreeMap<usize, f64>,
) -> usize {
    let tokens = tokenize_hybrid_search_tokens(field);
    for (pos, token) in tokens.iter().enumerate() {
        if let Some(terms) = starts.get(token.as_str()) {
            for &(i, rest) in terms {
                if tokens[pos + 1..].starts_with(rest) {
                    *counts.entry(i).or_default() += weight;
                }
            }
        }
    }
    tokens.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(index: usize, body: &str) -> LexicalDocument<'_> {
        LexicalDocument {
            index,
            title: None,
            body,
        }
    }

    fn bm25_only() -> LexicalConfig {
        LexicalConfig {
            phrase_boost: 0.0,
            ..LexicalConfig::default()
        }
    }

    fn indices(hits: &[LexicalHit]) -> Vec<usize> {
        hits.iter().map(|hit| hit.index).collect()
    }

    fn assert_score(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-12,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn singleton_match_has_positive_bm25_idf() {
        let hits =
            score_lexical_documents_with_config("archive", &[body(7, "archive")], 1, bm25_only());
        assert_eq!(indices(&hits), [7]);
        assert_score(hits[0].score, (4.0_f64 / 3.0).ln());
    }

    #[test]
    fn corpus_idf_prefers_rare_term_at_equal_frequency_and_length() {
        let docs = [
            body(0, "common note"),
            body(1, "rare note"),
            body(2, "common page"),
            body(3, "empty page"),
        ];
        let hits = score_lexical_documents_with_config("common rare", &docs, 4, bm25_only());
        assert_eq!(indices(&hits), [1, 0, 2]);
        // N = 4 includes the non-match. df = 1 for rare, 2 for common.
        assert_score(hits[0].score, (10.0_f64 / 3.0).ln());
        assert_score(hits[1].score, 2.0_f64.ln());
    }

    #[test]
    fn length_normalization_prefers_shorter_document_at_equal_frequency() {
        let docs = [body(0, "garden beside the old barn"), body(1, "garden")];
        let hits = score_lexical_documents_with_config("garden", &docs, 2, bm25_only());
        assert_eq!(indices(&hits), [1, 0]);
        let no_normalization = LexicalConfig {
            b: 0.0,
            ..bm25_only()
        };
        let hits = score_lexical_documents_with_config("garden", &docs, 2, no_normalization);
        assert_eq!(indices(&hits), [0, 1]);
        assert_eq!(hits[0].score, hits[1].score);
    }

    #[test]
    fn repeated_term_frequency_saturates() {
        let docs = [
            body(0, "garden barn barn"),
            body(1, "garden garden barn"),
            body(2, "garden garden garden"),
        ];
        let hits = score_lexical_documents_with_config("garden", &docs, 3, bm25_only());
        assert_eq!(indices(&hits), [2, 1, 0]);
        assert!(hits[0].score - hits[1].score < hits[1].score - hits[2].score);
    }

    #[test]
    fn title_terms_have_higher_weight_than_body_terms() {
        let docs = [
            body(0, "garden notes"),
            LexicalDocument {
                index: 1,
                title: Some("garden"),
                body: "notes",
            },
        ];
        let hits = score_lexical_documents_with_config("garden", &docs, 2, bm25_only());
        assert_eq!(indices(&hits), [1, 0]);
        let equal_fields = LexicalConfig {
            title_boost: 1.0,
            ..bm25_only()
        };
        let hits = score_lexical_documents_with_config("garden", &docs, 2, equal_fields);
        assert_eq!(hits[0].score, hits[1].score);
        let body_only = LexicalConfig {
            title_boost: 0.0,
            ..bm25_only()
        };
        assert_eq!(
            indices(&score_lexical_documents_with_config(
                "garden", &docs, 2, body_only
            )),
            [0]
        );
    }

    #[test]
    fn exact_phrase_is_trimmed_case_insensitive_and_boosted_once() {
        let docs = [
            body(0, "notes garden"),
            body(1, "GARDEN NOTES"),
            LexicalDocument {
                index: 2,
                title: Some("Garden Notes"),
                body: "garden notes",
            },
            LexicalDocument {
                index: 3,
                title: Some("notes garden"),
                body: "notes garden",
            },
            LexicalDocument {
                index: 4,
                title: Some("Garden Notes"),
                body: "notes garden",
            },
        ];
        let query = "  Garden notes\t";
        let config = LexicalConfig::default();
        let boosted = score_lexical_documents(query, &docs, docs.len());
        let plain = score_lexical_documents_with_config(query, &docs, docs.len(), bm25_only());
        assert_eq!(indices(&plain), [2, 3, 4, 0, 1]);
        assert_eq!(indices(&boosted), [2, 4, 1, 3, 0]);
        for hit in boosted {
            let base = plain
                .iter()
                .find(|plain| plain.index == hit.index)
                .unwrap()
                .score;
            let bonus = if [1, 2, 4].contains(&hit.index) {
                config.phrase_boost
            } else {
                0.0
            };
            assert_score(hit.score - base, bonus);
        }
    }

    #[test]
    fn cjk_compound_requires_adjacent_ordered_bigrams_in_one_field() {
        let docs = [
            body(0, "部 ... 署 ... 失 ... 敗"),
            body(1, "署失 部署 失敗"),
            body(2, "部署、失敗"),
            body(3, "重新部署失敗了"),
            LexicalDocument {
                index: 4,
                title: Some("部署失"),
                body: "失敗",
            },
        ];
        let hits = score_lexical_documents_with_config("部署失敗", &docs, 5, bm25_only());
        assert_eq!(indices(&hits), [3]);
        let hits = score_lexical_documents("部", &docs, 5);
        assert!(hits.iter().any(|hit| hit.index == 0));
        assert!(hits.iter().any(|hit| hit.index == 3));
    }

    #[test]
    fn mixed_identifier_matches_whole_token_subtokens_and_cjk() {
        let docs = [
            body(0, "deplOY staging V2"),
            body(1, "Deploy_Staging-v2-部署失敗"),
            body(2, "部署失敗"),
            body(3, "redeploy stagingarea v22"),
            body(4, "foo_bar-v2"),
        ];
        let hits = score_lexical_documents_with_config(
            "DEPLOY_Staging-v2-部署失敗",
            &docs,
            5,
            bm25_only(),
        );
        assert_eq!(hits[0].index, 1);
        let mut present = indices(&hits);
        present.sort_unstable();
        assert_eq!(present, [0, 1, 2, 4]);
        assert_eq!(indices(&score_lexical_documents("bar", &docs, 5)), [4]);
    }

    #[test]
    fn empty_or_non_searchable_inputs_return_no_hits() {
        let docs = [body(0, "garden !!!？！・、 ___--- a"), body(1, "")];
        for query in ["", " \n\t", "!!!？！・、", "___---", "a", "unmatched"] {
            assert!(
                score_lexical_documents(query, &docs, 2).is_empty(),
                "query: {query:?}"
            );
        }
        assert!(score_lexical_documents("garden", &[], 2).is_empty());
    }

    #[test]
    fn ties_use_caller_index_and_limit_only_truncates_results() {
        let docs = [
            body(9, "same note"),
            body(2, "same note"),
            body(4, "unmatched"),
        ];
        let all = score_lexical_documents("note", &docs, usize::MAX);
        assert_eq!(indices(&all), [2, 9]);
        assert_eq!(all[0].score, all[1].score);
        assert_eq!(score_lexical_documents("note", &docs, 1), all[..1]);
        assert!(score_lexical_documents("note", &docs, 0).is_empty());
    }

    #[test]
    fn invalid_config_values_fall_back_to_defaults() {
        let docs = [LexicalDocument {
            index: 0,
            title: Some("garden notes"),
            body: "garden",
        }];
        let invalid = LexicalConfig {
            k1: f64::NAN,
            b: 2.0,
            title_boost: -1.0,
            phrase_boost: f64::INFINITY,
        };
        assert_eq!(
            score_lexical_documents_with_config("garden notes", &docs, 1, invalid),
            score_lexical_documents("garden notes", &docs, 1)
        );
    }
}
