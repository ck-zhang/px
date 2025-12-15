use super::super::sandbox::apply_system_lib_compatibility;
use crate::InstallUserError;
use anyhow::Result;
use serde_json::Value;

#[test]
fn system_lib_compat_caps_unpinned_gdal() -> Result<()> {
    let mut system_deps = crate::core::system_deps::SystemDeps::default();
    system_deps
        .apt_versions
        .insert("libgdal-dev".to_string(), "3.6.2+dfsg-1+b2".to_string());
    let reqs = vec!["gdal".to_string()];
    let out = apply_system_lib_compatibility(reqs, &system_deps)?;
    assert_eq!(out, vec!["gdal<=3.6.2".to_string()]);
    Ok(())
}

#[test]
fn system_lib_compat_preserves_lower_bounds() -> Result<()> {
    let mut system_deps = crate::core::system_deps::SystemDeps::default();
    system_deps
        .apt_versions
        .insert("libgdal-dev".to_string(), "3.6.2".to_string());
    let reqs = vec!["gdal>=3.0".to_string(), "psycopg2".to_string()];
    let out = apply_system_lib_compatibility(reqs, &system_deps)?;
    assert_eq!(
        out,
        vec!["gdal>=3.0,<=3.6.2".to_string(), "psycopg2".to_string()]
    );
    Ok(())
}

#[test]
fn system_lib_compat_errors_on_incompatible_pin() {
    let mut system_deps = crate::core::system_deps::SystemDeps::default();
    system_deps
        .apt_versions
        .insert("libgdal-dev".to_string(), "3.6.2".to_string());
    let reqs = vec!["gdal==3.8.0".to_string()];
    let err = apply_system_lib_compatibility(reqs, &system_deps).unwrap_err();
    let Some(install_err) = err.downcast_ref::<InstallUserError>() else {
        panic!("expected InstallUserError, got {err:?}");
    };
    let hint = install_err
        .details()
        .get("hint")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(hint.contains("base provides libgdal 3.6.2"));
    assert!(hint.contains("requested gdal==3.8.0"));
}
