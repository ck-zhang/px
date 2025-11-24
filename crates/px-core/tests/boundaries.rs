use std::fs;

fn file_contains(path: &str, needle: &str) -> bool {
    fs::read_to_string(path)
        .map(|contents| contents.contains(needle))
        .unwrap_or(false)
}

fn dir_contains_rs(dir: &str, needle: &str) -> bool {
    let mut stack = vec![dir.to_string()];
    while let Some(entry) = stack.pop() {
        let path = std::path::PathBuf::from(entry);
        if path.is_dir() {
            if let Ok(read) = fs::read_dir(path) {
                for item in read.flatten() {
                    stack.push(item.path().display().to_string());
                }
            }
        } else if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("rs"))
        {
            if file_contains(path.to_str().unwrap_or_default(), needle) {
                return true;
            }
        }
    }
    false
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

#[test]
fn python_has_no_upward_edges() {
    assert!(
        !dir_contains_rs("crates/px-core/src/core/python", "crate::store"),
        "python must not depend on store"
    );
    assert!(
        !dir_contains_rs("crates/px-core/src/core/python", "crate::runtime"),
        "python must not depend on runtime"
    );
    assert!(
        !dir_contains_rs("crates/px-core/src/core/python", "crate::distribution"),
        "python must not depend on distribution"
    );
}

#[test]
fn store_stays_out_of_distribution() {
    assert!(
        !dir_contains_rs("crates/px-core/src/core/store", "crate::distribution"),
        "store must not depend on distribution"
    );
}

#[test]
fn tooling_stays_out_of_runtime() {
    assert!(
        !dir_contains_rs("crates/px-core/src/core/tooling", "crate::runtime"),
        "tooling must not depend on runtime"
    );
}
