//! Access requests: a trusted peer asking to read paths this node does not share. The
//! request is answered at once when the share rules already serve every path, refused
//! when the same peer is already waiting on the same set or on too many, and otherwise
//! held in the inbound store until the person at the keyboard grants or refuses it.

/// Matches `GRANT_MAX_PATHS`: a request for more is a share list by another name.
pub(crate) const ACCESS_MAX_PATHS: usize = 16;
pub(crate) const ACCESS_REASON_MAX_CHARS: usize = 500;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::grants::GRANT_MAX_PATHS;

    #[test]
    fn access_limits_match_the_grant_store() {
        assert_eq!(ACCESS_MAX_PATHS, GRANT_MAX_PATHS);
    }
}
