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

/// Pull cumulative token usage out of an OpenAI-wire object (a non-streaming response, or the
/// final SSE chunk that carries `usage`). Returns `None` when no `usage` block is present — the
/// caller keeps its last known snapshot. `cost_so_far` is left `None`: pricing is the router's job
/// (tokens × unit price), not the transport's.
pub fn parse_usage(value: &Value) -> Option<UsageSnapshot> {
    let usage = value.get("usage")?;
    let prompt_tokens = usage.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
    let completion_tokens = usage.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0);
    Some(UsageSnapshot { prompt_tokens, completion_tokens, cost_so_far: None })
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
    fn parse_usage_reads_tokens_or_none() {
        let resp = json!({
            "choices": [],
            "usage": { "prompt_tokens": 1200, "completion_tokens": 340 }
        });
        let u = parse_usage(&resp).unwrap();
        assert_eq!(u.prompt_tokens, 1200);
        assert_eq!(u.completion_tokens, 340);
        assert_eq!(u.cost_so_far, None);

        assert!(parse_usage(&json!({ "choices": [] })).is_none());
    }
}
