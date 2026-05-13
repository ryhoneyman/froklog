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
}
