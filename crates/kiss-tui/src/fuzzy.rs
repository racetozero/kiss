//! Subsequence fuzzy matcher for @file and /command completion.

/// A query that is lowercased and split once for repeated candidate scoring.
#[derive(Debug, Clone)]
pub struct PreparedFuzzyQuery {
    chars: Vec<char>,
    ascii: Option<Vec<u8>>,
}

impl PreparedFuzzyQuery {
    pub fn new(query: &str) -> Self {
        Self {
            chars: query.to_lowercase().chars().collect(),
            ascii: query.is_ascii().then(|| {
                query
                    .bytes()
                    .map(|byte| byte.to_ascii_lowercase())
                    .collect()
            }),
        }
    }

    /// Score one candidate. Higher is better. None means no match.
    /// Candidate case folding is streamed, so this does not allocate.
    pub fn score(&self, candidate: &str) -> Option<i64> {
        if let Some(query) = self.ascii.as_deref()
            && candidate.is_ascii()
        {
            return score_ascii(query, candidate.as_bytes());
        }
        let candidate: Vec<char> = candidate.chars().flat_map(char::to_lowercase).collect();
        best_alignment(
            &self.chars,
            &candidate,
            |character| character,
            |character| matches!(character, '/' | '_' | '-' | '.' | ' '),
        )
    }

    /// A safe letter-and-digit presence filter for ASCII candidates.
    pub fn required_ascii_mask(&self) -> Option<u64> {
        self.ascii.as_deref().map(ascii_mask)
    }
}

fn score_ascii(query: &[u8], candidate: &[u8]) -> Option<i64> {
    best_alignment(
        query,
        candidate,
        |byte| byte.to_ascii_lowercase(),
        |byte| matches!(byte, b'/' | b'_' | b'-' | b'.' | b' '),
    )
}

/// Score `query` against `candidate`, both already split into characters.
/// `lower` folds a candidate character to match the lowercase query.
fn best_alignment<T: Copy + PartialEq>(
    query: &[T],
    candidate: &[T],
    lower: impl Fn(T) -> T,
    boundary: impl Fn(T) -> bool,
) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let greedy_from = |start: usize| {
        let mut score = 0i64;
        let mut query_index = 0usize;
        let mut last_match = None;
        let mut previous = start.checked_sub(1).map(|index| lower(candidate[index]));
        for (candidate_index, raw) in candidate.iter().copied().enumerate().skip(start) {
            let item = lower(raw);
            if query_index < query.len() && item == query[query_index] {
                let mut bonus = 1;
                if candidate_index == 0 {
                    bonus += 8; // start of candidate
                } else if previous.is_some_and(&boundary) {
                    bonus += 6; // word boundary
                }
                if last_match.is_some_and(|last| candidate_index == last + 1) {
                    bonus += 4; // consecutive
                }
                score += bonus;
                last_match = Some(candidate_index);
                query_index += 1;
            }
            previous = Some(item);
        }
        (query_index == query.len()).then_some(score)
    };
    // The greedy alignment can scatter the query over an earlier, unrelated
    // part of the candidate (`opus` in `openrouter/...`), so also score each
    // contiguous occurrence and keep the best alignment.
    let contiguous = (1..=candidate.len().saturating_sub(query.len())).filter(|&start| {
        candidate[start..start + query.len()]
            .iter()
            .zip(query)
            .all(|(item, query)| lower(*item) == *query)
    });
    std::iter::once(0)
        .chain(contiguous)
        .filter_map(greedy_from)
        .max()
        // Prefer shorter candidates.
        .map(|score| score - candidate.len() as i64 / 8)
}

fn ascii_mask(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0u64, |mask, byte| {
        let lower = byte.to_ascii_lowercase();
        let bit = match lower {
            b'a'..=b'z' => Some(lower - b'a'),
            b'0'..=b'9' => Some(26 + lower - b'0'),
            _ => None,
        };
        bit.map_or(mask, |bit| mask | (1u64 << bit))
    })
}

/// Score `candidate` against `query`. Higher is better. None means no match.
/// Simple subsequence scoring with bonuses for prefix, word-boundary, and
/// consecutive matches.
pub fn fuzzy_score(query: &str, candidate: &str) -> Option<i64> {
    PreparedFuzzyQuery::new(query).score(candidate)
}

/// Rank candidates by score, best first, dropping non-matches.
pub fn fuzzy_rank<'a>(
    query: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Vec<(&'a str, i64)> {
    let query = PreparedFuzzyQuery::new(query);
    let mut out: Vec<(&str, i64)> = candidates
        .into_iter()
        .filter_map(|candidate| query.score(candidate).map(|score| (candidate, score)))
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.len().cmp(&b.0.len())));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_boundary_matches_higher() {
        let ranked = fuzzy_rank("sm", ["src/main.rs", "somefile.txt"]);
        assert_eq!(ranked[0].0, "src/main.rs");
    }

    #[test]
    fn contiguous_matches_beat_letters_scattered_over_an_earlier_part() {
        let ranked = fuzzy_rank(
            "opus",
            [
                "openrouter/openai/gpt-oss-120b",
                "github-copilot/claude-sonnet-4.5",
                "anthropic/claude-opus-4.1",
            ],
        );
        assert_eq!(ranked[0].0, "anthropic/claude-opus-4.1");
        let ranked = fuzzy_rank("astra", ["radius/gpt-6-sol", "openai/gpt-6-astra"]);
        assert_eq!(ranked[0].0, "openai/gpt-6-astra");
        let ranked = fuzzy_rank("ÖL", ["öffentlich/datei-l", "src/öl.rs"]);
        assert_eq!(ranked[0].0, "src/öl.rs");
    }

    #[test]
    fn non_match_dropped() {
        assert!(fuzzy_score("xyz", "abc").is_none());
        assert!(fuzzy_score("ab", "acb").is_some());
    }

    #[test]
    fn prepared_query_is_case_insensitive_without_candidate_allocation() {
        let query = PreparedFuzzyQuery::new("ÄR");
        assert!(query.score("src/ärger.rs").is_some());
        assert!(query.score("src/main.rs").is_none());
    }

    #[test]
    fn ascii_fast_path_matches_unicode_path_scores() {
        for query in ["sm", "MAIN", "42", "src.rs", ""] {
            let prepared = PreparedFuzzyQuery::new(query);
            let lowered = query.to_lowercase().chars().collect::<Vec<_>>();
            let unicode_only = PreparedFuzzyQuery {
                chars: lowered,
                ascii: None,
            };
            for candidate in ["src/main.rs", "some_file-42.rs", "README.md", ""] {
                assert_eq!(prepared.score(candidate), unicode_only.score(candidate));
            }
        }
    }
}
