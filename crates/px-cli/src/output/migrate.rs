use px_core::api::{CommandGroup, CommandInfo};
use serde_json::Value;

use crate::style::Style;

pub(super) fn render_migrate_table(
    style: &Style,
    info: CommandInfo,
    details: &Value,
) -> Option<String> {
    if info.group != CommandGroup::Migrate {
        return None;
    }
    let packages = details.get("packages")?.as_array()?;
    if packages.is_empty() {
        return None;
    }

    let mut rows = Vec::new();
    for pkg in packages {
        let obj = pkg.as_object()?;
        rows.push(PackageRow {
            name: obj.get("name")?.as_str()?.to_string(),
            source: obj.get("source")?.as_str()?.to_string(),
            requested: obj.get("requested")?.as_str()?.to_string(),
            scope: obj.get("scope")?.as_str()?.to_string(),
        });
    }

    Some(format_package_table(style, &rows))
}

struct PackageRow {
    name: String,
    source: String,
    requested: String,
    scope: String,
}

fn format_package_table(style: &Style, rows: &[PackageRow]) -> String {
    let headers = ["Package", "Source", "Requested", "Scope"];
    let mut widths = [
        headers[0].len(),
        headers[1].len(),
        headers[2].len(),
        headers[3].len(),
    ];

    for row in rows {
        widths[0] = widths[0].max(row.name.len());
        widths[1] = widths[1].max(row.source.len());
        widths[2] = widths[2].max(row.requested.len());
        widths[3] = widths[3].max(row.scope.len());
    }

    let header_line = format!(
        "{:<width0$}  {:<width1$}  {:<width2$}  {:<width3$}",
        headers[0],
        headers[1],
        headers[2],
        headers[3],
        width0 = widths[0],
        width1 = widths[1],
        width2 = widths[2],
        width3 = widths[3],
    );

    let mut lines = Vec::new();
    lines.push(style.table_header(&header_line));
    lines.push(format!(
        "{:-<width0$}  {:-<width1$}  {:-<width2$}  {:-<width3$}",
        "",
        "",
        "",
        "",
        width0 = widths[0],
        width1 = widths[1],
        width2 = widths[2],
        width3 = widths[3],
    ));

    for row in rows {
        lines.push(format!(
            "{:<width0$}  {:<width1$}  {:<width2$}  {:<width3$}",
            row.name,
            row.source,
            row.requested,
            row.scope,
            width0 = widths[0],
            width1 = widths[1],
            width2 = widths[2],
            width3 = widths[3],
        ));
    }

    lines.join("\n")
}
