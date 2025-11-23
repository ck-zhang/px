use std::fs;

fn file_contains(path: &str, needle: &str) -> bool {
    fs::read_to_string(path)
        .map(|contents| contents.contains(needle))
        .unwrap_or(false)
}

#[test]
fn store_has_no_runtime_back_edges() {
    assert!(
        !file_contains("crates/px-core/src/store/mod.rs", "crate::runtime"),
        "store must not depend on runtime modules"
    );
    assert!(
        !file_contains("crates/px-core/src/store/mod.rs", "crate::runtime_manager"),
        "store must not depend on runtime manager"
    );
}
