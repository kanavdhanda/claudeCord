//! Pairing codes are typed by people, so they are matched without regard to case, dashes or spaces.

/// Cleans up a typed pairing code: upper case, with dashes, spaces and anything else that is not a letter or digit removed.
pub fn normalize_code(input: &str) -> String {
    input
        .to_uppercase()
        .chars()
        .filter(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        .collect()
}

/// Shows a pairing code in groups so it is easy to read and type.
pub fn format_code(code: &str) -> String {
    let split = code.char_indices().nth(4).map_or(code.len(), |(i, _)| i);
    format!("{}-{}", &code[..split], &code[split..])
}
