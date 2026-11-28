//! Organisation views are reading clients of ordinary board messages.

pub(in crate::cli) mod bounded;
pub(in crate::cli) mod lifecycle;
pub(in crate::cli) mod pulse;
pub(in crate::cli) mod rail;
pub(in crate::cli) mod records;
pub(in crate::cli) mod seats;
pub(in crate::cli) mod typed;

pub(in crate::cli) fn print(
    items: Vec<serde_json::Value>,
    json: bool,
    page_hint: &str,
) -> Result<(), malak::Error> {
    let result = bounded::result(items, page_hint);
    if json {
        return super::output::line(format_args!("{result}"));
    }
    let mut text = String::new();
    for item in result["items"].as_array().into_iter().flatten() {
        if item["colour"].is_string() {
            text.push_str(&format!(
                "{} · {} · {} · {} · blocker {} · last activity {} · {} open / {} calls / {} finished today\n",
                item["lead"].as_str().unwrap_or(""),
                item["colour"].as_str().unwrap_or(""),
                item["on"].as_str().unwrap_or(""),
                item["reason"].as_str().unwrap_or(""),
                item["blocker"],
                super::output::clock(item["last_activity"].as_u64()),
                item["counts"]["started"].as_u64().unwrap_or(0),
                item["counts"]["calls"].as_u64().unwrap_or(0),
                item["counts"]["finished"].as_u64().unwrap_or(0)
            ));
        } else {
            let description = item["event"].as_str().map_or_else(
                || item["text"].as_str().unwrap_or("").to_owned(),
                |event| {
                    format!(
                        "{event} · {} · link {}",
                        item["subject"].as_str().unwrap_or(""),
                        item["link"]
                    )
                },
            );
            text.push_str(&format!(
                "{} · {} #{} · {}\n",
                item["board"].as_str().unwrap_or(""),
                item["channel"].as_str().unwrap_or("rail"),
                item["position"],
                description
            ));
        }
    }
    if result["omitted"].as_u64().unwrap_or(0) > 0 {
        text.push_str(&format!(
            "{} items left out. {}\n",
            result["omitted"],
            result["next"].as_str().unwrap_or(page_hint)
        ));
    }
    super::output::write(&text)
}
