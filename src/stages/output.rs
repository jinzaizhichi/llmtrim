//! Stage F — output control (request-shaping).
//!
//! These transforms change request fields/instructions whose payoff is on the
//! *response* side (fewer/cheaper output tokens), so they use the OutputShaping
//! gate (always applied; never reverted on input tokens). Their output savings are
//! validated out-of-band — recorded fixtures or the proxy phase — since input
//! and output compression are evaluated separately.
//!
//! The `terse` instruction — concise, full sentences; clean, low garble risk; ~73%
//! output cut in a live test. (`draft` below is a separate reasoning-scaffold tier.)

use anyhow::Result;
use serde_json::Value;

use crate::gate::{GateKind, PlanEntry, Transform};
use crate::ir::Request;
use crate::provider::Provider;

/// `terse` tier: a small, fixed input cost for a real output-token reduction
/// (output tokens cost ~3–5× input).
// Instructions stay verbose on purpose: the bench showed a shorter instruction cuts a
// few input tokens but is LESS forceful → the model rambles → far more output tokens.
// Output costs ~3–5× input, so the instruction's small input cost buys a much larger
// output saving. Don't trade it away to flatter the input %.
pub const TERSE_INSTRUCTION: &str = include_str!("../../prompts/output_terse.txt");

/// `draft` tier: Chain-of-Draft — collapse the reasoning scaffold, not the prose
/// (arXiv:2502.18600). Targets reasoning-model output tokens, which concentrate in
/// the chain-of-thought.
pub const DRAFT_INSTRUCTION: &str = include_str!("../../prompts/output_draft.txt");

/// Compact-code output instruction: emit minified code (arXiv:2508.13666 reports
/// up to −36% output tokens with no correctness loss on capable models). Model-
/// gated — weak models can emit syntactically broken compact code.
pub const COMPACT_CODE_INSTRUCTION: &str = include_str!("../../prompts/output_compact_code.txt");

/// Soft prompt-side token budget (TALE zero-shot, arXiv:2412.18547). `{budget}` is
/// replaced with the cap; complements the hard `max_tokens` cap.
pub const TOKEN_BUDGET_TMPL: &str = include_str!("../../prompts/output_token_budget.txt");

/// Output-control intensity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputLevel {
    Terse,
    Draft,
}

impl OutputLevel {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "draft" | "cod" => OutputLevel::Draft,
            _ => OutputLevel::Terse,
        }
    }

    fn instruction(self) -> &'static str {
        match self {
            OutputLevel::Terse => TERSE_INSTRUCTION,
            OutputLevel::Draft => DRAFT_INSTRUCTION,
        }
    }
}

pub struct OutputControlStage {
    pub level: OutputLevel,
    /// If set and the request has no cap, impose this output-token cap.
    pub max_tokens: Option<u64>,
    /// If set, inject a *soft* token budget into the prompt ("answer within N
    /// tokens") — the prompt-side complement of the hard `max_tokens` cap
    /// (TALE zero-shot, arXiv:2412.18547).
    pub token_budget: Option<u64>,
    /// Instruct the model to emit minified code (arXiv:2508.13666). Model-gated.
    pub compact_code: bool,
}

impl Transform for OutputControlStage {
    fn name(&self) -> &str {
        "output-control"
    }

    fn gate_kind(&self) -> GateKind {
        GateKind::OutputShaping
    }

    fn apply(
        &self,
        req: &mut Request,
        provider: &dyn Provider,
        _plan: &mut Vec<PlanEntry>,
    ) -> Result<()> {
        // Tool-call-shaped request: the expected answer is a function-call payload, not
        // prose. Prose-shaping instructions can't shrink call arguments — the live A/B's
        // agent corpus saves 0.0% output tokens with them — so on the most-resent request
        // shape (agent loops) they are pure input cost. Skip them; the hard `max_tokens`
        // cap below stays (it costs nothing). `tool_choice: "none"` means the model is
        // told NOT to call, so the answer is prose again and shaping applies.
        if !tool_call_shaped(req) {
            provider.add_system_instruction(req, self.level.instruction());
            // Soft numeric token budgets ("answer within N tokens") FAIL on reasoning
            // models: the batch-prompting overthinking study (arXiv:2511.04108, 2025)
            // found explicit thinking-budget instructions are ignored on DeepSeek-R1 /
            // OpenAI o1, and when followed they cut accuracy. They stay valid only for
            // NON-reasoning models (TALE, arXiv:2412.18547). The terse/draft style
            // instruction above and compact_code below are prose/scaffold shaping, not a
            // numeric cap — Chain-of-Draft (arXiv:2502.18600) validated terse/draft on
            // gpt-oss, a reasoning model — so they stay for all models. Skip ONLY the
            // soft budget here; the hard `max_tokens` cap below is server-enforced, not an
            // instruction the model can ignore, so it stays too.
            if let Some(budget) = self.token_budget
                && !reasoning_model_request(req)
            {
                provider.add_system_instruction(
                    req,
                    &TOKEN_BUDGET_TMPL.replace("{budget}", &budget.to_string()),
                );
            }
            if self.compact_code {
                provider.add_system_instruction(req, COMPACT_CODE_INSTRUCTION);
            }
        }
        if let Some(cap) = self.max_tokens
            && provider.max_tokens(req).is_none()
        {
            provider.set_max_tokens(req, cap);
        }
        Ok(())
    }
}

/// True when the request carries a non-empty `tools` array and tool calling isn't
/// disabled (`tool_choice: "none"`) — i.e. the answer is expected to be a tool call.
/// Shared shape across OpenAI and Anthropic bodies (both use `tools`/`tool_choice`).
fn tool_call_shaped(req: &Request) -> bool {
    let raw = req.raw();
    raw.get("tools")
        .and_then(Value::as_array)
        .is_some_and(|t| !t.is_empty())
        && raw.get("tool_choice").and_then(Value::as_str) != Some("none")
}

/// True when the request targets a reasoning model — one that emits an internal
/// thinking/chain-of-thought pass. Soft numeric token budgets are counter-productive
/// on these (arXiv:2511.04108); the caller skips that one lever when this returns true.
///
/// Detection is deterministic, two-pronged, and tuned for PRECISION over recall: a
/// missed reasoning model just keeps today's behavior (a soft budget that may be
/// ignored), whereas a false positive silently disables a working lever on a
/// non-reasoning model. Each signal:
///
/// 1. Explicit reasoning request fields (provider-set, unambiguous). Presence of ANY
///    means the caller has opted the request into a reasoning pass:
///    - `reasoning`         — OpenRouter / OpenAI Responses reasoning config object.
///    - `reasoning_effort`  — OpenAI o-series / gpt-5 effort knob.
///    - `thinking`          — Anthropic extended-thinking block.
///
/// 2. Model-id families that are reasoning-only or reasoning-by-default, matched on the
///    lowercased `model` string:
///    - OpenAI o-series (`o1`/`o3`/`o4`): matched per `/`-segment as an exact token or a
///      `oN-…` prefix, NEVER as a bare substring — so `openai/o3-mini`, `o1`, `o1-mini`
///      hit while `gpt-4o`, `phi-4`, `solar-pro` (which merely contain `o4`/`o1`/`o3`
///      as substrings) do NOT.
///    - `gpt-5` family — reasoning-default.
///    - `-r1` / `deepseek-r1` — DeepSeek-R1 line.
///    - `-reasoner` — DeepSeek `deepseek-reasoner` alias and similar.
///    - `-thinking` — explicit thinking variants (e.g. `…-thinking`).
///    - `gpt-oss` — reasoning model (the repo's own bench validated draft on it).
///    - `qwq` — Qwen QwQ reasoning line.
///
///    `grok-` is deliberately OMITTED: only Grok 3+ reason by default and the family
///    prefix is too broad, so including it would risk false positives (precision rule).
fn reasoning_model_request(req: &Request) -> bool {
    let raw = req.raw();
    if raw.get("reasoning").is_some()
        || raw.get("reasoning_effort").is_some()
        || raw.get("thinking").is_some()
    {
        return true;
    }
    let Some(model) = raw.get("model").and_then(Value::as_str) else {
        return false;
    };
    let model = model.to_ascii_lowercase();
    is_o_series(&model)
        || model.contains("gpt-5")
        || model.contains("-r1")
        || model.contains("-reasoner")
        || model.contains("-thinking")
        || model.contains("gpt-oss")
        || model.contains("qwq")
}

/// Match an OpenAI o-series id (`o1`/`o3`/`o4`) as a bounded token, not a substring.
/// A model id may be slash-namespaced (`openai/o3-mini`); we test each `/`-segment for
/// an exact `oN` or an `oN-…` prefix. This rejects ids that merely contain the digits,
/// e.g. `gpt-4o` (segment `gpt-4o`), `phi-4`, `solar-pro`.
fn is_o_series(model: &str) -> bool {
    model.split('/').any(|seg| {
        ["o1", "o3", "o4"]
            .iter()
            .any(|p| seg == *p || seg.starts_with(&format!("{p}-")))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::ProviderKind;
    use crate::pipeline;
    use crate::provider::OpenAiProvider;
    use crate::tokenizer::counter_for;
    use serde_json::json;

    fn run_one(body: Value, stage: OutputControlStage) -> Request {
        let mut req = Request::from_value(ProviderKind::OpenAi, body);
        let counter = counter_for(ProviderKind::OpenAi, Some("gpt-4o")).unwrap();
        let stages: Vec<Box<dyn Transform>> = vec![Box::new(stage)];
        let _ = pipeline::run(&mut req, &OpenAiProvider, counter.as_ref(), &stages);
        req
    }

    #[test]
    fn level_parses() {
        assert_eq!(OutputLevel::parse("draft"), OutputLevel::Draft);
        assert_eq!(OutputLevel::parse("terse"), OutputLevel::Terse);
        assert_eq!(OutputLevel::parse("ultra"), OutputLevel::Terse);
        assert_eq!(OutputLevel::parse("whatever"), OutputLevel::Terse);
    }

    #[test]
    fn draft_injects_chain_of_draft() {
        let req = run_one(
            json!({"messages":[{"role":"user","content":"hi"}]}),
            OutputControlStage {
                level: OutputLevel::Draft,
                max_tokens: None,
                token_budget: None,
                compact_code: false,
            },
        );
        let sys = req.get_str("/messages/0/content").unwrap();
        assert!(sys.contains("draft") && sys.contains("step"));
    }

    #[test]
    fn token_budget_injects_soft_limit() {
        let req = run_one(
            json!({"messages":[{"role":"user","content":"hi"}]}),
            OutputControlStage {
                level: OutputLevel::Terse,
                max_tokens: None,
                token_budget: Some(120),
                compact_code: false,
            },
        );
        let joined: String = req
            .raw()
            .pointer("/messages")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(|m| m.get("content").and_then(Value::as_str))
            .collect();
        assert!(joined.contains("120 tokens"), "soft budget injected");
    }

    #[test]
    fn tool_call_request_skips_prose_shaping_but_keeps_cap() {
        // tools present + tool_choice auto ⇒ the answer is a function call: no terse/budget
        // instruction (pure input cost, 0% output saving on the agent corpus), but the free
        // hard cap still applies.
        let req = run_one(
            json!({"messages":[{"role":"user","content":"book a flight"}],
                   "tools":[{"type":"function","function":{"name":"book","parameters":{}}}],
                   "tool_choice":"auto"}),
            OutputControlStage {
                level: OutputLevel::Terse,
                max_tokens: Some(900),
                token_budget: Some(120),
                compact_code: true,
            },
        );
        let joined: String = req
            .raw()
            .pointer("/messages")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(|m| m.get("content").and_then(Value::as_str))
            .collect();
        assert!(
            !joined.contains("concise") && !joined.contains("120 tokens"),
            "no prose-shaping instructions on a tool-call request: {joined}"
        );
        assert_eq!(
            req.raw()
                .get("max_completion_tokens")
                .and_then(Value::as_u64),
            Some(900),
            "hard cap still set (free)"
        );
    }

    #[test]
    fn tool_choice_none_restores_prose_shaping() {
        // tools present but calling disabled ⇒ the answer is prose; shaping applies again.
        let req = run_one(
            json!({"messages":[{"role":"user","content":"book a flight"}],
                   "tools":[{"type":"function","function":{"name":"book","parameters":{}}}],
                   "tool_choice":"none"}),
            OutputControlStage {
                level: OutputLevel::Terse,
                max_tokens: None,
                token_budget: None,
                compact_code: false,
            },
        );
        let sys = req.get_str("/messages/0/content").unwrap();
        assert!(sys.contains("concise"), "prose shaping applies: {sys}");
    }

    #[test]
    fn terse_injects_concise() {
        let req = run_one(
            json!({"messages":[{"role":"user","content":"hi"}]}),
            OutputControlStage {
                level: OutputLevel::Terse,
                max_tokens: None,
                token_budget: None,
                compact_code: false,
            },
        );
        let sys = req.get_str("/messages/0/content").unwrap();
        assert!(sys.contains("concise"));
        assert_eq!(
            req.raw()
                .pointer("/messages/0/role")
                .and_then(Value::as_str),
            Some("system")
        );
    }

    #[test]
    fn sets_max_tokens_only_when_absent() {
        let req = run_one(
            json!({"messages":[{"role":"user","content":"hi"}]}),
            OutputControlStage {
                level: OutputLevel::Terse,
                max_tokens: Some(256),
                token_budget: None,
                compact_code: false,
            },
        );
        assert_eq!(OpenAiProvider.max_tokens(&req), Some(256));

        let req2 = run_one(
            json!({"max_tokens":99,"messages":[{"role":"user","content":"hi"}]}),
            OutputControlStage {
                level: OutputLevel::Terse,
                max_tokens: Some(256),
                token_budget: None,
                compact_code: false,
            },
        );
        assert_eq!(
            OpenAiProvider.max_tokens(&req2),
            Some(99),
            "must not overwrite a caller-set cap"
        );
    }

    fn joined_content(req: &Request) -> String {
        req.raw()
            .pointer("/messages")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(|m| m.get("content").and_then(Value::as_str))
            .collect()
    }

    #[test]
    fn reasoning_model_skips_soft_budget_keeps_terse_and_cap() {
        // arXiv:2511.04108: soft numeric budgets are ignored / hurt on reasoning models.
        // Skip ONLY the soft budget; terse stays (Chain-of-Draft validates it on reasoning
        // models) and the server-enforced hard cap stays.
        let req = run_one(
            json!({"model":"deepseek/deepseek-r1",
                   "messages":[{"role":"user","content":"hi"}]}),
            OutputControlStage {
                level: OutputLevel::Terse,
                max_tokens: Some(256),
                token_budget: Some(120),
                compact_code: false,
            },
        );
        let joined = joined_content(&req);
        assert!(
            !joined.contains("120 tokens"),
            "soft budget must be skipped on a reasoning model: {joined}"
        );
        assert!(
            joined.contains("concise"),
            "terse instruction still injected: {joined}"
        );
        assert_eq!(OpenAiProvider.max_tokens(&req), Some(256), "hard cap stays");
    }

    #[test]
    fn reasoning_field_skips_soft_budget() {
        // The `reasoning` request field alone marks a reasoning request, regardless of model.
        let req = run_one(
            json!({"model":"some-model","reasoning":{"effort":"low"},
                   "messages":[{"role":"user","content":"hi"}]}),
            OutputControlStage {
                level: OutputLevel::Terse,
                max_tokens: None,
                token_budget: Some(120),
                compact_code: false,
            },
        );
        assert!(!joined_content(&req).contains("120 tokens"));
    }

    #[test]
    fn non_reasoning_model_still_gets_soft_budget() {
        // Regression guard: a plain chat model must keep the TALE soft budget.
        let req = run_one(
            json!({"model":"gpt-4o-mini",
                   "messages":[{"role":"user","content":"hi"}]}),
            OutputControlStage {
                level: OutputLevel::Terse,
                max_tokens: None,
                token_budget: Some(120),
                compact_code: false,
            },
        );
        assert!(
            joined_content(&req).contains("120 tokens"),
            "soft budget must still be injected on a non-reasoning model"
        );
    }

    fn req_with(body: Value) -> Request {
        Request::from_value(ProviderKind::OpenAi, body)
    }

    #[test]
    fn detects_reasoning_request_fields() {
        assert!(reasoning_model_request(&req_with(
            json!({"model":"x","reasoning":{"effort":"low"}})
        )));
        assert!(reasoning_model_request(&req_with(
            json!({"model":"x","reasoning_effort":"high"})
        )));
        // Anthropic extended-thinking block.
        assert!(reasoning_model_request(&req_with(
            json!({"model":"claude-3-7-sonnet","thinking":{"type":"enabled","budget_tokens":1024}})
        )));
    }

    #[test]
    fn detects_reasoning_model_ids() {
        for id in [
            "deepseek/deepseek-r1",
            "deepseek-reasoner",
            "o1",
            "o1-mini",
            "openai/o3-mini",
            "o4-mini",
            "openai/gpt-5",
            "gpt-5-mini",
            "openai/gpt-oss-20b",
            "qwen/qwq-32b",
            "qwen3-235b-thinking",
        ] {
            assert!(
                reasoning_model_request(&req_with(json!({"model": id}))),
                "{id} should be detected as reasoning"
            );
        }
    }

    #[test]
    fn does_not_false_positive_on_non_reasoning_ids() {
        // `o4`/`o1`/`o3` substrings must NOT trip the o-series matcher, and no field/family
        // signal should fire on these plain chat models.
        for id in [
            "gpt-4o",
            "gpt-4o-mini",
            "phi-4",
            "solar-pro",
            "claude-3-5-sonnet-20241022",
            "mistral-large",
            "gemini-2.0-flash",
            "grok-2",
        ] {
            assert!(
                !reasoning_model_request(&req_with(json!({"model": id}))),
                "{id} must NOT be detected as reasoning"
            );
        }
    }

    #[test]
    fn missing_model_is_not_reasoning() {
        assert!(!reasoning_model_request(&req_with(
            json!({"messages":[{"role":"user","content":"hi"}]})
        )));
    }

    #[test]
    fn compact_code_injects_instruction() {
        let req = run_one(
            json!({"messages":[{"role":"user","content":"hi"}]}),
            OutputControlStage {
                level: OutputLevel::Terse,
                max_tokens: None,
                token_budget: None,
                compact_code: true,
            },
        );
        let joined: String = req
            .raw()
            .pointer("/messages")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(|m| m.get("content").and_then(Value::as_str))
            .collect();
        assert!(
            joined.contains("minified"),
            "compact-code instruction injected"
        );
    }
}
