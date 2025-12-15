use super::super::env_materialize::materialize_wheel_scripts;
use anyhow::Result;
use std::fs;
use std::io::Write;
use std::path::Path;
use tempfile::tempdir;
use zip::write::FileOptions;

#[test]
fn materialize_scripts_from_dist_directory() -> Result<()> {
    let temp = tempdir()?;
    let artifact = temp.path().join("demo-0.1.0.dist");
    let dist_info = artifact.join("demo-0.1.0.dist-info");
    let data_scripts = artifact.join("demo-0.1.0.data").join("scripts");
    fs::create_dir_all(&dist_info)?;
    fs::create_dir_all(&data_scripts)?;
    fs::write(
        dist_info.join("entry_points.txt"),
        "[console_scripts]\nalpha = demo.cli:main\n[gui_scripts]\nbeta = demo.gui:run\n",
    )?;
    fs::write(data_scripts.join("copied.sh"), "echo copied\n")?;

    let bin_dir = temp.path().join("bin");
    materialize_wheel_scripts(&artifact, &bin_dir, Some(Path::new("/custom/python")))?;

    let alpha = fs::read_to_string(bin_dir.join("alpha"))?;
    assert!(
        alpha.starts_with("#!/custom/python"),
        "shebang honors python"
    );
    assert!(alpha.contains("demo.cli"));
    let beta = fs::read_to_string(bin_dir.join("beta"))?;
    assert!(beta.contains("demo.gui"));
    let copied = fs::read_to_string(bin_dir.join("copied.sh"))?;
    assert!(copied.contains("copied"));
    Ok(())
}

#[test]
fn materialize_scripts_from_wheel_file() -> Result<()> {
    let temp = tempdir()?;
    let wheel_path = temp.path().join("demo-0.2.0-py3-none-any.whl");
    let file = fs::File::create(&wheel_path)?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = FileOptions::default();
    zip.start_file("demo-0.2.0.dist-info/entry_points.txt", opts)?;
    zip.write_all(b"[console_scripts]\ngamma = demo.core:run\n")?;
    zip.start_file("demo-0.2.0.data/scripts/helper.sh", opts)?;
    zip.write_all(b"echo helper\n")?;
    zip.finish()?;

    let bin_dir = temp.path().join("wheel-bin");
    materialize_wheel_scripts(&wheel_path, &bin_dir, None)?;

    let gamma = fs::read_to_string(bin_dir.join("gamma"))?;
    assert!(gamma.starts_with("#!/usr/bin/env python3"));
    assert!(gamma.contains("demo.core"));
    let helper = fs::read_to_string(bin_dir.join("helper.sh"))?;
    assert!(helper.contains("helper"));
    Ok(())
}
