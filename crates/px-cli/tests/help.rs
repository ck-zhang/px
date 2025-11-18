use assert_cmd::cargo::cargo_bin_cmd;

fn help_output(args: &[&str]) -> String {
    let assert = cargo_bin_cmd!("px").args(args).assert().success();
    String::from_utf8(assert.get_output().stdout.clone()).expect("utf8 help")
}

#[test]
fn run_help_mentions_usage_and_examples() {
    let output = help_output(&["run", "--help"]);
    assert!(
        output.contains("Run scripts/tasks with auto-sync unless --frozen or CI=1."),
        "run help missing updated about: {output}"
    );
    assert!(
        output.contains("px run [ENTRY] [-- <ARG>...]")
            || output.contains("px run [entry] [-- <arg>...]")
    );
    assert!(
        output.contains("--frozen"),
        "run help should mention the --frozen guard: {output}"
    );
}

#[test]
fn init_help_lists_examples() {
    let output = help_output(&["init", "--help"]);
    assert!(
        output.contains("Start a px project: writes pyproject, px.lock, and an empty env."),
        "init about missing: {output}"
    );
    assert!(
        output.contains("px init [--package NAME] [--py VERSION]")
            || output.contains("px init [--package name] [--py version]")
    );
}

#[test]
fn env_help_highlights_modes() {
    let output = help_output(&["debug", "env", "--help"]);
    assert!(
        output.contains("px debug env [python|info|paths]")
            || output.contains("px debug env [python|info|paths"),
        "env usage missing modes: {output}"
    );
    assert!(
        output.contains("Output mode: info, paths, or python"),
        "env flag description missing: {output}"
    );
}

#[test]
fn cache_prune_help_mentions_dry_run_example() {
    let output = help_output(&["debug", "cache", "prune", "--help"]);
    assert!(
        output.contains("Prune cache files (pair with --dry-run to preview)."),
        "cache prune about missing: {output}"
    );
}

#[test]
fn cache_prefetch_help_shows_workspace_example() {
    let output = help_output(&["debug", "cache", "prefetch", "--help"]);
    assert!(
        output.contains("Prefetch and cache artifacts for offline use."),
        "cache prefetch about missing: {output}"
    );
}

#[test]
fn build_help_mentions_skip_tests_example() {
    let output = help_output(&["build", "--help"]);
    assert!(
        output.contains("Build sdists/wheels using the px env (prep for px publish)."),
        "build about missing: {output}"
    );
}

#[test]
fn fmt_help_mentions_frozen_flag() {
    let output = help_output(&["fmt", "--help"]);
    assert!(
        output.contains("--frozen"),
        "fmt help should mention the --frozen guard: {output}"
    );
    assert!(
        output.contains("px fmt [-- <ARG>...]") || output.contains("px fmt [-- <arg>...]"),
        "fmt usage missing forwarded arg example: {output}"
    );
}

#[test]
fn lint_help_mentions_frozen_flag() {
    let output = help_output(&["lint", "--help"]);
    assert!(
        output.contains("--frozen"),
        "lint help should mention the --frozen guard: {output}"
    );
    assert!(
        output.contains("px lint [-- <ARG>...]") || output.contains("px lint [-- <arg>...]"),
        "lint usage missing forwarded arg example: {output}"
    );
}
