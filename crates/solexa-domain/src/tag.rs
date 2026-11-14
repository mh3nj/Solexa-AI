//! How capability names and knowledge/scope tags are compared.
//!
//! Round 63: they were compared byte-for-byte, but the planner always emits
//! lowercase capabilities while tags and capabilities are free text typed by
//! people, so a specialist created with the capability `Backend`, or
//! knowledge tagged `Backend`, silently never matched a task needing
//! `backend`. Matching is done *at comparison time* (not by rewriting stored
//! values), so existing and synced data is healed too and the user's own
//! spelling is preserved for display.

/// True when two capability names / tags are the same word, ignoring case and
/// surrounding whitespace. A blank tag matches nothing (not even another
/// blank one).
pub fn tags_match(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a == b || a.to_lowercase() == b.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_tags_match() {
        assert!(tags_match("backend", "backend"));
    }

    #[test]
    fn case_differences_do_not_matter_in_either_direction() {
        assert!(tags_match("Backend", "backend"));
        assert!(tags_match("backend", "BACKEND"));
        assert!(tags_match("SQL", "sql"));
    }

    #[test]
    fn surrounding_whitespace_does_not_matter() {
        assert!(tags_match("  backend ", "backend"));
        assert!(tags_match("backend", "\tBackend\n"));
    }

    #[test]
    fn non_ascii_case_is_folded_too() {
        assert!(tags_match("Über", "über"));
        assert!(tags_match("ПРИВЕТ", "привет"));
    }

    #[test]
    fn different_words_do_not_match_and_inner_whitespace_is_significant() {
        // both argument orders: a substring/prefix bug is invisible in one of them
        assert!(!tags_match("backend", "backends"));
        assert!(!tags_match("backends", "backend"));
        assert!(!tags_match("sql", "mysql"));
        assert!(!tags_match("mysql", "sql"));
        assert!(!tags_match("back end", "backend"));
        assert!(!tags_match("sql", "design"));
    }

    #[test]
    fn matching_is_symmetric() {
        for (a, b) in [("Backend", "backend"), (" SQL", "sql "), ("Über", "über"), ("x", "y"), ("", "z")] {
            assert_eq!(tags_match(a, b), tags_match(b, a), "{a:?} vs {b:?}");
        }
    }

    #[test]
    fn a_blank_tag_matches_nothing_not_even_another_blank() {
        assert!(!tags_match("", ""));
        assert!(!tags_match("   ", "  "));
        assert!(!tags_match("", "backend"));
        assert!(!tags_match("backend", " "));
    }
}
