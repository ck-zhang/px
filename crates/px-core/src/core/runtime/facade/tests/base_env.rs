use super::super::*;
use anyhow::Result;
use px_domain::api::PxOptions;
use serde_json::json;
use std::env;
use tempfile::tempdir;

#[test]
fn base_env_sets_pyc_cache_prefix() -> Result<()> {
    env::remove_var("PYTHONPYCACHEPREFIX");
    let temp = tempdir()?;
    let prefix = temp.path().join("pyc-prefix");
    let ctx = PythonContext {
        project_root: temp.path().to_path_buf(),
        state_root: temp.path().to_path_buf(),
        project_name: "demo".to_string(),
        python: "python".into(),
        pythonpath: temp.path().display().to_string(),
        allowed_paths: vec![temp.path().to_path_buf()],
        site_bin: None,
        pep582_bin: Vec::new(),
        pyc_cache_prefix: Some(prefix.clone()),
        px_options: PxOptions::default(),
    };
    let envs = ctx.base_env(&json!({}))?;
    assert!(
        envs.iter().all(|(key, _)| key != "PYTHONDONTWRITEBYTECODE"),
        "base env should not disable bytecode writes"
    );
    let value = envs
        .iter()
        .find(|(key, _)| key == "PYTHONPYCACHEPREFIX")
        .map(|(_, value)| value.clone());
    assert_eq!(value, Some(prefix.display().to_string()));
    assert!(
        prefix.exists(),
        "expected cache prefix {} to exist",
        prefix.display()
    );
    Ok(())
}
