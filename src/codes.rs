//! Pairing codes are typed by people, so they are matched without regard to case, dashes or spaces.

pub fn normalize_code(input: &str) -> String {
    input
        .to_uppercase()
        .chars()
        .filter(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        .collect()
}

pub fn format_code(code: &str) -> String {
    let split = code.char_indices().nth(4).map_or(code.len(), |(i, _)| i);
    format!("{}-{}", &code[..split], &code[split..])
}
