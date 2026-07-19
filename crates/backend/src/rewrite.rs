//! The provider-blind heart of the proxy: pure transforms over OpenAI-wire JSON, with no network
//! and no provider knowledge. Everything provider-specific arrives as data (`real_slug`,
//! `extra_body`) so these functions never branch on which backend they serve — which is what keeps
//! every OpenAI-wire `/chat/completions` endpoint on one code path.

use anyhow::{bail, Result};
use serde_json::{Map, Value};

use crate::UsageSnapshot;

/// Rewrite an outbound chat-completions request in place:
///
/// 1. Replace the `model` field with the real provider slug (the alias→identity crossing has
///    already happened via the reveal gate; this just stamps the resolved slug on the wire).
/// 2. Shallow-merge `extra_body` (from the provider config) over the top level, so provider knobs
///    — provider-routing / privacy flags, etc. — are applied without a code branch.
///    `extra_body` is applied first, then `model` is set last, so `extra_body` can never overwrite
///    the resolved model (a misconfigured or hostile `extra_body.model` cannot deblind the route).
///
/// Errors only if the body is not a JSON object (a malformed request we refuse to forward).
pub fn rewrite_request(
    body: &mut Value,
    real_slug: &str,
    extra_body: &Map<String, Value>,
) -> Result<()> {
    let Some(obj) = body.as_object_mut() else {
        bail!("request body is not a JSON object");
    };
    for (k, v) in extra_body {
        obj.insert(k.clone(), v.clone());
    }
    obj.insert("model".to_string(), Value::String(real_slug.to_string()));
    Ok(())
}

/// Pull token usage — and, when the gateway reports it, the real cost — out of an OpenAI-wire
/// object (a non-streaming response, or the final SSE chunk that carries `usage`). Returns `None`
/// when no `usage` block is present. `cost_so_far` is the provider-reported `usage.cost` when
/// present (authoritative, e.g. OpenRouter), else `None` — in which case the router falls back to
/// its own tokens × shelf-price estimate.
pub fn parse_usage(value: &Value) -> Option<UsageSnapshot> {
    let usage = value.get("usage")?;
    let prompt_tokens = usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion_tokens = usage
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cost_so_far = usage.get("cost").and_then(Value::as_f64);
    Some(UsageSnapshot {
        prompt_tokens,
        completion_tokens,
        cost_so_far,
    })
}

/// Response-side fields that name or fingerprint the real model/provider. Stripped on the way back
/// so the blind holds in both directions (the request path is already rewritten). `id` is kept — it
/// is not a model name and some clients need it.
const FINGERPRINT_KEYS: [&str; 3] = ["system_fingerprint", "provider", "x_groq"];

/// Mask a response JSON object in place: replace `model` with the alias and drop provider
/// fingerprint fields. No-op on non-objects.
pub fn mask_response_obj(v: &mut Value, alias: &str) {
    if let Some(o) = v.as_object_mut() {
        if o.contains_key("model") {
            o.insert("model".to_string(), Value::String(alias.to_string()));
        }
        for k in FINGERPRINT_KEYS {
            o.remove(k);
        }
    }
}

/// Replace every occurrence of `needle` with `repl` in a byte stream. Used to scrub the real model
/// slug out of *free text* the structured mask can't reach — e.g. a provider error message like
/// ``Request too large for model `vendor/model-x` …``. Operating on raw bytes keeps a non-UTF-8 body
/// intact; an empty needle is a no-op.
fn replace_bytes(hay: &[u8], needle: &[u8], repl: &[u8]) -> Vec<u8> {
    if needle.is_empty() {
        return hay.to_vec();
    }
    let mut out = Vec::with_capacity(hay.len());
    let mut i = 0;
    while i < hay.len() {
        if hay[i..].starts_with(needle) {
            out.extend_from_slice(repl);
            i += needle.len();
        } else {
            out.push(hay[i]);
            i += 1;
        }
    }
    out
}

/// Mask a whole non-streaming JSON response body: structured masking (rewrite `model`, strip
/// fingerprints) **plus** a raw replace of the real slug with the alias across the entire body, so the
/// slug is scrubbed even when it appears in free text the structured pass can't see (provider error
/// messages, a model self-identifying in its output). Returns the original bytes unchanged only if it
/// is neither JSON nor contains the slug — we never corrupt a body we don't understand. The slug is a
/// specific string, so replacing it is safe; a coincidental appearance in content is itself a leak we
/// *want* masked.
pub fn mask_json_body(body: &[u8], real_slug: &str, alias: &str) -> Vec<u8> {
    let structured = match serde_json::from_slice::<Value>(body) {
        Ok(mut v) => {
            mask_response_obj(&mut v, alias);
            serde_json::to_vec(&v).unwrap_or_else(|_| body.to_vec())
        }
        Err(_) => body.to_vec(),
    };
    replace_bytes(&structured, real_slug.as_bytes(), alias.as_bytes())
}

/// Mask one SSE line (no trailing newline). A `data: {json}` frame gets its `model`/fingerprints
/// masked; `data: [DONE]`, empty payloads, and non-`data:` lines pass through verbatim. In all cases
/// the real slug is scrubbed from the line text (free-text leak defence, as in [`mask_json_body`]).
pub fn mask_sse_line(line: &str, real_slug: &str, alias: &str) -> String {
    let masked = if let Some(rest) = line.trim_start().strip_prefix("data:") {
        let payload = rest.trim();
        if payload.is_empty() || payload == "[DONE]" {
            line.to_string()
        } else if let Ok(mut v) = serde_json::from_str::<Value>(payload) {
            mask_response_obj(&mut v, alias);
            serde_json::to_string(&v)
                .map(|s| format!("data: {s}"))
                .unwrap_or_else(|_| line.to_string())
        } else {
            line.to_string()
        }
    } else {
        line.to_string()
    };
    if real_slug.is_empty() {
        masked
    } else {
        masked.replace(real_slug, alias)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn rewrites_model_and_leaves_the_rest_untouched() {
        let mut body = json!({
            "model": "x7k2:q4m9",             // the alias the CLI sent
            "messages": [{"role": "user", "content": "hi"}],
            "temperature": 0.2,
        });
        rewrite_request(&mut body, "moonshotai/kimi-k2-instruct", &Map::new()).unwrap();
        assert_eq!(body["model"], "moonshotai/kimi-k2-instruct");
        assert_eq!(body["temperature"], 0.2);
        assert_eq!(body["messages"][0]["content"], "hi");
    }

    #[test]
    fn merges_extra_body_but_never_clobbers_the_resolved_model() {
        let mut body = json!({ "model": "alias", "messages": [] });
        // A provider extra_body that (adversarially) also sets `model` must not win — else it could
        // route around the blind. `require_parameters` is a normal provider-routing flag.
        let extra = map(json!({
            "provider": { "require_parameters": true },
            "model": "attacker/override",
        }));
        rewrite_request(&mut body, "real/slug", &extra).unwrap();
        assert_eq!(body["model"], "real/slug", "resolved model must always win");
        assert_eq!(body["provider"]["require_parameters"], true);
    }

    #[test]
    fn non_object_body_is_refused() {
        let mut body = json!("not an object");
        assert!(rewrite_request(&mut body, "real/slug", &Map::new()).is_err());
    }

    #[test]
    fn parse_usage_reads_tokens_and_cost() {
        let resp = json!({
            "choices": [],
            "usage": { "prompt_tokens": 1200, "completion_tokens": 340, "cost": 0.00025938 }
        });
        let u = parse_usage(&resp).unwrap();
        assert_eq!(u.prompt_tokens, 1200);
        assert_eq!(u.completion_tokens, 340);
        assert_eq!(u.cost_so_far, Some(0.00025938)); // provider-reported cost captured

        // No `cost` field → None (router falls back to its estimate).
        let no_cost =
            parse_usage(&json!({ "usage": { "prompt_tokens": 1, "completion_tokens": 2 } }))
                .unwrap();
        assert_eq!(no_cost.cost_so_far, None);
        assert!(parse_usage(&json!({ "choices": [] })).is_none());
    }

    #[test]
    fn mask_json_body_replaces_model_and_strips_fingerprints() {
        let body = serde_json::to_vec(&json!({
            "id": "gen-123", "model": "openai/gpt-oss-120b", "provider": "AkashML",
            "system_fingerprint": "fp_x", "choices": [{"message": {"content": "hi"}}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3}
        }))
        .unwrap();
        let out: Value =
            serde_json::from_slice(&mask_json_body(&body, "openai/gpt-oss-120b", "x7k2:q4m9"))
                .unwrap();
        assert_eq!(out["model"], "x7k2:q4m9"); // real slug masked to alias
        assert!(out.get("provider").is_none()); // fingerprint stripped
        assert!(out.get("system_fingerprint").is_none());
        assert_eq!(out["id"], "gen-123"); // id preserved
        assert_eq!(out["choices"][0]["message"]["content"], "hi"); // content untouched
    }

    #[test]
    fn mask_json_body_scrubs_the_slug_from_an_error_message() {
        // The exact Groq deblind: no top-level `model` field, the real slug is inside error.message.
        let body = serde_json::to_vec(&json!({
            "error": {"message": "Request too large for model `openai/gpt-oss-120b` in organization `org_abc`",
                      "code": "rate_limit_exceeded"}
        })).unwrap();
        let out = mask_json_body(&body, "openai/gpt-oss-120b", "x7k2:q4m9");
        let text = String::from_utf8(out).unwrap();
        assert!(
            !text.contains("openai/gpt-oss-120b"),
            "real slug must not survive in the error text"
        );
        assert!(text.contains("x7k2:q4m9"), "the alias replaces it");
        assert!(
            text.contains("rate_limit_exceeded"),
            "the rest of the error is intact"
        );
    }

    #[test]
    fn mask_json_body_leaves_non_json_without_the_slug_untouched() {
        assert_eq!(mask_json_body(b"not json", "real/slug", "a"), b"not json");
        // but a real slug in a non-JSON body is still scrubbed
        assert_eq!(
            mask_json_body(b"failed on real/slug", "real/slug", "al"),
            b"failed on al"
        );
    }

    #[test]
    fn mask_sse_line_masks_data_frames_only() {
        let masked = mask_sse_line(
            r#"data: {"model":"qwen/qwen3.6-35b-a3b","provider":"AkashML"}"#,
            "qwen/qwen3.6-35b-a3b",
            "al:al",
        );
        let v: Value = serde_json::from_str(masked.strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(v["model"], "al:al");
        assert!(v.get("provider").is_none());
        // control frames + non-data lines pass through verbatim
        assert_eq!(
            mask_sse_line("data: [DONE]", "real/slug", "al:al"),
            "data: [DONE]"
        );
        assert_eq!(
            mask_sse_line(": keep-alive", "real/slug", "al:al"),
            ": keep-alive"
        );
        // the slug is scrubbed even from a non-data SSE error line
        assert_eq!(
            mask_sse_line("event: error real/slug", "real/slug", "al"),
            "event: error al"
        );
    }
}
