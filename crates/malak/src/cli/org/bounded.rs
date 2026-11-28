//! The same result budget for command-line and MCP consumers.

use serde_json::{Value, json};

const BUDGET: usize = 32 * 1024;

fn fact_hint(item: &Value, tail: &str) -> String {
    format!(
        "First omitted fact: board {}, channel {}, creation {}, position {}. Use get with board, channel, creation and position; CLI: malak --board BOARD get --agent --creation CREATION CHANNEL POSITION. {tail}",
        item["board"], item["channel"], item["creation"], item["position"]
    )
}

pub(in crate::cli) fn fits(text: &str, value: &Value) -> bool {
    // Match Reply::into_result, including both copies and JSON escaping.
    json!({"content":[{"type":"text","text":text}],"structuredContent":value,"isError":false})
        .to_string()
        .len()
        < BUDGET
}

/// Choose a UTF-8 prefix with a caller-supplied serialized-size check.
pub(in crate::cli) fn prefix(
    text: &str,
    offset: usize,
    limit: usize,
    mut fits: impl FnMut(usize) -> bool,
) -> usize {
    let mut low = offset;
    let mut high = limit.min(text.len());
    while low < high {
        let mut end = low + (high - low).div_ceil(2);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if fits(end) {
            if end == low {
                let next = text[end..]
                    .chars()
                    .next()
                    .map_or(end, |character| end + character.len_utf8());
                if next <= high && fits(next) {
                    low = next;
                    continue;
                }
                break;
            }
            low = end;
        } else {
            high = end.saturating_sub(1);
        }
    }
    while !text.is_char_boundary(low) {
        low -= 1;
    }
    low
}

#[cfg(feature = "mcp")]
fn fits_request(value: &Value, id: &Value) -> bool {
    json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":value.to_string()}],"structuredContent":value,"isError":false}})
        .to_string().len() < BUDGET
}

/// A brief page keeps its item and advances its byte cursor. Dropping the item
/// would discard the continuation that distinguishes paging from a feed.
#[cfg(feature = "mcp")]
fn page_for_request(value: &Value, id: &Value) -> Option<Value> {
    let items = value["items"].as_array()?;
    if items.len() != 1 {
        return None;
    }
    let item = &items[0];
    if item["link"]["channel"] != "briefs" {
        return None;
    }
    let body = item["body"].as_str()?;
    let start = item["offset"].as_u64()?;
    let total = item["body_bytes"].as_u64()?;
    if start.checked_add(body.len() as u64)? > total {
        return None;
    }
    let board = item["link"]["board"].as_str()?;
    let position = item["link"]["position"].as_u64()?;
    let warning = value["next"]
        .as_str()
        .filter(|next| next.starts_with("Rail publication "))
        .and_then(|next| next.lines().next());
    let candidate = |length| {
        let mut page = value.clone();
        let end = start + length as u64;
        let item = &mut page["items"][0];
        item["body"] = json!(&body[..length]);
        item["omitted_bytes"] = json!(total - end);
        let mut next = None;
        if end < total {
            item["fetch"] = json!({"tool":"brief","board":board,"position":position,"offset":end});
            next = Some(format!(
                "malak brief get '{}' {position} --offset {end}",
                board.replace('\'', "'\\''")
            ));
        } else {
            item.as_object_mut().unwrap().remove("fetch");
        }
        if let Some(warning) = warning {
            next = Some(format!("{warning}\n{}", next.unwrap_or_default()));
        }
        page["next"] = json!(next);
        page
    };
    let low = prefix(body, 0, body.len(), |end| fits_request(&candidate(end), id));
    let page = candidate(low);
    (fits_request(&page, id) && (low > 0 || body.is_empty())).then_some(page)
}

#[cfg(feature = "mcp")]
pub(in crate::cli) fn for_request(mut value: Value, id: &Value) -> Value {
    loop {
        if fits_request(&value, id) {
            break;
        }
        if let Some(page) = page_for_request(&value, id) {
            value = page;
            continue;
        }
        let Some(items) = value["items"].as_array_mut() else {
            break;
        };
        let Some(item) = items.pop() else {
            break;
        };
        value["omitted"] = json!(value["omitted"].as_u64().unwrap_or(0) + 1);
        let warning = value["next"]
            .as_str()
            .filter(|next| next.starts_with("Rail publication "))
            .and_then(|next| next.lines().next())
            .map(str::to_owned);
        value["next"] = json!(if item["position"].is_u64() {
            fact_hint(&item, "Then read earlier retained positions.")
        } else if let Some(board) = item["board"]
            .as_str()
            .or_else(|| item["link"]["board"].as_str())
        {
            format!(
                "First omitted board: {board}. Fetch its source with brief get or list it by board."
            )
        } else {
            "Read the linked source facts with get and earlier retained channel positions."
                .to_owned()
        });
        if let Some(warning) = warning {
            value["next"] = json!(format!("{warning}\n{}", value["next"].as_str().unwrap()));
        }
    }
    value
}

pub(in crate::cli) fn result(items: Vec<Value>, page_hint: &str) -> Value {
    let total = items.len();
    let mut result = json!({"items":[], "omitted":total, "next":page_hint});
    for item in items {
        result["items"].as_array_mut().expect("array").push(item);
        result["omitted"] = json!(total - result["items"].as_array().expect("array").len());
        if !fits(&result.to_string(), &result) {
            loop {
                let omitted = result["items"].as_array_mut().expect("array").pop();
                result["omitted"] = json!(total - result["items"].as_array().expect("array").len());
                if let Some(item) = omitted.filter(|item| item["position"].is_u64()) {
                    result["next"] = json!(fact_hint(&item, page_hint));
                }
                if fits(&result.to_string(), &result)
                    || result["items"].as_array().expect("array").is_empty()
                {
                    break;
                }
            }
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

    #[cfg(feature = "mcp")]
    #[test]
    fn request_id_bounding_preserves_lossless_brief_paging() {
        let body = "🦀\"\\é".repeat(9_000);
        let id = json!("request-🦀\"\\".repeat(500));
        let mut offset = 0;
        let mut reconstructed = String::new();
        while offset < body.len() {
            let value = json!({"items":[{"link":{"board":"build","channel":"briefs","creation":"c","position":1},"body":&body[offset..],"body_bytes":body.len(),"offset":offset,"omitted_bytes":0}],"omitted":0,"next":null});
            let page = for_request(value, &id);
            assert!(fits_request(&page, &id));
            assert_eq!(page["items"].as_array().unwrap().len(), 1);
            let item = &page["items"][0];
            let shown = item["body"].as_str().unwrap();
            assert!(!shown.is_empty());
            reconstructed.push_str(shown);
            offset += shown.len();
            assert_eq!(item["omitted_bytes"], body.len() - offset);
            if offset < body.len() {
                assert_eq!(item["fetch"]["offset"], offset);
            } else {
                assert!(item.get("fetch").is_none());
            }
        }
        assert_eq!(reconstructed, body);
    }

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

    #[cfg(feature = "mcp")]
    #[test]
    fn the_sdk_serialized_result_including_both_copies_fits() {
        let items=(0..100).map(|position| json!({"board":"work","channel":"rail","position":position,"text":"\"\\🦀".repeat(90)})).collect();
        let value = result(items, "Use get on this address, then earlier positions.");
        let mut envelope = rmcp::model::CallToolResult::structured(value.clone());
        envelope.content = vec![rmcp::model::ContentBlock::text(value.to_string())];
        assert!(serde_json::to_vec(&envelope).unwrap().len() < BUDGET);
        assert!(value["omitted"].as_u64().unwrap() > 0);
        assert!(
            value["next"]
                .as_str()
                .unwrap()
                .contains("First omitted fact")
        );
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn the_full_response_includes_the_actual_request_id_in_its_budget() {
        let id = json!("quoted\\\"".repeat(100));
        let value = for_request(
            result(
                (0..100).map(|_| json!({"text":"🦀".repeat(300)})).collect(),
                "Read the source positions.",
            ),
            &id,
        );
        let mut response = rmcp::model::CallToolResult::structured(value.clone());
        response.content = vec![rmcp::model::ContentBlock::text(value.to_string())];
        assert!(
            json!({"jsonrpc":"2.0","id":id,"result":response})
                .to_string()
                .len()
                < BUDGET
        );
    }
}
