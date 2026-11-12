use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::io::{self, Read, Write};
use w::{apply, compose, merge, transport_marks, Change, Edit, Gravity, Mark, Segment};

const PRIORITY: &[&str] = &[
    "invalid_json",
    "trailing_input",
    "unknown_operation",
    "unknown_field",
    "missing_field",
    "invalid_field",
    "invalid_gravity",
    "duplicate_mark_id",
    "unsorted_changes",
    "overlapping_changes",
    "position_out_of_range",
];

#[derive(Debug)]
struct Failure {
    code: &'static str,
    message: String,
}

impl Failure {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Default)]
struct Validation {
    errors: Vec<Failure>,
}

impl Validation {
    fn add(&mut self, code: &'static str, message: impl Into<String>) {
        self.errors.push(Failure::new(code, message));
    }

    fn finish(self) -> Result<(), Failure> {
        self.errors
            .into_iter()
            .min_by_key(|error| {
                PRIORITY
                    .iter()
                    .position(|code| *code == error.code)
                    .unwrap_or(PRIORITY.len())
            })
            .map_or(Ok(()), Err)
    }
}

fn object<'a>(
    value: &'a Value,
    path: &str,
    check: &mut Validation,
) -> Option<&'a Map<String, Value>> {
    match value.as_object() {
        Some(value) => Some(value),
        None => {
            check.add("invalid_field", format!("{path} must be an object"));
            None
        }
    }
}

fn fields<'a>(
    value: &'a Value,
    path: &str,
    allowed: &[&str],
    required: &[&str],
    check: &mut Validation,
) -> Option<&'a Map<String, Value>> {
    let map = object(value, path, check)?;
    for key in map.keys() {
        if !allowed.contains(&key.as_str()) {
            check.add("unknown_field", format!("unknown field {path}.{key}"));
        }
    }
    for key in required {
        if !map.contains_key(*key) {
            check.add("missing_field", format!("missing field {path}.{key}"));
        }
    }
    Some(map)
}

fn string(
    map: &Map<String, Value>,
    key: &str,
    path: &str,
    check: &mut Validation,
) -> Option<String> {
    map.get(key).and_then(|value| {
        value.as_str().map(str::to_owned).or_else(|| {
            check.add("invalid_field", format!("{path}.{key} must be a string"));
            None
        })
    })
}

fn position(
    map: &Map<String, Value>,
    key: &str,
    path: &str,
    check: &mut Validation,
) -> Option<usize> {
    map.get(key).and_then(|value| {
        value
            .as_u64()
            .and_then(|number| usize::try_from(number).ok())
            .or_else(|| {
                check.add(
                    "invalid_field",
                    format!("{path}.{key} must be a nonnegative integer"),
                );
                None
            })
    })
}

fn read_edit(
    value: &Value,
    path: &str,
    length: Option<usize>,
    check: &mut Validation,
) -> Option<Edit> {
    let map = fields(value, path, &["changes"], &["changes"], check)?;
    let changes = match map.get("changes") {
        Some(Value::Array(changes)) => changes,
        Some(_) => {
            check.add("invalid_field", format!("{path}.changes must be an array"));
            return None;
        }
        None => return None,
    };
    let mut output = Vec::new();
    let mut previous: Option<Change> = None;
    for (index, item) in changes.iter().enumerate() {
        let item_path = format!("{path}.changes[{index}]");
        let Some(change_map) = fields(
            item,
            &item_path,
            &["start", "end", "text"],
            &["start", "end", "text"],
            check,
        ) else {
            continue;
        };
        let start = position(change_map, "start", &item_path, check);
        let end = position(change_map, "end", &item_path, check);
        let text = string(change_map, "text", &item_path, check);
        if let (Some(start), Some(end)) = (start, end) {
            if start > end {
                check.add("invalid_field", format!("{item_path}.start exceeds end"));
            }
            if let Some(length) = length {
                if start > length || end > length {
                    check.add(
                        "position_out_of_range",
                        format!("{item_path} exceeds text length"),
                    );
                }
            }
            if let Some(ref prior) = previous {
                if start < prior.start {
                    check.add(
                        "unsorted_changes",
                        format!("{item_path} starts before its predecessor"),
                    );
                } else if start < prior.end
                    || (start == prior.start && start == end && prior.start == prior.end)
                {
                    check.add(
                        "overlapping_changes",
                        format!("{item_path} overlaps its predecessor"),
                    );
                }
            }
            if let Some(text) = text {
                let change = Change { start, end, text };
                previous = Some(change.clone());
                output.push(change);
            }
        }
    }
    Some(Edit { changes: output })
}

fn read_marks(value: &Value, length: Option<usize>, check: &mut Validation) -> Option<Vec<Mark>> {
    let Value::Array(items) = value else {
        check.add("invalid_field", "marks must be an array");
        return None;
    };
    let mut output = Vec::new();
    let mut ids = HashSet::new();
    for (index, item) in items.iter().enumerate() {
        let path = format!("marks[{index}]");
        let Some(map) = fields(
            item,
            &path,
            &["id", "pos", "gravity"],
            &["id", "pos"],
            check,
        ) else {
            continue;
        };
        let id = string(map, "id", &path, check);
        let pos = position(map, "pos", &path, check);
        let gravity = match map.get("gravity") {
            None => Some(Gravity::Right),
            Some(Value::String(value)) if value == "left" => Some(Gravity::Left),
            Some(Value::String(value)) if value == "right" => Some(Gravity::Right),
            Some(Value::String(_)) => {
                check.add(
                    "invalid_gravity",
                    format!("{path}.gravity must be left or right"),
                );
                None
            }
            Some(_) => {
                check.add("invalid_field", format!("{path}.gravity must be a string"));
                None
            }
        };
        if let Some(ref id) = id {
            if !ids.insert(id.clone()) {
                check.add("duplicate_mark_id", format!("duplicate mark id {id}"));
            }
        }
        if let (Some(pos), Some(length)) = (pos, length) {
            if pos > length {
                check.add(
                    "position_out_of_range",
                    format!("{path}.pos exceeds text length"),
                );
            }
        }
        if let (Some(id), Some(pos), Some(gravity)) = (id, pos, gravity) {
            output.push(Mark { id, pos, gravity });
        }
    }
    Some(output)
}

fn parse_json(input: &str) -> Result<Value, Failure> {
    let mut deserializer = serde_json::Deserializer::from_str(input);
    let value = serde::Deserialize::deserialize(&mut deserializer)
        .map_err(|error| Failure::new("invalid_json", error.to_string()))?;
    deserializer
        .end()
        .map_err(|_| Failure::new("trailing_input", "non-whitespace follows the JSON object"))?;
    Ok(value)
}

fn run(operation: &str, input: &str) -> Result<Value, Failure> {
    let value = parse_json(input)?;
    let mut check = Validation::default();
    let expected: &[&str] = match operation {
        "apply" => &["text", "edit"],
        "compose" => &["text", "a", "b"],
        "marks" => &["text", "marks", "edit"],
        "merge" => &["base", "a", "b"],
        _ => {
            check.add(
                "unknown_operation",
                format!("unknown operation {operation}"),
            );
            &[]
        }
    };
    let map = fields(&value, "input", expected, expected, &mut check);
    let mut base = None;
    let mut edit = None;
    let mut a = None;
    let mut b = None;
    let mut marks = None;
    if let Some(map) = map {
        let text_key = if operation == "merge" { "base" } else { "text" };
        base = string(map, text_key, "input", &mut check);
        let length = base.as_ref().map(|text| text.chars().count());
        if let Some(value) = map.get("edit") {
            edit = read_edit(value, "edit", length, &mut check);
        }
        if let Some(value) = map.get("a") {
            a = read_edit(value, "a", length, &mut check);
        }
        if let Some(value) = map.get("b") {
            let b_length = if operation == "compose" {
                if check.errors.is_empty() {
                    base.as_ref()
                        .zip(a.as_ref())
                        .map(|(text, a)| apply(text, a).chars().count())
                } else {
                    None
                }
            } else {
                length
            };
            b = read_edit(value, "b", b_length, &mut check);
        }
        if let Some(value) = map.get("marks") {
            marks = read_marks(value, length, &mut check);
        }
    }
    check.finish()?;
    let base = base.expect("validated text");
    Ok(match operation {
        "apply" => json!({"text": apply(&base, &edit.expect("validated edit"))}),
        "compose" => {
            let composed = compose(&base, &a.expect("validated a"), &b.expect("validated b"));
            json!({"edit": edit_json(&composed)})
        }
        "marks" => {
            let moved = transport_marks(
                &marks.expect("validated marks"),
                &edit.expect("validated edit"),
            );
            let values: Vec<Value> = moved
                .iter()
                .map(|mark| json!({"id":mark.id,"pos":mark.pos,"gravity": if mark.gravity == Gravity::Left {"left"} else {"right"}}))
                .collect();
            json!({"marks":values})
        }
        "merge" => {
            let merged = merge(&base, &a.expect("validated a"), &b.expect("validated b"));
            let segments: Vec<Value> = merged
                .segments
                .iter()
                .map(|segment| match segment {
                    Segment::Text(text) => json!({"text":text}),
                    Segment::Conflict { base, a, b } => {
                        json!({"conflict":{"base":base,"a":a,"b":b}})
                    }
                })
                .collect();
            json!({"segments":segments,"conflicts":merged.conflicts})
        }
        _ => unreachable!("unknown operation rejected"),
    })
}

fn edit_json(edit: &Edit) -> Value {
    let changes: Vec<Value> = edit
        .changes
        .iter()
        .map(|change| json!({"start":change.start,"end":change.end,"text":change.text}))
        .collect();
    json!({"changes":changes})
}

fn main() {
    let operation = std::env::args().nth(1).unwrap_or_default();
    let mut input = String::new();
    if let Err(error) = io::stdin().read_to_string(&mut input) {
        emit_failure(Failure::new("invalid_json", error.to_string()), 2);
    }
    match run(&operation, &input) {
        Ok(output) => {
            let stdout = io::stdout();
            let mut writer = stdout.lock();
            if let Err(error) = writeln!(writer, "{output}").and_then(|_| writer.flush()) {
                emit_failure(Failure::new("internal_error", error.to_string()), 1);
            }
        }
        Err(error) => emit_failure(error, 2),
    }
}

fn emit_failure(error: Failure, exit_code: i32) -> ! {
    eprintln!("{}", failure_json(&error));
    std::process::exit(exit_code);
}

fn failure_json(error: &Failure) -> Value {
    json!({"error":{"code":error.code,"message":error.message}})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(operation: &str, input: &str) -> &'static str {
        run(operation, input).unwrap_err().code
    }

    #[test]
    fn cli_shapes_and_multibyte_positions() {
        assert_eq!(
            run(
                "apply",
                r#"{"text":"甲乙","edit":{"changes":[{"start":1,"end":2,"text":"🌿"}]}}"#
            )
            .unwrap(),
            json!({"text":"甲🌿"})
        );
        assert_eq!(run("compose", r#"{"text":"甲乙","a":{"changes":[{"start":0,"end":1,"text":"丁"}]},"b":{"changes":[{"start":1,"end":2,"text":"戊"}]}}"#).unwrap(), json!({"edit":{"changes":[{"start":0,"end":2,"text":"丁戊"}]}}));
        assert_eq!(run("marks", r#"{"text":"甲","marks":[{"id":"x","pos":0}],"edit":{"changes":[{"start":0,"end":0,"text":"乙"}]}}"#).unwrap(), json!({"marks":[{"id":"x","pos":1,"gravity":"right"}]}));
        assert_eq!(run("merge", r#"{"base":"甲乙丙","a":{"changes":[{"start":0,"end":2,"text":"A"}]},"b":{"changes":[{"start":1,"end":3,"text":"B"}]}}"#).unwrap(), json!({"segments":[{"conflict":{"base":"甲乙丙","a":"A丙","b":"甲B"}}],"conflicts":1}));
    }

    #[test]
    fn error_priority_and_validation() {
        assert_eq!(code("apply", "{"), "invalid_json");
        assert_eq!(code("apply", "{} false"), "trailing_input");
        assert_eq!(code("other", r#"{"extra":1}"#), "unknown_operation");
        assert_eq!(
            code(
                "apply",
                r#"{"text":"甲","edit":{"changes":[{"start":-1,"end":0,"text":""}]},"extra":1}"#
            ),
            "unknown_field"
        );
        assert_eq!(
            code(
                "marks",
                r#"{"text":"甲","marks":[{"id":"x","pos":0,"gravity":"up"}],"extra":0}"#
            ),
            "unknown_field"
        );
        assert_eq!(code("apply", r#"{"text":"甲"}"#), "missing_field");
        assert_eq!(
            code(
                "apply",
                r#"{"text":"甲","edit":{"changes":[{"start":-1,"end":0,"text":""}]}}"#
            ),
            "invalid_field"
        );
        assert_eq!(
            code(
                "marks",
                r#"{"text":"甲","marks":[{"id":"x","pos":0,"gravity":"up"}],"edit":{"changes":[]}}"#
            ),
            "invalid_gravity"
        );
        assert_eq!(
            code(
                "marks",
                r#"{"text":"甲","marks":[{"id":"x","pos":0},{"id":"x","pos":1}],"edit":{"changes":[]}}"#
            ),
            "duplicate_mark_id"
        );
        assert_eq!(
            code(
                "apply",
                r#"{"text":"甲乙","edit":{"changes":[{"start":1,"end":1,"text":"a"},{"start":0,"end":0,"text":"b"}]}}"#
            ),
            "unsorted_changes"
        );
        assert_eq!(
            code(
                "apply",
                r#"{"text":"甲乙","edit":{"changes":[{"start":0,"end":2,"text":"a"},{"start":1,"end":2,"text":"b"}]}}"#
            ),
            "overlapping_changes"
        );
        assert_eq!(
            code(
                "apply",
                r#"{"text":"甲","edit":{"changes":[{"start":2,"end":2,"text":"a"}]}}"#
            ),
            "position_out_of_range"
        );
    }

    #[test]
    fn output_field_order_is_stable() {
        assert_eq!(
            run(
                "compose",
                r#"{"text":"甲乙","a":{"changes":[{"start":0,"end":1,"text":"丁"}]},"b":{"changes":[{"start":1,"end":2,"text":"戊"}]}}"#,
            )
            .unwrap()
            .to_string(),
            r#"{"edit":{"changes":[{"start":0,"end":2,"text":"丁戊"}]}}"#,
        );
        assert_eq!(
            run(
                "marks",
                r#"{"text":"甲","marks":[{"id":"m","pos":0}],"edit":{"changes":[{"start":0,"end":0,"text":"乙"}]}}"#,
            )
            .unwrap()
            .to_string(),
            r#"{"marks":[{"id":"m","pos":1,"gravity":"right"}]}"#,
        );
        assert_eq!(
            run(
                "merge",
                r#"{"base":"甲乙丙","a":{"changes":[{"start":0,"end":2,"text":"A"}]},"b":{"changes":[{"start":1,"end":3,"text":"B"}]}}"#,
            )
            .unwrap()
            .to_string(),
            r#"{"segments":[{"conflict":{"base":"甲乙丙","a":"A丙","b":"甲B"}}],"conflicts":1}"#,
        );
        assert_eq!(
            failure_json(&Failure::new("invalid_field", "bad input")).to_string(),
            r#"{"error":{"code":"invalid_field","message":"bad input"}}"#,
        );
    }
}
