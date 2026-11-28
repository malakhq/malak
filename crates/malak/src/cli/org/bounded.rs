//! The same result budget for command-line and MCP consumers.

use serde_json::{Value, json};

const BUDGET: usize = 32 * 1024;

pub(in crate::cli) fn fits(text: &str, value: &Value) -> bool {
    // Match Reply::into_result, including both copies and JSON escaping.
    json!({"content":[{"type":"text","text":text}],"structuredContent":value,"isError":false})
        .to_string()
        .len()
        < BUDGET
}

pub(in crate::cli) fn result(items: Vec<Value>, page_hint: &str) -> Value {
    let total = items.len();
    let mut result = json!({"items":[], "omitted":total, "next":page_hint});
    for item in items {
        result["items"].as_array_mut().expect("array").push(item);
        result["omitted"] = json!(total - result["items"].as_array().expect("array").len());
        if !fits(&result.to_string(), &result) {
            result["items"].as_array_mut().expect("array").pop();
            result["omitted"] = json!(total - result["items"].as_array().expect("array").len());
            break;
        }
    }
    if result["omitted"] == 0 {
        result["next"] = Value::Null;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_counts_utf8_bytes_and_reports_omissions() {
        let result = result(
            vec![json!({"text":"🦀".repeat(9000)})],
            "malak get rail POSITION",
        );
        assert!(result.to_string().len() + 1 < BUDGET);
        assert_eq!(result["omitted"], 1);
        assert!(result["next"].as_str().unwrap().contains("get"));
    }
}
