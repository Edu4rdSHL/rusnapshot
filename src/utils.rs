pub fn is_same_character(s: &str, specific_char: char) -> bool {
    s.chars().all(|c| c == specific_char)
}
