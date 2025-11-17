

#![allow(dead_code)]

use std::env;

use anyhow::{bail, Result};


pub fn detect_interpreter() -> Result<String> {
    if let Some(explicit) = env::var("PX_RUNTIME_PYTHON").ok() {
        return Ok(explicit);
    }

    for candidate in ["python3", "python"] {
        if let Ok(path) = which::which(candidate) {
            return Ok(path
                .into_os_string()
                .into_string()
                .map_err(|_| anyhow::anyhow!("non-utf8 path"))?);
        }
    }

    bail!("no python interpreter found; set PX_RUNTIME_PYTHON");
}
