use assert_cmd::cargo::cargo_bin_cmd;

#[test]
fn help_prints_usage() {
    cargo_bin_cmd!("px").arg("--help").assert().success();
}
