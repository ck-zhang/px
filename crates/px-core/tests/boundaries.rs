use std::fs;

fn file_contains(path: &str, needle: &str) -> bool {
    fs::read_to_string(path)
        .map(|contents| contents.contains(needle))
        .unwrap_or(false)
}

#[test]
fn store_has_no_runtime_back_edges() {
    assert!(
        !file_contains("crates/px-core/src/core/store/mod.rs", "crate::runtime"),
        "store must not depend on runtime modules"
    );
    assert!(
        !file_contains(
            "crates/px-core/src/core/store/mod.rs",
            "crate::runtime_manager"
        ),
        "store must not depend on runtime manager"
    );
}

#[test]
fn distribution_stays_out_of_runtime() {
    let distro_files = [
        "crates/px-core/src/core/distribution/mod.rs",
        "crates/px-core/src/core/distribution/build.rs",
        "crates/px-core/src/core/distribution/publish.rs",
        "crates/px-core/src/core/distribution/plan.rs",
        "crates/px-core/src/core/distribution/artifacts.rs",
    ];
    for path in distro_files {
        assert!(
            !file_contains(path, "crate::run"),
            "distribution should not depend on runtime planning: {path}"
        );
        assert!(
            !file_contains(path, "crate::runtime"),
            "distribution should not depend on runtime modules: {path}"
        );
    }
}
