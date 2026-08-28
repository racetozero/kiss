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

    /// Score one candidate. Higher is better; None means no match.
    /// Candidate case folding is streamed, so this does not allocate.
    pub fn score(&self, candidate: &str) -> Option<i64> {
        if let Some(query) = self.ascii.as_deref()
            && candidate.is_ascii()
        {
            return score_ascii(query, candidate.as_bytes());
        }
        if self.chars.is_empty() {
            return Some(0);
        }

        let mut score: i64 = 0;
        let mut query_index = 0usize;
        let mut last_match: Option<usize> = None;
        let mut previous = None;
        let mut candidate_len = 0usize;
        for (candidate_index, candidate_char) in
            candidate.chars().flat_map(char::to_lowercase).enumerate()
        {
            candidate_len = candidate_index + 1;
            if query_index < self.chars.len() && candidate_char == self.chars[query_index] {
                let mut bonus = 1;
                if candidate_index == 0 {
                    bonus += 8; // start of candidate
                } else if previous
                    .is_some_and(|character| matches!(character, '/' | '_' | '-' | '.' | ' '))
                {
                    bonus += 6; // word boundary
                }
                if let Some(last) = last_match
                    && candidate_index == last + 1
                {
                    bonus += 4; // consecutive
                }
                score += bonus;
                last_match = Some(candidate_index);
                query_index += 1;
            }
            previous = Some(candidate_char);
        }
        if query_index < self.chars.len() {
            return None;
        }
        // Prefer shorter candidates.
        score -= (candidate_len as i64) / 8;
        Some(score)
    }

    /// A safe letter-and-digit presence filter for ASCII candidates.
    pub fn required_ascii_mask(&self) -> Option<u64> {
        self.ascii.as_deref().map(ascii_mask)
    }
}

fn score_ascii(query: &[u8], candidate: &[u8]) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let mut score = 0i64;
    let mut query_index = 0usize;
    let mut last_match = None;
    let mut previous = None;
    for (candidate_index, raw) in candidate.iter().copied().enumerate() {
        let byte = raw.to_ascii_lowercase();
        if query_index < query.len() && byte == query[query_index] {
            let mut bonus = 1;
            if candidate_index == 0 {
                bonus += 8;
            } else if previous
                .is_some_and(|previous| matches!(previous, b'/' | b'_' | b'-' | b'.' | b' '))
            {
                bonus += 6;
            }
            if last_match.is_some_and(|last| candidate_index == last + 1) {
                bonus += 4;
            }
            score += bonus;
            last_match = Some(candidate_index);
            query_index += 1;
        }
        previous = Some(byte);
    }
    (query_index == query.len()).then_some(score - candidate.len() as i64 / 8)
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

/// Score `candidate` against `query`. Higher is better; None means no match.
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
