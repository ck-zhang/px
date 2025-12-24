use assert_cmd::cargo::cargo_bin_cmd;

fn help_output(args: &[&str]) -> String {
    let assert = cargo_bin_cmd!("px").args(args).assert().success();
    String::from_utf8(assert.get_output().stdout.clone()).expect("utf8 help")
}

#[test]
fn run_help_mentions_usage_and_examples() {
    let output = help_output(&["run", "--help"]);
    assert!(
        output.contains("auto-repair the env from px.lock unless --frozen or CI=1"),
        "run help missing updated about: {output}"
    );
    assert!(
        output.contains("px run <TARGET> [ARG...]") || output.contains("px run <target> [arg...]")
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
fn add_help_mentions_manifest_pinning() {
    let output = help_output(&["add", "--help"]);
    assert!(
        output.to_ascii_lowercase().contains("ranges/unpinned")
            && output.to_ascii_lowercase().contains("px.lock pins")
            && output.contains("--pin"),
        "add help should mention default range semantics and the --pin flag, got: {output}"
    );
}

#[test]
fn remove_help_uses_name_argument() {
    let output = help_output(&["remove", "--help"]);
    let upper = output.to_ascii_uppercase();
    assert!(
        upper.contains("<NAME>") || upper.contains("[NAME"),
        "remove help should name the argument NAME, got: {output}"
    );
    assert!(
        !upper.contains("<SPEC>") && !upper.contains("[SPEC"),
        "remove help should not refer to SPEC, got: {output}"
    );
}

#[test]
fn build_help_mentions_skip_tests_example() {
    let output = help_output(&["build", "--help"]);
    assert!(
        output.contains("Build sdists/wheels from project sources (prep for px publish)."),
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
fn top_level_help_mentions_debug_flag() {
    let output = help_output(&["--help"]);
    assert!(
        output.contains("--debug"),
        "global help should surface the --debug flag: {output}"
    );
}

#[test]
fn sync_help_mentions_lock_resolution_and_flag_help() {
    let output = help_output(&["sync", "--help"]);
    assert!(
        output.contains("Resolve (if needed) and sync env from lock"),
        "sync help should describe dev-mode lock/env behavior: {output}"
    );
    assert!(
        output.contains("--dry-run") && output.contains("Preview changes and print a summary"),
        "sync help should describe --dry-run behavior: {output}"
    );
    assert!(
        output.contains("--frozen") && output.contains("do not resolve dependencies"),
        "sync help should describe --frozen behavior: {output}"
    );
}

#[test]
fn force_flag_is_init_only() {
    let init = help_output(&["init", "--help"]);
    assert!(
        init.contains("\n      --force "),
        "init help should include the --force flag: {init}"
    );

    for cmd in ["add", "remove", "sync", "build", "update"] {
        let output = help_output(&[cmd, "--help"]);
        assert!(
            !output.contains("\n      --force "),
            "{cmd} should not accept a standalone --force flag: {output}"
        );
    }
}

#[test]
fn top_level_help_describes_build_sources() {
    let output = help_output(&["--help"]);
    assert!(
        output.contains("build            Build sdists/wheels from project sources."),
        "top-level help should describe build accurately: {output}"
    );
}
