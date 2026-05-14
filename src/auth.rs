use uuid::Uuid;

/// Generate a cryptographically random token (UUID v4, 32 lowercase hex chars).
pub fn generate_token() -> String {
    Uuid::new_v4().simple().to_string()
}

/// Constant-time string comparison to prevent timing attacks on token validation.
/// Returns true only when both strings are equal in length and content.
pub fn tokens_match(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.as_bytes().iter().zip(b.as_bytes().iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_length() {
        assert_eq!(generate_token().len(), 32);
    }

    #[test]
    fn token_match() {
        let t = generate_token();
        assert!(tokens_match(&t, &t));
        assert!(!tokens_match(&t, "wrong"));
    }
    #[test]
    fn token_all_lowercase_hex() {
        let t = generate_token();
        assert!(
            t.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "token: {t}"
        );
    }
    #[test]
    fn tokens_match_same_length_different_content() {
        // Constant-time comparison: same length but different bytes must return false
        let a = "a".repeat(32);
        let b = "b".repeat(32);
        assert!(!tokens_match(&a, &b));
    }
    #[test]
    fn tokens_match_different_lengths() {
        assert!(!tokens_match("short", "longer_string"));
    }
    #[test]
    fn tokens_match_empty_both() {
        assert!(tokens_match("", ""));
    }
    #[test]
    fn tokens_match_empty_vs_nonempty() {
        assert!(!tokens_match("", "x"));
    }
    #[test]
    fn tokens_match_single_bit_difference() {
        // Off-by-one in last byte
        assert!(!tokens_match(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaab",
        ));
    }
}
