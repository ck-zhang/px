pub fn canonicalize_spec(spec: &str) -> String {
    let trimmed = strip_wrapping_quotes(spec.trim());
    if trimmed.is_empty() {
        return String::new();
    }
    let mut end = trimmed.len();
    for (idx, ch) in trimmed.char_indices() {
        if ch.is_ascii_whitespace() || matches!(ch, '<' | '>' | '=' | '!' | '~' | ';') {
            end = idx;
            break;
        }
    }
    let head = &trimmed[..end];
    let base = head.split('[').next().unwrap_or(head);
    let suffix = &trimmed[base.len()..];
    let canonical = canonicalize_package_name(base);
    format!("{canonical}{suffix}")
}

pub fn canonicalize_package_name(name: &str) -> String {
    let normalized = name.to_ascii_lowercase().replace(['_', '.'], "-");
    match normalized.as_str() {
        "osgeo" => "gdal".to_string(),
        _ => normalized,
    }
}

pub(crate) fn dependency_name(spec: &str) -> String {
    let trimmed = strip_wrapping_quotes(spec.trim());
    let mut end = trimmed.len();
    for (idx, ch) in trimmed.char_indices() {
        if ch.is_ascii_whitespace() || matches!(ch, '<' | '>' | '=' | '!' | '~' | ';') {
            end = idx;
            break;
        }
    }
    let head = &trimmed[..end];
    let base = head.split('[').next().unwrap_or(head);
    canonicalize_package_name(base)
}

pub(crate) fn strip_wrapping_quotes(input: &str) -> &str {
    if input.len() >= 2 {
        let bytes = input.as_bytes();
        let first = bytes[0];
        let last = bytes[input.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &input[1..input.len() - 1];
        }
    }
    input
}
