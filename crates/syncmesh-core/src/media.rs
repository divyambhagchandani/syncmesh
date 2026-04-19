//! Media-identity matching.
//!
//! Per plan decision 11, two peers are considered to be watching "the same
//! thing" when their `(filename_lower, size_bytes, duration_s)` tuples agree.
//! Disagreement does not block playback — it surfaces a warning so viewers can
//! decide whether to continue.

use crate::protocol::MediaId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchResult {
    /// All three fields match.
    Identical,
    /// Filenames match but size or duration differ — likely a different
    /// release of the same title (different encode, re-edit, etc.).
    SameNameDifferentFile,
    /// Filenames do not match. Peers are watching different things.
    Different,
}

/// Compare two media identities.
pub fn match_media(a: &MediaId, b: &MediaId) -> MatchResult {
    if a == b {
        return MatchResult::Identical;
    }
    if a.filename_lower == b.filename_lower {
        return MatchResult::SameNameDifferentFile;
    }
    MatchResult::Different
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media(name: &str, size: u64, dur: u32) -> MediaId {
        MediaId {
            filename_lower: name.into(),
            size_bytes: size,
            duration_s: dur,
        }
    }

    #[test]
    fn identical_tuples_match() {
        let a = media("movie.mkv", 1000, 60);
        let b = media("movie.mkv", 1000, 60);
        assert_eq!(match_media(&a, &b), MatchResult::Identical);
    }

    #[test]
    fn same_name_different_size_is_soft_match() {
        let a = media("movie.mkv", 1000, 60);
        let b = media("movie.mkv", 2000, 60);
        assert_eq!(match_media(&a, &b), MatchResult::SameNameDifferentFile);
    }

    #[test]
    fn same_name_different_duration_is_soft_match() {
        let a = media("movie.mkv", 1000, 60);
        let b = media("movie.mkv", 1000, 59);
        assert_eq!(match_media(&a, &b), MatchResult::SameNameDifferentFile);
    }

    #[test]
    fn different_names_are_different() {
        let a = media("movie.mkv", 1000, 60);
        let b = media("other.mkv", 1000, 60);
        assert_eq!(match_media(&a, &b), MatchResult::Different);
    }

    #[test]
    fn case_is_not_folded_here() {
        // Callers must lowercase before constructing; this layer trusts them.
        let a = media("Movie.mkv", 1000, 60);
        let b = media("movie.mkv", 1000, 60);
        assert_eq!(match_media(&a, &b), MatchResult::Different);
    }
}
