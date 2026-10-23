//! Query tokenisation shared by search and duplicate detection.

use std::collections::BTreeSet;

/// English stopwords plus words every memory contains ("user").
const STOPWORDS: &[&str] = &[
    "a", "about", "above", "after", "again", "against", "all", "am", "an", "and", "any", "are", "as", "at", "be",
    "because", "been", "before", "being", "below", "between", "both", "but", "by", "can", "could", "did", "do",
    "does", "doing", "down", "during", "each", "few", "for", "from", "further", "had", "has", "have", "having", "he",
    "her", "here", "hers", "herself", "him", "himself", "his", "how", "i", "if", "in", "into", "is", "it", "its",
    "itself", "just", "know", "me", "more", "most", "my", "myself", "no", "nor", "not", "now", "of", "off", "on",
    "once", "only", "or", "other", "our", "ours", "ourselves", "out", "over", "own", "same", "she", "should", "so",
    "some", "such", "than", "that", "the", "their", "theirs", "them", "themselves", "then", "there", "these", "they",
    "this", "those", "through", "to", "too", "under", "until", "up", "very", "was", "we", "were", "what", "when",
    "where", "which", "while", "who", "whom", "why", "will", "with", "would", "you", "your", "yours", "yourself",
    "user", "users", "s", "t", "anything", "something", "tell", "any", "get", "got", "also", "like", "let",
];

/// Lowercased content words with stopwords removed, in order, deduplicated.
pub fn words(s: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .filter(|w| !STOPWORDS.contains(&w.as_str()))
        .filter(|w| seen.insert(w.clone()))
        .collect()
}

pub fn word_set(s: &str) -> BTreeSet<String> {
    words(s).into_iter().collect()
}

/// Build a safe FTS5 MATCH expression. Every term is quoted, so user input
/// can never inject FTS operators.
pub fn fts_query(s: &str, any: bool) -> Option<String> {
    let w = words(s);
    if w.is_empty() {
        return None;
    }
    let joiner = if any { " OR " } else { " " };
    Some(w.iter().map(|t| format!("\"{t}\"")).collect::<Vec<_>>().join(joiner))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_queries() {
        assert_eq!(fts_query("What does the user eat?", true).as_deref(), Some("\"eat\""));
        assert_eq!(fts_query("dark-mode \"OR\" NEAR(", true).as_deref(), Some("\"dark\" OR \"mode\" OR \"near\""));
        assert_eq!(fts_query("the of and", true), None);
    }
}
