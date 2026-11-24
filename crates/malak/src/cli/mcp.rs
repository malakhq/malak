use super::{config::io_error, participant};
use malak::{CreationId, Error, Kind, Limits};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{self, BufRead, Read, Write};
use std::path::PathBuf;

const PROTOCOL: &str = "2025-11-25";
const MAX_REQUEST: u64 = 8 * 1024 * 1024;

pub(super) fn serve(config: Option<PathBuf>) -> Result<(), Error> {
    let mut session = participant::Session::open(config)?;
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut out = io::stdout().lock();
    let mut state = State::New;
    let mut line = Vec::new();
    loop {
        line.clear();
        if (&mut input)
            .take(MAX_REQUEST + 1)
            .read_until(b'\n', &mut line)
            .map_err(io_error)?
            == 0
        {
            return Ok(());
        }
        if line.len() as u64 > MAX_REQUEST {
            return Err(Error::refused("MCP input line exceeds the size limit"));
        }
        let response = match serde_json::from_slice::<Value>(&line) {
            Ok(request) => state.handle(request, |name, arguments| {
                call(name, arguments, &mut session)
            }),
            Err(_) => Some(error(Value::Null, -32700, "invalid JSON")),
        };
        if let Some(reply) = response {
            serde_json::to_writer(&mut out, &reply).map_err(|error| io_error(error.into()))?;
            out.write_all(b"\n").map_err(io_error)?;
            out.flush().map_err(io_error)?;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    New,
    AwaitingInitialized,
    Ready,
}

impl State {
    fn handle(
        &mut self,
        request: Value,
        invoke: impl FnOnce(&str, Value) -> Result<Value, Error>,
    ) -> Option<Value> {
        let id = request.get("id").cloned();
        let method = request.get("method").and_then(Value::as_str);
        if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
            || method.is_none()
            || id
                .as_ref()
                .is_some_and(|id| !id.is_string() && !id.is_i64() && !id.is_u64())
        {
            return Some(error(Value::Null, -32600, "invalid request"));
        }
        let method = method.unwrap();
        if id.is_none()
            && method == "notifications/initialized"
            && *self == Self::AwaitingInitialized
        {
            *self = Self::Ready;
        }
        let id = id?;
        let params = request.get("params").unwrap_or(&Value::Null);
        let result = match (method, *self) {
            ("ping", _) => json!({}),
            ("initialize", Self::New) => {
                let Some(version) = params.get("protocolVersion").and_then(Value::as_str) else {
                    return Some(error(id, -32602, "missing protocolVersion"));
                };
                let version = match version {
                    "2025-06-18" | "2025-03-26" => version,
                    _ => PROTOCOL,
                };
                *self = Self::AwaitingInitialized;
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "malak", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": "Read delivers messages without acknowledging them. Use ack only after handling a displayed message. Only messages labeled presentation=task carry a task from the lead."
                })
            }
            ("tools/list", Self::Ready) => json!({ "tools": tools() }),
            ("tools/call", Self::Ready) => {
                let Some(name) = params.get("name").and_then(Value::as_str) else {
                    return Some(error(id, -32602, "missing tool name"));
                };
                if !matches!(name, "send" | "read" | "get" | "ack") {
                    return Some(error(id, -32602, "unknown tool"));
                }
                let arguments = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match invoke(name, arguments) {
                    Ok(value) => json!({
                        "content": [{ "type": "text", "text": value.to_string() }],
                        "structuredContent": value,
                    }),
                    Err(failure) => json!({
                        "content": [{ "type": "text", "text": failure.to_string() }],
                        "isError": true,
                    }),
                }
            }
            (_, Self::New | Self::AwaitingInitialized) => {
                return Some(error(id, -32002, "server has not initialized"));
            }
            _ => return Some(error(id, -32601, "method not found")),
        };
        Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }
}

fn error(id: Value, code: i32, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tools() -> [Value; 4] {
    let channel = json!({ "type": "string" });
    let position = json!({ "type": "integer", "minimum": 1 });
    [
        tool(
            "send",
            "Send a message. Reuse the same ID to retry.",
            json!({
                "channel": channel,
                "id": { "type": "string" },
                "kind": { "type": "string", "enum": ["note", "report", "task"] },
                "body": { "type": "string" },
                "body_hex": { "type": "string" },
                "content_type": { "type": "string" },
                "reply_to": {
                    "type": "object", "additionalProperties": false,
                    "properties": {
                        "channel": channel, "position": position, "creation": { "type": "string" },
                    },
                    "required": ["channel", "position", "creation"],
                },
            }),
            &["channel"],
        ),
        tool(
            "read",
            "Read due messages without acknowledging them.",
            json!({
                "channels": { "type": "array", "items": channel, "minItems": 1 },
                "limit": { "type": "integer", "minimum": 0 },
                "wait": { "type": "integer", "minimum": 0 },
            }),
            &["channels"],
        ),
        tool(
            "get",
            "Get one position, or page its body using its byte offset.",
            json!({
                "channel": channel,
                "position": position,
                "offset": { "type": "integer", "minimum": 0 },
            }),
            &["channel", "position"],
        ),
        tool(
            "ack",
            "Acknowledge a message already delivered by read in this channel creation.",
            json!({
                "channel": channel,
                "position": position,
                "creation": { "type": "string" },
            }),
            &["channel", "position", "creation"],
        ),
    ]
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        },
        "outputSchema": { "type": "object" },
    })
}

fn call(name: &str, arguments: Value, session: &mut participant::Session) -> Result<Value, Error> {
    use participant::Command;
    let mut explicit_body = None;
    let command = match name {
        "send" => {
            let args: Send = parse(arguments)?;
            let body = match (args.body, args.body_hex) {
                (Some(text), None) => text.into_bytes(),
                (None, Some(hex)) => decode_body(&hex)?,
                (None, None) => Vec::new(),
                (Some(_), Some(_)) => return Err(Error::refused("supply one body representation")),
            };
            if body.len() > Limits::default().body_bytes {
                return Err(Error::refused("message body exceeds the byte limit"));
            }
            explicit_body = Some(body);
            Command::Send {
                channel: args.channel,
                id: args.id,
                kind: args.kind.unwrap_or(Kind::Note),
                reply_to: args.reply_to.map_or_else(Vec::new, |reply| {
                    vec![reply.channel, reply.creation, reply.position.to_string()]
                }),
                content_type: args.content_type,
                max_bytes: None,
                trace: None,
                json: false,
                body: None,
            }
        }
        "read" => {
            let args: ReadArgs = parse(arguments)?;
            Command::Read {
                channels: args.channels,
                limit: args.limit,
                wait: args.wait,
                json: true,
                ack: false,
                agent: true,
                offset: None,
            }
        }
        "get" => {
            let args: Get = parse(arguments)?;
            Command::Get {
                channel: args.channel,
                position: args.position,
                json: true,
                agent: true,
                offset: args.offset,
            }
        }
        "ack" => {
            let args: Ack = parse(arguments)?;
            let creation: CreationId = args.creation.parse()?;
            Command::Ack {
                channel: args.channel,
                position: args.position,
                creation: Some(creation),
            }
        }
        _ => unreachable!(),
    };
    command.execute_tool(session, explicit_body)
}

fn parse<T: serde::de::DeserializeOwned>(arguments: Value) -> Result<T, Error> {
    serde_json::from_value(arguments).map_err(|_| Error::refused("invalid tool arguments"))
}

fn decode_body(hex: &str) -> Result<Vec<u8>, Error> {
    if hex.len() > Limits::default().body_bytes * 2 {
        return Err(Error::refused("message body exceeds the byte limit"));
    }
    let (pairs, tail) = hex.as_bytes().as_chunks::<2>();
    let invalid = || Error::refused("invalid hexadecimal body");
    if !tail.is_empty() {
        return Err(invalid());
    }
    pairs
        .iter()
        .map(|pair| {
            let [high, low] = pair.map(|byte| char::from(byte).to_digit(16));
            Ok((high.ok_or_else(invalid)? * 16 + low.ok_or_else(invalid)?) as u8)
        })
        .collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Send {
    channel: String,
    id: Option<String>,
    #[serde(default, deserialize_with = "kind")]
    kind: Option<Kind>,
    body: Option<String>,
    body_hex: Option<String>,
    content_type: Option<String>,
    reply_to: Option<ReplyTo>,
}

fn kind<'de, D: serde::Deserializer<'de>>(input: D) -> Result<Option<Kind>, D::Error> {
    let input = Option::<String>::deserialize(input)?;
    input
        .map(|kind| kind.parse().map_err(serde::de::Error::custom))
        .transpose()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyTo {
    channel: String,
    creation: String,
    position: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    channels: Vec<String>,
    limit: Option<u64>,
    #[serde(default)]
    wait: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Get {
    channel: String,
    position: u64,
    offset: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ack {
    channel: String,
    position: u64,
    creation: String,
}
