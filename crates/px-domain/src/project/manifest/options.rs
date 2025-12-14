use super::*;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PxOptions {
    pub manage_command: Option<String>,
    pub plugin_imports: Vec<String>,
    pub env_vars: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxConfig {
    pub base: Option<String>,
    pub auto: bool,
    pub capabilities: BTreeMap<String, bool>,
    pub defined: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            base: None,
            auto: true,
            capabilities: BTreeMap::new(),
            defined: false,
        }
    }
}

pub fn px_options_from_doc(doc: &DocumentMut) -> PxOptions {
    let px_table = doc
        .get("tool")
        .and_then(Item::as_table)
        .and_then(|tool| tool.get("px"))
        .and_then(Item::as_table);
    let mut options = PxOptions::default();
    if let Some(px) = px_table {
        if let Some(value) = px.get("manage-command").and_then(Item::as_str) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                options.manage_command = Some(trimmed.to_string());
            }
        }
        if let Some(array) = px.get("plugin-imports").and_then(Item::as_array) {
            let mut imports = Vec::new();
            for entry in array.iter() {
                if let Some(value) = entry.as_str() {
                    let trimmed = value.trim();
                    if !trimmed.is_empty() {
                        imports.push(trimmed.to_string());
                    }
                }
            }
            imports.sort();
            imports.dedup();
            options.plugin_imports = imports;
        }
        if let Some(env_table) = px.get("env").and_then(Item::as_table) {
            for (key, value) in env_table.iter() {
                let key = key.trim();
                if key.is_empty() {
                    continue;
                }
                if let Some(val) = value.as_str() {
                    let trimmed = val.trim();
                    if !trimmed.is_empty() {
                        options
                            .env_vars
                            .insert(key.to_string(), trimmed.to_string());
                    }
                } else if let Some(val) = value.as_value() {
                    let trimmed = val.to_string().trim().to_string();
                    if !trimmed.is_empty() {
                        options.env_vars.insert(key.to_string(), trimmed);
                    }
                }
            }
        }
    }
    options
}

pub fn sandbox_config_from_doc(doc: &DocumentMut) -> SandboxConfig {
    let px_table = doc
        .get("tool")
        .and_then(Item::as_table)
        .and_then(|tool| tool.get("px"))
        .and_then(Item::as_table);
    let mut config = SandboxConfig {
        auto: true,
        ..SandboxConfig::default()
    };
    let Some(px) = px_table else {
        return config;
    };
    let Some(sandbox) = px.get("sandbox").and_then(Item::as_table) else {
        return config;
    };
    config.defined = true;
    if let Some(base) = sandbox.get("base").and_then(Item::as_str) {
        let trimmed = base.trim();
        if !trimmed.is_empty() {
            config.base = Some(trimmed.to_string());
        }
    }
    if let Some(auto) = sandbox.get("auto").and_then(Item::as_bool) {
        config.auto = auto;
    }
    if let Some(table) = sandbox.get("capabilities").and_then(Item::as_table) {
        for (name, value) in table.iter() {
            let trimmed = name.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(flag) = value.as_value().and_then(TomlValue::as_bool) {
                config.capabilities.insert(trimmed.to_string(), flag);
            }
        }
    }
    config
}

pub fn sandbox_config_from_manifest(path: &Path) -> Result<SandboxConfig> {
    ensure_pyproject_exists(path)?;
    let contents = fs::read_to_string(path)?;
    let doc: DocumentMut = contents.parse()?;
    Ok(sandbox_config_from_doc(&doc))
}
