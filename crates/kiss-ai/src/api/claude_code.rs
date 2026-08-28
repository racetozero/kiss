//! Claude Code request compatibility for Anthropic subscription OAuth.

use crate::types::{ContentBlock, Context, Message, UserContent};
use anyhow::{Context as _, Result};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::path::PathBuf;

pub const VERSION: &str = "2.1.224";
pub const ENTRYPOINT: &str = "sdk-cli";
const CCH_PLACEHOLDER: &str = "cch=00000";
const CCH_SEED: u64 = 0x4d65_9218_e32a_3268;
const AGENT_SDK_PROMPT: &str = "You are a Claude agent, built on Anthropic's Claude Agent SDK.";
const LEGACY_PI_PROMPT: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub device_id: String,
    pub account_uuid: String,
}

pub fn xxhash64(bytes: &[u8], seed: u64) -> u64 {
    xxhash_rust::xxh64::xxh64(bytes, seed)
}

fn first_user_prompt(context: &Context) -> String {
    context
        .messages
        .iter()
        .find_map(|message| match message {
            Message::User(user) => Some(match &user.content {
                UserContent::Text(text) => text.clone(),
                UserContent::Blocks(blocks) => blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect(),
            }),
            _ => None,
        })
        .unwrap_or_default()
}

pub fn version_fingerprint(context: &Context) -> String {
    // pi-black runs in JavaScript, where string indexes address UTF-16 code
    // units. Preserve that behavior for prompts that contain non-BMP text.
    // TextEncoder replaces a selected unpaired surrogate with U+FFFD.
    let prompt: Vec<u16> = first_user_prompt(context).encode_utf16().collect();
    let selected: String = [4, 7, 20]
        .into_iter()
        .map(|index| {
            prompt
                .get(index)
                .copied()
                .and_then(|unit| char::from_u32(u32::from(unit)))
                .unwrap_or('\u{fffd}')
        })
        .collect();
    let digest = Sha256::digest(format!("59cf53e54c78{selected}{VERSION}").as_bytes());
    format!("{:02x}{:02x}", digest[0], digest[1])[..3].to_string()
}

fn billing_header(context: &Context) -> String {
    format!(
        "x-anthropic-billing-header: cc_version={VERSION}.{}; cc_entrypoint={ENTRYPOINT}; {CCH_PLACEHOLDER};",
        version_fingerprint(context)
    )
}

fn parse_identity(value: &Value) -> Option<Identity> {
    let device_id = value["userID"].as_str()?;
    let account_uuid = value["oauthAccount"]["accountUuid"].as_str()?;
    if device_id.len() != 64 || !device_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    uuid::Uuid::parse_str(account_uuid).ok()?;
    Some(Identity {
        device_id: device_id.to_string(),
        account_uuid: account_uuid.to_string(),
    })
}

pub fn discover_identity() -> Option<Identity> {
    if let (Ok(device_id), Ok(account_uuid)) = (
        std::env::var("CLAUDE_CODE_DEVICE_ID"),
        std::env::var("CLAUDE_CODE_ACCOUNT_UUID"),
    ) && let Some(identity) = parse_identity(&json!({
        "userID": device_id,
        "oauthAccount": { "accountUuid": account_uuid }
    })) {
        return Some(identity);
    }
    let path = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(dirs::home_dir)?
        .join(".claude.json");
    let value = serde_json::from_slice::<Value>(&std::fs::read(path).ok()?).ok()?;
    parse_identity(&value)
}

fn canonical_tool_name(name: &str) -> &str {
    match name.to_ascii_lowercase().as_str() {
        "read" => "Read",
        "write" => "Write",
        "edit" => "Edit",
        "bash" => "Bash",
        "grep" => "Grep",
        "find" | "glob" => "Glob",
        _ => name,
    }
}

pub fn local_tool_name(name: &str) -> String {
    match name {
        "Read" | "Write" | "Edit" | "Bash" | "Grep" => name.to_ascii_lowercase(),
        "Glob" => "find".into(),
        _ => name.to_string(),
    }
}

fn canonicalize_tools(body: &mut Value) {
    if let Some(tools) = body["tools"].as_array_mut() {
        for tool in tools {
            if let Some(name) = tool["name"].as_str() {
                tool["name"] = json!(canonical_tool_name(name));
            }
        }
    }
    if let Some(messages) = body["messages"].as_array_mut() {
        for message in messages {
            let Some(content) = message["content"].as_array_mut() else {
                continue;
            };
            for block in content {
                if block["type"] == "tool_use"
                    && let Some(name) = block["name"].as_str()
                {
                    block["name"] = json!(canonical_tool_name(name));
                }
            }
        }
    }
}

pub fn transform_payload(
    mut body: Value,
    context: &Context,
    session_id: Option<&str>,
    identity: Option<&Identity>,
) -> Result<Value> {
    let object = body
        .as_object_mut()
        .context("Anthropic OAuth request is not a JSON object")?;
    let existing = object
        .get("system")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let first = existing.first().and_then(|block| block["text"].as_str());
    let second = existing.get(1).and_then(|block| block["text"].as_str());
    let remaining = if first.is_some_and(|text| text.starts_with("x-anthropic-billing-header: "))
        && second == Some(AGENT_SDK_PROMPT)
    {
        &existing[2..]
    } else if first == Some(LEGACY_PI_PROMPT) {
        &existing[1..]
    } else {
        &existing[..]
    };
    let mut system = vec![
        json!({"type": "text", "text": billing_header(context)}),
        json!({"type": "text", "text": AGENT_SDK_PROMPT}),
    ];
    system.extend_from_slice(remaining);
    object.insert("system".into(), Value::Array(system));
    if let (Some(identity), Some(session_id)) = (identity, session_id) {
        object.insert(
            "metadata".into(),
            json!({
                "user_id": serde_json::to_string(&json!({
                    "device_id": identity.device_id,
                    "account_uuid": identity.account_uuid,
                    "session_id": session_id,
                }))?
            }),
        );
    }
    canonicalize_tools(&mut body);
    Ok(body)
}

pub fn patch_cch(serialized_body: &str) -> Result<String> {
    let mut body: Value = serde_json::from_str(serialized_body)
        .context("Anthropic OAuth request is not a JSON object")?;
    let billing = body["system"]
        .as_array()
        .and_then(|system| system.first())
        .and_then(|block| block["text"].as_str())
        .context("Anthropic OAuth request has no billing system block")?
        .to_string();
    if !billing.starts_with("x-anthropic-billing-header: ") {
        anyhow::bail!("Anthropic OAuth request has an invalid billing block");
    }
    if !billing.contains(CCH_PLACEHOLDER) {
        if billing
            .strip_suffix(';')
            .and_then(|text| text.rsplit_once("; cch="))
            .is_some_and(|(_, value)| {
                value.len() == 5 && value.bytes().all(|b| b.is_ascii_hexdigit())
            })
        {
            return Ok(serialized_body.to_string());
        }
        anyhow::bail!("Anthropic OAuth request has an invalid cch value");
    }
    if !body["model"].is_string() || body.get("max_tokens").is_none() {
        anyhow::bail!("Anthropic OAuth request has no model or max_tokens");
    }
    let mut normalized = body.clone();
    normalized["model"] = json!("");
    normalized
        .as_object_mut()
        .expect("validated object")
        .shift_remove("max_tokens");
    let normalized = serde_json::to_string(&normalized)?;
    let cch = format!(
        "{:05x}",
        xxhash64(normalized.as_bytes(), CCH_SEED) & 0xfffff
    );
    body["system"][0]["text"] = json!(billing.replace(CCH_PLACEHOLDER, &format!("cch={cch}")));
    Ok(serde_json::to_string(&body)?)
}

pub fn serialize_request(
    body: Value,
    context: &Context,
    session_id: Option<&str>,
) -> Result<String> {
    let body = transform_payload(body, context, session_id, discover_identity().as_ref())?;
    patch_cch(&serde_json::to_string(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{UserMessage, now_ms};

    fn context(prompt: &str) -> Context {
        Context {
            system_prompt: Some("Kiss system".into()),
            openai_responses_input: None,
            messages: vec![Message::User(UserMessage {
                content: UserContent::Text(prompt.into()),
                timestamp: now_ms(),
            })],
            tools: vec![],
        }
    }

    #[test]
    fn standard_xxhash64_vectors_match() {
        assert_eq!(xxhash64(b"", 0), 0xef46_db37_51d8_e999);
        assert_eq!(xxhash64(b"hello", 0), 0x26c7_827d_889f_6da3);
    }

    #[test]
    fn recovered_prompt_fingerprint_matches() {
        assert_eq!(
            version_fingerprint(&context("Reply with exactly: PROBE_OK")),
            "f97"
        );
    }

    #[test]
    fn prompt_fingerprint_uses_javascript_utf16_indexes() {
        assert_eq!(
            version_fingerprint(&context("😀Reply with exactly: PROBE_OK")),
            "686"
        );
        assert_eq!(
            version_fingerprint(&context("abcd😀efghijklmnopqrstuvw")),
            "39f"
        );
    }

    #[test]
    fn recovered_body_checksum_matches() {
        let body = r#"{"model":"claude-opus-5","messages":[{"role":"user","content":"A"}],"max_tokens":64000,"stream":true,"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.224.000; cc_entrypoint=sdk-cli; cch=00000;"}]}"#;
        let expected_normalized = r#"{"model":"","messages":[{"role":"user","content":"A"}],"stream":true,"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.224.000; cc_entrypoint=sdk-cli; cch=00000;"}]}"#;
        assert_eq!(
            xxhash64(expected_normalized.as_bytes(), CCH_SEED),
            0x6a37_bc2b_f327_ba34
        );
        let mut parsed: Value = serde_json::from_str(body).unwrap();
        parsed["model"] = json!("");
        parsed.as_object_mut().unwrap().shift_remove("max_tokens");
        assert_eq!(serde_json::to_string(&parsed).unwrap(), expected_normalized);
        let patched = patch_cch(body).unwrap();
        assert!(patched.contains("cch=7ba34"), "{patched}");
    }

    #[test]
    fn oauth_payload_adds_protocol_blocks_identity_and_tool_case() {
        let body = json!({
            "model": "claude-opus-5",
            "messages": [{"role":"user", "content":[]}],
            "max_tokens": 10,
            "stream": true,
            "system": [{"type":"text", "text":"Kiss system"}],
            "tools": [{"name":"read", "description":"Read", "input_schema":{}}]
        });
        let identity = Identity {
            device_id: "f".repeat(64),
            account_uuid: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into(),
        };
        let transformed = transform_payload(
            body,
            &context("Reply with exactly: PROBE_OK"),
            Some("session-one"),
            Some(&identity),
        )
        .unwrap();
        assert!(
            transformed["system"][0]["text"]
                .as_str()
                .unwrap()
                .contains("cch=00000")
        );
        assert_eq!(transformed["system"][1]["text"], AGENT_SDK_PROMPT);
        assert_eq!(transformed["system"][2]["text"], "Kiss system");
        assert_eq!(transformed["tools"][0]["name"], "Read");
        assert!(
            transformed["metadata"]["user_id"]
                .as_str()
                .unwrap()
                .contains("session-one")
        );
    }
}
