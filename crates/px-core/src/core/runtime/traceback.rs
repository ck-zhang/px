use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;

const TRACEBACK_HEADER: &str = "Traceback (most recent call last):";

#[derive(Debug, Clone, Serialize)]
pub struct TracebackFrame {
    pub file: String,
    pub line: u32,
    pub function: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TracebackRecommendation {
    pub reason: &'static str,
    pub hint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TracebackReport {
    pub header: String,
    pub frames: Vec<TracebackFrame>,
    pub error_type: String,
    pub error_message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<TracebackRecommendation>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct TracebackContext {
    pub command: String,
    pub target: String,
    pub entry: Option<String>,
    pub mode: Option<String>,
    manifest_deps: HashSet<String>,
    locked_deps: HashSet<String>,
}

impl TracebackContext {
    pub fn new(command: &str, target: &str, extra: Option<&Value>) -> Self {
        Self {
            command: command.to_string(),
            target: target.to_string(),
            entry: extra
                .and_then(|value| value.get("entry"))
                .and_then(Value::as_str)
                .map(str::to_string),
            mode: extra
                .and_then(|value| value.get("mode"))
                .and_then(Value::as_str)
                .map(str::to_string),
            manifest_deps: dep_set(extra, "manifest_deps"),
            locked_deps: dep_set(extra, "locked_deps"),
        }
    }

    fn dependency_declared(&self, package: &str) -> bool {
        let needle = package.to_lowercase();
        self.manifest_deps.contains(&needle) || self.locked_deps.contains(&needle)
    }
}

fn dep_set(extra: Option<&Value>, key: &str) -> HashSet<String> {
    extra
        .and_then(|value| value.get(key))
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .map(|value| value.to_lowercase())
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone)]
struct TracebackSummary {
    header: String,
    frames: Vec<TracebackFrame>,
    error_type: String,
    error_message: String,
}

pub fn analyze_python_traceback(stderr: &str, ctx: &TracebackContext) -> Option<TracebackReport> {
    let summary = TracebackSummary::parse(stderr)?;
    let recommendation = recommendation_for(&summary, ctx);
    Some(TracebackReport {
        header: summary.header,
        frames: summary.frames,
        error_type: summary.error_type,
        error_message: summary.error_message,
        recommendation,
    })
}

fn recommendation_for(
    summary: &TracebackSummary,
    ctx: &TracebackContext,
) -> Option<TracebackRecommendation> {
    missing_import(summary, ctx).or_else(|| distribution_not_found(summary))
}

fn missing_import(
    summary: &TracebackSummary,
    ctx: &TracebackContext,
) -> Option<TracebackRecommendation> {
    if !(summary.error_type.contains("ModuleNotFoundError")
        || summary.error_type.contains("ImportError")
            && summary.error_message.contains("No module named"))
    {
        return None;
    }
    let module = extract_missing_module(&summary.error_message)?;
    if is_stdlib(&module) {
        return None;
    }
    let package = module_to_package(&module);
    if ctx.dependency_declared(&package) {
        return Some(TracebackRecommendation {
            reason: "missing_import",
            hint: format!(
                "dependency '{package}' is declared; run `px sync` to rebuild the environment"
            ),
            command: Some("px sync".to_string()),
            confidence: Some("high"),
        });
    }
    if looks_like_missing_local_extension(&module, &summary.frames) {
        return Some(TracebackRecommendation {
            reason: "missing_import",
            hint: format!(
                "`{module}` looks like a compiled extension imported from your source tree; build the project (native extensions) or install a prebuilt wheel, then rerun the command"
            ),
            command: None,
            confidence: Some("medium"),
        });
    }
    let dev_tool = should_treat_as_dev_tool(&package, ctx);
    let command = if dev_tool {
        format!("px add --dev {package}")
    } else {
        format!("px add {package}")
    };
    let hint = if dev_tool {
        format!("add the dev tool with `{command}` and rerun the command")
    } else {
        format!("add '{package}' with `{command}` and rerun the command")
    };
    Some(TracebackRecommendation {
        reason: "missing_import",
        hint,
        command: Some(command),
        confidence: Some("high"),
    })
}

fn distribution_not_found(summary: &TracebackSummary) -> Option<TracebackRecommendation> {
    let error = summary.error_type.to_lowercase();
    let message = summary.error_message.to_lowercase();
    if error.contains("distributionnotfound") || message.contains("distribution was not found") {
        return Some(TracebackRecommendation {
            reason: "distribution_missing",
            hint: "run `px install` to sync the environment with px.lock".to_string(),
            command: Some("px install".to_string()),
            confidence: Some("medium"),
        });
    }
    None
}

impl TracebackSummary {
    fn parse(stderr: &str) -> Option<Self> {
        let lines: Vec<&str> = stderr.lines().collect();
        let mut idx = 0;
        let mut latest: Option<Self> = None;
        while idx < lines.len() {
            let line = lines[idx].trim_start();
            if line.starts_with(TRACEBACK_HEADER) {
                if let Some((summary, next_idx)) = Self::parse_block(&lines, idx + 1) {
                    latest = Some(summary);
                    idx = next_idx;
                    continue;
                }
                break;
            }
            idx += 1;
        }
        latest
    }

    fn parse_block(lines: &[&str], mut idx: usize) -> Option<(Self, usize)> {
        let mut frames = Vec::new();
        while idx < lines.len() {
            let line = lines[idx];
            let trimmed = line.trim_start();
            if trimmed.is_empty() {
                idx += 1;
                continue;
            }
            if is_pointer_line(trimmed) || is_ellipsis_line(trimmed) {
                idx += 1;
                continue;
            }
            if let Some(frame) = parse_frame_line(trimmed) {
                let mut frame = frame;
                idx += 1;
                if idx < lines.len() {
                    let next = lines[idx];
                    if next.starts_with(' ') || next.starts_with('\t') {
                        let next_trimmed = next.trim_end();
                        if !next_trimmed.trim_start().starts_with("File \"") {
                            frame.code = Some(next_trimmed.trim().to_string());
                            idx += 1;
                        }
                    }
                }
                frames.push(frame);
                continue;
            }
            let (error_type, error_message) = parse_error_line(trimmed);
            return Some((
                Self {
                    header: TRACEBACK_HEADER.to_string(),
                    frames,
                    error_type,
                    error_message,
                },
                idx + 1,
            ));
        }
        None
    }
}

fn is_pointer_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    trimmed.chars().all(|ch| ch == '^' || ch == '~')
}

fn is_ellipsis_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("...") && trimmed.ends_with("...")
}

fn parse_frame_line(line: &str) -> Option<TracebackFrame> {
    let trimmed = line.trim();
    if !trimmed.starts_with("File \"") {
        return None;
    }
    let after_prefix = trimmed.trim_start_matches("File \"");
    let quote_end = after_prefix.find('"')?;
    let file = after_prefix[..quote_end].to_string();
    let after_file = after_prefix[quote_end + 1..].trim_start();
    let after_line = after_file.strip_prefix(", line ")?;
    let mut parts = after_line.splitn(2, ',');
    let line_number = parts.next()?.trim().parse().ok()?;
    let mut function = "<module>".to_string();
    if let Some(rest) = parts.next() {
        let trimmed = rest.trim();
        if let Some(name) = trimmed.strip_prefix("in ") {
            function = name.trim().to_string();
        }
    }
    Some(TracebackFrame {
        file,
        line: line_number,
        function,
        code: None,
    })
}

fn parse_error_line(line: &str) -> (String, String) {
    if let Some((kind, message)) = line.split_once(':') {
        (kind.trim().to_string(), message.trim().to_string())
    } else {
        (line.trim().to_string(), String::new())
    }
}

fn extract_missing_module(message: &str) -> Option<String> {
    let offset = message.find("No module named")?;
    let mut token = message[offset + "No module named".len()..].trim();
    token = token.trim_start_matches(':').trim();
    token = token.trim_start_matches('\'').trim_start_matches('"');
    let mut end = token
        .find(|c| [' ', '"', '\'', ':', ')'].contains(&c))
        .unwrap_or(token.len());
    while end > 0
        && matches!(
            token.as_bytes()[end - 1] as char,
            '"' | '\'' | '.' | ',' | ')'
        )
    {
        end -= 1;
    }
    let module = token[..end].trim();
    if module.is_empty() {
        None
    } else {
        Some(module.to_string())
    }
}

fn module_to_package(module: &str) -> String {
    let module = module.split('.').next().unwrap_or(module);
    match module {
        "yaml" => "PyYAML".to_string(),
        "cv2" => "opencv-python".to_string(),
        "PIL" | "pil" => "Pillow".to_string(),
        "sklearn" => "scikit-learn".to_string(),
        "bs4" => "beautifulsoup4".to_string(),
        other => other.to_string(),
    }
}

fn looks_like_missing_local_extension(module: &str, frames: &[TracebackFrame]) -> bool {
    let mut parts = module.split('.');
    let Some(package) = parts.next() else {
        return false;
    };
    let Some(submodule) = parts.next() else {
        return false;
    };
    if !submodule.starts_with('_') {
        return false;
    }
    let unix_marker = format!("/{package}/");
    let windows_marker = format!("\\{package}\\");
    frames.iter().any(|frame| {
        let file = frame.file.as_str();
        if !file.ends_with(".py") {
            return false;
        }
        if !file.contains(&unix_marker) && !file.contains(&windows_marker) {
            return false;
        }
        let lower = file.to_ascii_lowercase();
        if lower.contains("site-packages")
            || lower.contains("dist-packages")
            || lower.contains("/.px/")
            || lower.contains("\\.px\\")
        {
            return false;
        }
        true
    })
}

fn should_treat_as_dev_tool(package: &str, ctx: &TracebackContext) -> bool {
    const DEV_TOOLS: &[&str] = &[
        "pytest", "ruff", "coverage", "black", "mypy", "isort", "tox", "nox",
    ];
    let lower = package.to_lowercase();
    DEV_TOOLS.contains(&lower.as_str()) || ctx.command == "test"
}

fn is_stdlib(module: &str) -> bool {
    const BLOCKLIST: &[&str] = &["sys", "os", "pathlib", "importlib", "typing", "functools"];
    let lower = module.to_lowercase();
    BLOCKLIST.contains(&lower.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_traceback_frames_and_error() {
        let stderr = "Traceback (most recent call last):\n  File \"/tmp/app/main.py\", line 7, in <module>\n    import missing_pkg\nModuleNotFoundError: No module named 'missing_pkg'\n";
        let ctx = TracebackContext::new("run", "main", None);
        let report = analyze_python_traceback(stderr, &ctx).expect("report");
        assert_eq!(report.frames.len(), 1);
        assert_eq!(report.frames[0].file, "/tmp/app/main.py");
        assert_eq!(report.error_type, "ModuleNotFoundError");
        assert_eq!(report.error_message, "No module named 'missing_pkg'");
        let rec = report.recommendation.expect("recommendation");
        assert_eq!(rec.reason, "missing_import");
        assert!(rec.hint.contains("missing_pkg"));
    }

    #[test]
    fn missing_import_prefers_sync_when_declared() {
        let stderr = "Traceback (most recent call last):\n  File \"/tmp/app/main.py\", line 1, in <module>\n    import requests\nModuleNotFoundError: No module named 'requests'\n";
        let ctx = TracebackContext::new(
            "run",
            "main",
            Some(&json!({
                "manifest_deps": ["requests>=2.0"],
                "locked_deps": ["requests"],
            })),
        );
        let report = analyze_python_traceback(stderr, &ctx).expect("report");
        let rec = report.recommendation.expect("recommendation");
        assert_eq!(rec.reason, "missing_import");
        assert_eq!(rec.command.as_deref(), Some("px sync"));
        assert!(
            rec.hint.contains("px sync"),
            "hint should encourage syncing: {}",
            rec.hint
        );
    }

    #[test]
    fn missing_import_suggests_add_when_undeclared() {
        let stderr = "Traceback (most recent call last):\n  File \"/tmp/app/main.py\", line 1, in <module>\n    import newpkg\nModuleNotFoundError: No module named 'newpkg'\n";
        let ctx = TracebackContext::new(
            "run",
            "main",
            Some(&json!({
                "manifest_deps": [],
                "locked_deps": [],
            })),
        );
        let report = analyze_python_traceback(stderr, &ctx).expect("report");
        let rec = report.recommendation.expect("recommendation");
        assert!(
            rec.command
                .as_deref()
                .is_some_and(|cmd| cmd.contains("px add")),
            "missing dependency should suggest px add, got {rec:?}"
        );
    }

    #[test]
    fn missing_import_strips_submodules_for_resolution() {
        let stderr = "Traceback (most recent call last):\n  File \"/tmp/app/main.py\", line 1, in <module>\n    import requests.packages\nModuleNotFoundError: No module named 'requests.packages'\n";
        let ctx = TracebackContext::new(
            "run",
            "main",
            Some(&json!({
                "manifest_deps": ["requests>=2.0"],
                "locked_deps": ["requests"],
            })),
        );
        let report = analyze_python_traceback(stderr, &ctx).expect("report");
        let rec = report.recommendation.expect("recommendation");
        assert_eq!(rec.command.as_deref(), Some("px sync"));
    }

    #[test]
    fn missing_import_for_local_extension_does_not_suggest_px_add() {
        let stderr = "Traceback (most recent call last):\n  File \"/repo/pkg/__init__.py\", line 1, in <module>\n    import pkg._ext\nModuleNotFoundError: No module named 'pkg._ext'\n";
        let ctx = TracebackContext::new(
            "run",
            "main",
            Some(&json!({
                "manifest_deps": [],
                "locked_deps": [],
            })),
        );
        let report = analyze_python_traceback(stderr, &ctx).expect("report");
        let rec = report.recommendation.expect("recommendation");
        assert!(
            rec.hint.contains("compiled extension"),
            "expected hint to mention compiled extension, got {rec:?}"
        );
        assert!(
            match rec.command.as_deref() {
                Some(cmd) => !cmd.contains("px add"),
                None => true,
            },
            "expected missing local extension to avoid px add, got {rec:?}"
        );
    }

    #[test]
    fn detects_distribution_not_found() {
        let stderr = "Traceback (most recent call last):\n  File \"/app/run.py\", line 2, in <module>\n    import pkg_resources\npkg_resources.DistributionNotFound: The 'requests' distribution was not found and is required by the application\n";
        let ctx = TracebackContext::new("run", "run", None);
        let report = analyze_python_traceback(stderr, &ctx).expect("report");
        let rec = report.recommendation.expect("recommendation");
        assert_eq!(rec.reason, "distribution_missing");
        assert!(rec.hint.contains("px install"));
    }

    #[test]
    fn parses_tracebacks_with_elided_frames() {
        let stderr = "Traceback (most recent call last):\n  File \"/app/main.py\", line 5, in <module>\n    main()\n  File \"/app/main.py\", line 2, in main\n    do_call()\n    ...<5 lines>...\n  File \"/app/lib.py\", line 9, in do_call\n    raise RuntimeError('boom')\nRuntimeError: boom\n";
        let ctx = TracebackContext::new("run", "demo", None);
        let report = analyze_python_traceback(stderr, &ctx).expect("report");
        assert_eq!(report.error_type, "RuntimeError");
        assert_eq!(report.error_message, "boom");
        assert_eq!(report.frames.len(), 3);
        assert_eq!(report.frames.last().unwrap().file, "/app/lib.py");
    }
}
