use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::ffi::OsString;
use std::path::Path;

use serde_json::{Map, Number, Value};

use crate::daemon::{self, CachedTool};

pub(crate) async fn run_tool_command(
    args: &[OsString],
    socket_override: Option<&Path>,
    url: &str,
) -> Result<(), Box<dyn Error>> {
    let tool_name = args
        .first()
        .ok_or("missing tool name")?
        .to_string_lossy()
        .into_owned();
    let tool = find_tool(socket_override, url, &tool_name)?
        .ok_or_else(|| format!("unknown tool command `{tool_name}`"))?;

    if should_print_tool_help(args) {
        print!("{}", render_tool_help(&tool));
        return Ok(());
    }

    let arguments = parse_tool_arguments(&tool, &args[1..])?;
    let result =
        daemon::call_tool(url, socket_override, &tool.name, Value::Object(arguments)).await?;
    println!(
        "{}",
        serde_json::to_string_pretty(display_tool_result(&result))?
    );
    Ok(())
}

fn find_tool(
    socket_override: Option<&Path>,
    url: &str,
    tool_name: &str,
) -> Result<Option<CachedTool>, Box<dyn Error>> {
    Ok(daemon::read_cached_tools(url, socket_override)?
        .into_iter()
        .find(|tool| tool.name == tool_name))
}

fn should_print_tool_help(args: &[OsString]) -> bool {
    args.iter().skip(1).any(|arg| {
        let arg = arg.to_string_lossy();
        arg == "-h" || arg == "--help"
    })
}

fn parse_tool_arguments(
    tool: &CachedTool,
    args: &[OsString],
) -> Result<Map<String, Value>, Box<dyn Error>> {
    let properties = tool_properties(tool);
    let required = required_properties(tool);
    let raw_values = collect_raw_parameter_values(tool, args, &properties)?;
    let mut arguments = Map::new();

    for (name, values) in &raw_values {
        let schema = properties
            .get(name.as_str())
            .copied()
            .ok_or_else(|| format!("unknown parameter `--{name}` for tool `{}`", tool.name))?;
        arguments.insert(name.clone(), parse_parameter_values(name, schema, values)?);
    }

    let missing = required
        .into_iter()
        .filter(|name| !arguments.contains_key(name))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(format!(
            "missing required parameters for `{}`: {}. Use `ones-mcp-cli {} --help`.",
            tool.name,
            missing.join(", "),
            tool.name
        )
        .into());
    }

    Ok(arguments)
}

fn collect_raw_parameter_values(
    tool: &CachedTool,
    args: &[OsString],
    properties: &BTreeMap<&str, &Value>,
) -> Result<BTreeMap<String, Vec<String>>, Box<dyn Error>> {
    let mut values = BTreeMap::<String, Vec<String>>::new();
    let mut index = 0;

    while index < args.len() {
        let current = args[index].to_string_lossy();
        if current == "-h" || current == "--help" {
            index += 1;
            continue;
        }

        if !current.starts_with("--") || current.len() <= 2 {
            return Err(format!(
                "invalid argument `{current}` for tool `{}`. Expected `--<parameter> <value>`.",
                tool.name
            )
            .into());
        }

        let (name, value, consumed_next) = if let Some((name, value)) = current[2..].split_once('=')
        {
            (name.to_owned(), value.to_owned(), false)
        } else {
            let name = current[2..].to_owned();
            let value = args
                .get(index + 1)
                .ok_or_else(|| format!("missing value for parameter `--{name}`"))?
                .to_string_lossy()
                .into_owned();
            (name, value, true)
        };

        if !properties.contains_key(name.as_str()) {
            return Err(format!(
                "unknown parameter `--{name}` for tool `{}`. Use `ones-mcp-cli {} --help`.",
                tool.name, tool.name
            )
            .into());
        }

        values.entry(name).or_default().push(value);
        index += if consumed_next { 2 } else { 1 };
    }

    Ok(values)
}

fn parse_parameter_values(
    name: &str,
    schema: &Value,
    values: &[String],
) -> Result<Value, Box<dyn Error>> {
    if is_array_schema(schema) {
        let item_schema = schema
            .get("items")
            .ok_or_else(|| format!("parameter `--{name}` is missing an array item schema"))?;
        let mut items = Vec::with_capacity(values.len());
        for value in values {
            items.push(parse_single_parameter_value(name, item_schema, value)?);
        }
        return Ok(Value::Array(items));
    }

    if values.len() != 1 {
        return Err(format!("parameter `--{name}` only accepts one value").into());
    }

    parse_single_parameter_value(name, schema, &values[0])
}

fn parse_single_parameter_value(
    name: &str,
    schema: &Value,
    raw: &str,
) -> Result<Value, Box<dyn Error>> {
    if let Some(candidates) = schema_candidates(schema) {
        let mut string_error = None;

        for candidate in candidates {
            match parse_single_parameter_value(name, candidate, raw) {
                Ok(value) => return Ok(value),
                Err(error) if candidate_type(candidate) == Some("string") => {
                    string_error = Some(error);
                }
                Err(_) => {}
            }
        }

        if let Some(error) = string_error {
            return Err(error);
        }

        return Err(format!("invalid value `{raw}` for parameter `--{name}`").into());
    }

    if let Some(enum_values) = schema.get("enum").and_then(Value::as_array) {
        if enum_values.iter().any(|item| item.as_str() == Some(raw)) {
            return Ok(Value::String(raw.to_owned()));
        }

        return Err(format!(
            "invalid value `{raw}` for parameter `--{name}`. Expected one of: {}",
            enum_values
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        )
        .into());
    }

    match candidate_type(schema) {
        Some("string") | None => Ok(Value::String(raw.to_owned())),
        Some("number") => parse_number_value(name, raw),
        Some("integer") => parse_integer_value(name, raw),
        Some("boolean") => parse_boolean_value(name, raw),
        Some("object") => Ok(serde_json::from_str(raw)
            .map_err(|error| format!("invalid JSON object for parameter `--{name}`: {error}"))?),
        Some("array") => Err(format!(
            "parameter `--{name}` must be provided multiple times instead of as a single JSON array"
        )
        .into()),
        Some(other) => {
            Err(format!("unsupported schema type `{other}` for parameter `--{name}`").into())
        }
    }
}

fn parse_number_value(name: &str, raw: &str) -> Result<Value, Box<dyn Error>> {
    let number = raw
        .parse::<f64>()
        .map_err(|_| format!("invalid number `{raw}` for parameter `--{name}`"))?;
    let number = Number::from_f64(number)
        .ok_or_else(|| format!("invalid number `{raw}` for parameter `--{name}`"))?;
    Ok(Value::Number(number))
}

fn parse_integer_value(name: &str, raw: &str) -> Result<Value, Box<dyn Error>> {
    if let Ok(value) = raw.parse::<i64>() {
        return Ok(Value::Number(Number::from(value)));
    }

    if let Ok(value) = raw.parse::<u64>() {
        return Ok(Value::Number(Number::from(value)));
    }

    Err(format!("invalid integer `{raw}` for parameter `--{name}`").into())
}

fn parse_boolean_value(name: &str, raw: &str) -> Result<Value, Box<dyn Error>> {
    match raw {
        "true" => Ok(Value::Bool(true)),
        "false" => Ok(Value::Bool(false)),
        _ => Err(format!(
            "invalid boolean `{raw}` for parameter `--{name}`. Use `true` or `false`."
        )
        .into()),
    }
}

fn schema_candidates(schema: &Value) -> Option<Vec<&Value>> {
    let mut candidates = schema
        .get("anyOf")
        .or_else(|| schema.get("oneOf"))
        .and_then(Value::as_array)?
        .iter()
        .filter(|candidate| candidate_type(candidate) != Some("null"))
        .collect::<Vec<_>>();
    candidates.sort_by_key(candidate_priority);
    Some(candidates)
}

fn candidate_priority(schema: &&Value) -> usize {
    match candidate_type(schema) {
        Some("boolean") => 0,
        Some("integer") => 1,
        Some("number") => 2,
        Some("object") => 3,
        Some("array") => 4,
        Some("string") => 5,
        Some(_) | None => 6,
    }
}

fn candidate_type(schema: &Value) -> Option<&str> {
    schema.get("type").and_then(Value::as_str)
}

fn is_array_schema(schema: &Value) -> bool {
    if candidate_type(schema) == Some("array") {
        return true;
    }

    schema_candidates(schema)
        .map(|candidates| candidates.into_iter().any(is_array_schema))
        .unwrap_or(false)
}

fn tool_properties<'a>(tool: &'a CachedTool) -> BTreeMap<&'a str, &'a Value> {
    tool.input_schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| {
            properties
                .iter()
                .map(|(name, schema)| (name.as_str(), schema))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default()
}

fn required_properties(tool: &CachedTool) -> BTreeSet<String> {
    tool.input_schema
        .get("required")
        .and_then(Value::as_array)
        .map(|required| {
            required
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default()
}

fn render_tool_help(tool: &CachedTool) -> String {
    let properties = tool_properties(tool);
    let required = required_properties(tool);
    let parameters = properties
        .into_iter()
        .map(|(name, schema)| ToolParameterHelp {
            name: name.to_owned(),
            value_hint: parameter_value_hint(schema),
            description: parameter_description(schema, required.contains(name)),
        })
        .collect::<Vec<_>>();
    let width = parameters
        .iter()
        .map(|parameter| {
            format!("--{} {}", parameter.name, parameter.value_hint)
                .chars()
                .count()
        })
        .max()
        .unwrap_or(0);
    let mut output = String::new();

    if let Some(description) = tool.description.as_deref() {
        output.push_str(&normalize_description(description));
        output.push_str("\n\n");
    }

    output.push_str(&format!(
        "Usage: ones-mcp-cli {} [--<parameter> <value>]\n",
        tool.name
    ));

    if !parameters.is_empty() {
        output.push_str("\nParameters:\n");
        for parameter in parameters {
            let label = format!("--{} {}", parameter.name, parameter.value_hint);
            output.push_str(&format!(
                "  {:width$}  {}\n",
                label,
                parameter.description,
                width = width
            ));
        }
    }

    output
}

fn parameter_value_hint(schema: &Value) -> String {
    if is_array_schema(schema) {
        let item_schema = schema
            .get("items")
            .or_else(|| {
                schema_candidates(schema)
                    .and_then(|candidates| {
                        candidates
                            .into_iter()
                            .find(|candidate| candidate_type(candidate) == Some("array"))
                    })
                    .and_then(|array_schema| array_schema.get("items"))
            })
            .unwrap_or(&Value::Null);
        return format!("<{}>...", scalar_value_hint(item_schema));
    }

    format!("<{}>", scalar_value_hint(schema))
}

fn scalar_value_hint(schema: &Value) -> &'static str {
    if let Some(candidates) = schema_candidates(schema) {
        for candidate in candidates {
            let hint = scalar_value_hint(candidate);
            if hint != "VALUE" {
                return hint;
            }
        }
        return "VALUE";
    }

    match candidate_type(schema) {
        Some("string") => "STRING",
        Some("number") => "NUMBER",
        Some("integer") => "INTEGER",
        Some("boolean") => "BOOLEAN",
        Some("object") => "JSON",
        Some("array") => "VALUE",
        Some(_) | None => "VALUE",
    }
}

fn parameter_description(schema: &Value, required: bool) -> String {
    let mut description = schema
        .get("description")
        .and_then(Value::as_str)
        .map(normalize_description)
        .unwrap_or_else(|| "No description.".to_owned());

    if required {
        description.push_str(" [required]");
    }

    if let Some(enum_values) = schema.get("enum").and_then(Value::as_array) {
        let values = enum_values
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        if !values.is_empty() {
            description.push_str(&format!(" Allowed values: {}.", values.join(", ")));
        }
    }

    description
}

fn normalize_description(description: &str) -> String {
    description.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn display_tool_result<'a>(result: &'a Value) -> &'a Value {
    result.get("structuredContent").unwrap_or(result)
}

struct ToolParameterHelp {
    name: String,
    value_hint: String,
    description: String,
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use serde_json::json;

    use super::{
        CachedTool, display_tool_result, parameter_value_hint, parse_tool_arguments,
        render_tool_help,
    };

    fn sample_tool() -> CachedTool {
        CachedTool {
            name: "sample".to_owned(),
            description: Some("Sample tool".to_owned()),
            input_schema: json!({
                "type": "object",
                "required": ["issueID", "members"],
                "properties": {
                    "issueID": {
                        "type": "string",
                        "description": "Issue identifier"
                    },
                    "hours": {
                        "type": "number",
                        "description": "Hours"
                    },
                    "includeClosed": {
                        "type": "boolean",
                        "description": "Whether to include closed issues"
                    },
                    "members": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Member IDs"
                    }
                }
            }),
        }
    }

    #[test]
    fn parses_number_boolean_and_array_arguments() {
        let tool = sample_tool();
        let arguments = parse_tool_arguments(
            &tool,
            &[
                OsString::from("--issueID"),
                OsString::from("ISS-1"),
                OsString::from("--hours"),
                OsString::from("1.5"),
                OsString::from("--includeClosed"),
                OsString::from("true"),
                OsString::from("--members"),
                OsString::from("u1"),
                OsString::from("--members"),
                OsString::from("u2"),
            ],
        )
        .expect("expected parsed arguments");

        assert_eq!(arguments.get("issueID"), Some(&json!("ISS-1")));
        assert_eq!(arguments.get("hours"), Some(&json!(1.5)));
        assert_eq!(arguments.get("includeClosed"), Some(&json!(true)));
        assert_eq!(arguments.get("members"), Some(&json!(["u1", "u2"])));
    }

    #[test]
    fn rejects_missing_required_arguments() {
        let error = parse_tool_arguments(
            &sample_tool(),
            &[OsString::from("--issueID"), OsString::from("ISS-1")],
        )
        .expect_err("expected missing argument error");

        assert!(error.to_string().contains("missing required parameters"));
        assert!(error.to_string().contains("members"));
    }

    #[test]
    fn rejects_unknown_arguments() {
        let error = parse_tool_arguments(
            &sample_tool(),
            &[
                OsString::from("--issueID"),
                OsString::from("ISS-1"),
                OsString::from("--members"),
                OsString::from("u1"),
                OsString::from("--unknown"),
                OsString::from("x"),
            ],
        )
        .expect_err("expected unknown argument error");

        assert!(error.to_string().contains("unknown parameter `--unknown`"));
    }

    #[test]
    fn renders_tool_help_with_parameter_list() {
        let help = render_tool_help(&sample_tool());

        assert!(help.contains("Usage: ones-mcp-cli sample"));
        assert!(help.contains("--issueID <STRING>"));
        assert!(help.contains("--members <STRING>..."));
        assert!(help.contains("[required]"));
    }

    #[test]
    fn renders_value_hint_for_arrays() {
        assert_eq!(
            parameter_value_hint(&json!({
                "type": "array",
                "items": { "type": "string" }
            })),
            "<STRING>..."
        );
    }

    #[test]
    fn prefers_structured_content_when_present() {
        let result = json!({
            "content": [
                {
                    "type": "text",
                    "text": "{\"name\":\"alice\"}"
                }
            ],
            "structuredContent": {
                "name": "alice"
            }
        });

        assert_eq!(display_tool_result(&result), &json!({ "name": "alice" }));
    }
}
