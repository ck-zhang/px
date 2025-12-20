use std::fs;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use toml_edit::{DocumentMut, InlineTable, Item, Table};

pub(crate) const DEFAULT_RUFF_REQUIREMENT: &str = "ruff";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum QualityKind {
    Fmt,
}

impl QualityKind {
    pub(crate) fn section_name(self) -> &'static str {
        match self {
            QualityKind::Fmt => "fmt",
        }
    }

    pub(crate) fn success_message(self) -> &'static str {
        match self {
            QualityKind::Fmt => "formatted source files",
        }
    }

    pub(crate) fn default_tools(self) -> Vec<QualityTool> {
        match self {
            QualityKind::Fmt => vec![QualityTool::new(
                "ruff".to_string(),
                vec!["format".to_string()],
                None,
                Some(DEFAULT_RUFF_REQUIREMENT.to_string()),
            )],
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum QualityConfigSource {
    Default,
    Pyproject,
}

impl QualityConfigSource {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            QualityConfigSource::Default => "default",
            QualityConfigSource::Pyproject => "pyproject",
        }
    }
}

#[derive(Clone)]
pub(crate) struct QualityToolConfig {
    pub(crate) tools: Vec<QualityTool>,
    pub(crate) source: QualityConfigSource,
}

#[derive(Clone)]
pub(crate) struct QualityTool {
    pub(crate) label: String,
    pub(crate) module: String,
    pub(crate) args: Vec<String>,
    pub(crate) requirement: Option<String>,
}

impl QualityTool {
    pub(crate) fn new(
        module: String,
        args: Vec<String>,
        label: Option<String>,
        requirement: Option<String>,
    ) -> Self {
        let label = label.unwrap_or_else(|| module.clone());
        Self {
            label,
            module,
            args,
            requirement,
        }
    }

    pub(crate) fn from_table(table: &Table) -> Result<Self> {
        let module = table
            .get("module")
            .and_then(Item::as_str)
            .ok_or_else(|| anyhow!("tool entry missing `module`"))?
            .to_string();
        let args = parse_item_string_array(table.get("args"))?;
        let label = table
            .get("label")
            .and_then(Item::as_str)
            .map(ToString::to_string);
        let requirement = table
            .get("requirement")
            .and_then(Item::as_str)
            .map(ToString::to_string);
        Ok(Self::new(module, args, label, requirement))
    }

    pub(crate) fn from_inline_table(inline: &InlineTable) -> Result<Self> {
        let table = inline.clone().into_table();
        Self::from_table(&table)
    }

    pub(crate) fn display_name(&self) -> &str {
        &self.label
    }

    pub(crate) fn python_args(&self, forwarded: &[String]) -> Vec<String> {
        let mut args = Vec::with_capacity(2 + self.args.len() + forwarded.len());
        args.push("-m".to_string());
        args.push(self.module.clone());
        args.extend(self.args.iter().cloned());
        args.extend(forwarded.iter().cloned());
        args
    }

    pub(crate) fn requirement_spec(&self) -> Option<String> {
        self.requirement.clone()
    }

    pub(crate) fn install_name(&self) -> &str {
        &self.module
    }

    pub(crate) fn install_command(&self) -> String {
        match self.requirement_spec() {
            Some(requirement) => format!("px tool install {requirement}"),
            None => format!("px tool install {}", self.module),
        }
    }
}

pub(crate) fn load_quality_tools(pyproject: &Path, kind: QualityKind) -> Result<QualityToolConfig> {
    let contents = fs::read_to_string(pyproject)
        .with_context(|| format!("reading {}", pyproject.display()))?;
    let doc: DocumentMut = contents.parse()?;
    let section_table = doc
        .get("tool")
        .and_then(Item::as_table)
        .and_then(|tool| tool.get("px"))
        .and_then(Item::as_table)
        .and_then(|px| px.get(kind.section_name()))
        .and_then(Item::as_table);

    if let Some(table) = section_table {
        let tools = parse_quality_section(table)?;
        if tools.is_empty() {
            return Err(anyhow!(
                "no commands configured under [tool.px.{}]",
                kind.section_name()
            ));
        }
        return Ok(QualityToolConfig {
            tools,
            source: QualityConfigSource::Pyproject,
        });
    }

    Ok(QualityToolConfig {
        tools: kind.default_tools(),
        source: QualityConfigSource::Default,
    })
}

pub(crate) fn parse_quality_section(table: &Table) -> Result<Vec<QualityTool>> {
    if let Some(item) = table.get("commands") {
        let tools = parse_commands_item(item)?;
        if !tools.is_empty() {
            return Ok(tools);
        }
    }

    if let Some(module) = table.get("module").and_then(Item::as_str) {
        let args = parse_item_string_array(table.get("args"))?;
        let label = table
            .get("label")
            .and_then(Item::as_str)
            .map(ToString::to_string);
        let requirement = table
            .get("requirement")
            .and_then(Item::as_str)
            .map(ToString::to_string);
        return Ok(vec![QualityTool::new(
            module.to_string(),
            args,
            label,
            requirement,
        )]);
    }
    Ok(Vec::new())
}

fn parse_commands_item(item: &Item) -> Result<Vec<QualityTool>> {
    if let Some(array) = item.as_array() {
        let mut tools = Vec::new();
        for entry in array {
            let inline = entry
                .as_inline_table()
                .ok_or_else(|| anyhow!("commands entries must be inline tables"))?;
            tools.push(QualityTool::from_inline_table(inline)?);
        }
        return Ok(tools);
    }
    if let Some(array) = item.as_array_of_tables() {
        let mut tools = Vec::new();
        for table in array {
            tools.push(QualityTool::from_table(table)?);
        }
        return Ok(tools);
    }
    Ok(Vec::new())
}

fn parse_item_string_array(item: Option<&Item>) -> Result<Vec<String>> {
    let Some(array_item) = item else {
        return Ok(Vec::new());
    };
    let array = array_item
        .as_array()
        .ok_or_else(|| anyhow!("args must be an array of strings"))?;
    let mut values = Vec::new();
    for value in array {
        let literal = value
            .as_str()
            .ok_or_else(|| anyhow!("args entries must be strings"))?;
        values.push(literal.to_string());
    }
    Ok(values)
}

pub(crate) fn missing_module_error(output: &str, module: &str) -> bool {
    let needle = format!("No module named '{module}'");
    let needle_unquoted = format!("No module named {module}");
    output.contains(&needle) || output.contains(&needle_unquoted)
}
