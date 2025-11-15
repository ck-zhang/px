use assert_cmd::cargo::cargo_bin_cmd;

fn help_output(args: &[&str]) -> String {
    let assert = cargo_bin_cmd!("px").args(args).assert().success();
    String::from_utf8(assert.get_output().stdout.clone()).expect("utf8 help")
}

#[test]
fn run_help_mentions_usage_and_examples() {
    let output = help_output(&["run", "--help"]);
    assert!(
        output.contains("Run the inferred entry or a named module inside px."),
        "run help missing about: {output}"
    );
    assert!(
        output.contains("px run [ENTRY] [-- <ARG>...]")
            || output.contains("px run [entry] [-- <arg>...]")
    );
    assert!(
        output.contains("px run sample_px_app.cli -- -n Demo"),
        "run example missing entry override: {output}"
    );
}

#[test]
fn project_init_help_lists_examples() {
    let output = help_output(&["project", "init", "--help"]);
    assert!(
        output.contains("Scaffold pyproject, src/, and tests using the current folder."),
        "init about missing: {output}"
    );
    assert!(
        output.contains("px project init [--package NAME] [--py VERSION]")
            || output.contains("px project init [--package name] [--py version]")
    );
    assert!(
        output.contains("px project init --package demo_pkg --py 3.11"),
        "init example missing override: {output}"
    );
}

#[test]
fn env_help_highlights_modes() {
    let output = help_output(&["env", "--help"]);
    assert!(
        output.contains("px env [python|info|paths]"),
        "env usage missing modes: {output}"
    );
    assert!(
        output.contains("px env python"),
        "env example missing python shim: {output}"
    );
}

#[test]
fn cache_prune_help_mentions_dry_run_example() {
    let output = help_output(&["cache", "prune", "--help"]);
    assert!(
        output.contains("Prune cache files (pair with --dry-run to preview)."),
        "cache prune about missing: {output}"
    );
    assert!(
        output.contains("px cache prune --all --dry-run"),
        "cache prune example missing: {output}"
    );
}

#[test]
fn store_prefetch_help_shows_workspace_example() {
    let output = help_output(&["store", "prefetch", "--help"]);
    assert!(
        output.contains("Hydrate lock artifacts into the cache (workspace optional)."),
        "store prefetch about missing: {output}"
    );
    assert!(
        output.contains("PX_ONLINE=1 px store prefetch --workspace"),
        "store prefetch example missing gating note: {output}"
    );
}

#[test]
fn build_help_mentions_skip_tests_example() {
    let output = help_output(&["build", "--help"]);
    assert!(
        output.contains("Build sdists and wheels into the project build/ folder."),
        "build about missing: {output}"
    );
    assert!(
        output.contains("PX_SKIP_TESTS=1 px build"),
        "build example missing skip-tests hint: {output}"
    );
}
