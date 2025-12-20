use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::Result;

#[cfg(not(windows))]
use std::collections::BTreeMap;

#[cfg(not(windows))]
use serde_json::Value;

#[cfg(not(windows))]
pub(crate) fn write_python_shim(
    bin_dir: &Path,
    runtime: &Path,
    site: &Path,
    env_vars: &BTreeMap<String, Value>,
) -> Result<()> {
    fs::create_dir_all(bin_dir)?;
    let shim = bin_dir.join("python");
    let mut script = String::new();
    script.push_str("#!/usr/bin/env bash\n");
    if let Some(runtime_root) = runtime.parent().and_then(|bin| bin.parent()) {
        script.push_str(&format!(
            "export PYTHONHOME=\"{}\"\n",
            runtime_root.display()
        ));
    }
    let path_sep = if cfg!(windows) { ";" } else { ":" };
    let mut pythonpath = site.display().to_string();
    if let Some(runtime_root) = runtime.parent().and_then(|bin| bin.parent()) {
        if let Some(version_dir) = site.parent().and_then(|p| p.file_name()) {
            let runtime_site = runtime_root
                .join("lib")
                .join(version_dir)
                .join("site-packages");
            pythonpath.push_str(path_sep);
            pythonpath.push_str(&runtime_site.display().to_string());
        }
    }
    script.push_str("if [ -n \"$PYTHONPATH\" ]; then\n");
    script.push_str(&format!(
        "  export PYTHONPATH=\"$PYTHONPATH{path_sep}{pythonpath}\"\n"
    ));
    script.push_str("else\n");
    script.push_str(&format!("  export PYTHONPATH=\"{pythonpath}\"\n"));
    script.push_str("fi\n");
    script.push_str(&format!("export PX_PYTHON=\"{}\"\n", runtime.display()));
    script.push_str("export PYTHONUNBUFFERED=1\n");
    let profile_key = bin_dir
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    script.push_str("if [ -z \"${PYTHONPYCACHEPREFIX:-}\" ]; then\n");
    script.push_str("  if [ -n \"${PX_CACHE_PATH:-}\" ]; then\n");
    script.push_str("    cache_root=\"$PX_CACHE_PATH\"\n");
    script.push_str("  elif [ -n \"${HOME:-}\" ]; then\n");
    script.push_str("    cache_root=\"$HOME/.px/cache\"\n");
    script.push_str("  else\n");
    script.push_str("    cache_root=\"/tmp/px/cache\"\n");
    script.push_str("  fi\n");
    script.push_str(&format!(
        "  export PYTHONPYCACHEPREFIX=\"$cache_root/pyc/{profile_key}\"\n"
    ));
    script.push_str("fi\n");
    script.push_str("if [ -n \"${PYTHONPYCACHEPREFIX:-}\" ]; then\n");
    script.push_str("  mkdir -p \"$PYTHONPYCACHEPREFIX\" 2>/dev/null || {\n");
    script.push_str("    echo \"px: python bytecode cache directory is not writable: $PYTHONPYCACHEPREFIX\" >&2\n");
    script.push_str("    exit 1\n");
    script.push_str("  }\n");
    script.push_str("fi\n");
    // Profile env_vars override the parent environment for the launched runtime.
    for (key, value) in env_vars {
        let rendered = env_var_value(value);
        script.push_str(&format!("export {key}={}\n", shell_escape(&rendered)));
    }
    script.push_str(&format!("exec \"{}\" \"$@\"\n", runtime.display()));
    fs::write(&shim, script)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o755))?;
    }
    for alias in ["python3", "python3.11", "python3.12"] {
        let dest = bin_dir.join(alias);
        let _ = fs::remove_file(&dest);
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let _ = symlink(Path::new("python"), &dest)
                .or_else(|_| fs::hard_link(&shim, &dest))
                .or_else(|_| fs::copy(&shim, &dest).map(|_| ()));
        }
        #[cfg(not(unix))]
        {
            let _ = fs::hard_link(&shim, &dest).or_else(|_| fs::copy(&shim, &dest).map(|_| ()));
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn env_var_value(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(not(windows))]
fn shell_escape(value: &str) -> String {
    let mut escaped = String::from("'");
    for ch in value.chars() {
        if ch == '\'' {
            escaped.push_str("'\\''");
        } else {
            escaped.push(ch);
        }
    }
    escaped.push('\'');
    escaped
}

pub(super) fn materialize_wheel_scripts(artifact_path: &Path, bin_dir: &Path) -> Result<()> {
    fs::create_dir_all(bin_dir)?;
    if artifact_path.extension().is_some_and(|ext| ext == "dist") && artifact_path.is_dir() {
        let entry_points = fs::read_dir(artifact_path)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|ext| ext == "dist-info"))
            .and_then(|dist_info| {
                let ep = dist_info.join("entry_points.txt");
                ep.exists().then_some(ep)
            });
        if let Some(ep_path) = entry_points {
            if let Ok(contents) = fs::read_to_string(&ep_path) {
                let mut section = String::new();
                for line in contents.lines() {
                    let trimmed = line.trim();
                    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
                        continue;
                    }
                    if trimmed.starts_with('[') && trimmed.ends_with(']') {
                        section = trimmed
                            .trim_start_matches('[')
                            .trim_end_matches(']')
                            .to_string();
                        continue;
                    }
                    if section != "console_scripts" && section != "gui_scripts" {
                        continue;
                    }
                    if let Some((name, target)) = trimmed.split_once('=') {
                        let entry_name = name.trim();
                        let raw_target = target.trim();
                        let target_value = raw_target
                            .split_whitespace()
                            .next()
                            .unwrap_or(raw_target)
                            .trim();
                        if let Some((module, callable)) = target_value.split_once(':') {
                            let _ = write_entrypoint_script(
                                bin_dir,
                                entry_name,
                                module.trim(),
                                callable.trim(),
                            );
                        }
                    }
                }
            }
        }

        let script_dirs: Vec<PathBuf> = fs::read_dir(artifact_path)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name.ends_with(".data"))
                    .unwrap_or(false)
            })
            .map(|data_dir| data_dir.join("scripts"))
            .filter(|path| path.exists())
            .collect();
        for dir in script_dirs {
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                if entry.file_type()?.is_file() {
                    let dest = bin_dir.join(entry.file_name());
                    fs::copy(entry.path(), &dest)?;
                    let _ = set_exec_permissions(&dest);
                }
            }
        }
        return Ok(());
    }

    Ok(())
}

fn write_entrypoint_script(
    bin_dir: &Path,
    name: &str,
    module: &str,
    callable: &str,
) -> Result<PathBuf> {
    fs::create_dir_all(bin_dir)?;
    let python_shebang = "/usr/bin/env python3".to_string();
    let parts: Vec<String> = callable
        .split('.')
        .filter(|part| !part.is_empty())
        .map(ToString::to_string)
        .collect();
    let parts_repr = format!("{parts:?}");
    let contents = format!(
        "#!{python_shebang}\nimport importlib\nimport sys\n\ndef _load():\n    module = importlib.import_module({module:?})\n    target = module\n    for attr in {parts_repr}:\n        target = getattr(target, attr)\n    return target\n\nif __name__ == '__main__':\n    sys.exit(_load()())\n"
    );
    let script_path = bin_dir.join(name);
    fs::write(&script_path, contents)?;
    let _ = set_exec_permissions(&script_path);
    Ok(script_path)
}

fn set_exec_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

pub(super) fn should_rewrite_python_entrypoint(path: &Path) -> Result<bool> {
    let mut file = fs::File::open(path)?;
    let mut buf = [0u8; 256];
    let read = file.read(&mut buf)?;
    let prefix = std::str::from_utf8(&buf[..read]).unwrap_or_default();
    let Some(first_line) = prefix.lines().next() else {
        return Ok(false);
    };
    if !first_line.starts_with("#!") {
        return Ok(false);
    }
    Ok(first_line.to_ascii_lowercase().contains("python"))
}

pub(super) fn rewrite_python_entrypoint(src: &Path, dest: &Path, python: &Path) -> Result<()> {
    if dest.exists() {
        let _ = fs::remove_file(dest);
    }
    let contents = fs::read_to_string(src)?;
    let mut parts = contents.splitn(2, '\n');
    let _ = parts.next();
    let rest = parts.next().unwrap_or("");
    let mut rewritten = format!("#!{}\n", python.display());
    rewritten.push_str(rest);
    fs::write(dest, rewritten.as_bytes())?;
    set_exec_permissions(dest)?;
    Ok(())
}
