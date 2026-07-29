//! Typed `session/request_permission` handling (WF-08 / Phase 1 P2).
//!
//! Replaces raw `serde_json::Value` poking in the ACP read loop with typed
//! parsing, and correlates a permission request with the `tool_call`
//! session/update that announced the gated tool call (matched by
//! `toolCallId`). Parsing is deliberately behavior-preserving with the
//! previous inline code: option quirks (a `reject_once` without an
//! `optionId` defaults to `"reject"`) and error strings are kept intact.
//!
//! The runtime-mode policy (P3) will consume [`PermissionRequest`] to decide
//! between auto-selection and parking the request for a human.

use serde_json::Value;

use crate::config::RuntimeMode;

/// What the runtime-mode policy does with a permission request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateAction {
    /// Answer immediately with the allow-then-reject auto ladder.
    AutoSelect,
    /// Park the request for a human decision (pending row on the relay).
    Park,
}

/// Decide whether a permission request is auto-selected or parked, from the
/// harness runtime mode and the gated tool call's ACP kind.
///
/// `auto-accept-edits` auto-approves the file-shaped kinds (`read`, `edit`)
/// and gates everything else — including requests whose tool call could not
/// be correlated (`None`), which fail toward the gate, never around it.
pub fn gate_action(mode: RuntimeMode, tool_kind: Option<&str>) -> GateAction {
    match mode {
        RuntimeMode::FullAccess => GateAction::AutoSelect,
        RuntimeMode::Supervised => GateAction::Park,
        RuntimeMode::AutoAcceptEdits => match tool_kind {
            Some("read") | Some("edit") => GateAction::AutoSelect,
            _ => GateAction::Park,
        },
    }
}

/// Map an ACP tool-call kind onto the human-facing request taxonomy stored
/// with a parked request (`command` / `file-read` / `file-change` / `other`).
pub fn request_kind_for_tool(tool_kind: Option<&str>) -> &'static str {
    match tool_kind {
        Some("execute") => "command",
        Some("read") => "file-read",
        Some("edit") | Some("delete") | Some("move") => "file-change",
        _ => "other",
    }
}

/// Render the human-facing `detail` line for a parked request, bounded to
/// the relay's 400-char limit.
pub fn detail_for_tool(tool_call: Option<&ToolCallRef>, fallback: &str) -> String {
    let raw = tool_call
        .and_then(|tc| tc.title.as_deref())
        .unwrap_or(fallback);
    let mut detail: String = raw.chars().take(400).collect();
    if detail.trim().is_empty() {
        detail = fallback.to_owned();
    }
    detail
}

/// How many announced tool calls to retain for correlation.
///
/// A permission request follows its announcing `tool_call` update almost
/// immediately, so a small window is enough; the bound keeps a chatty agent
/// from growing client state without limit.
pub const TOOL_CALL_MEMORY: usize = 16;

/// One entry of the agent's `options` array on a permission request.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionOption {
    /// The id to echo back in the `selected` outcome. Optional because
    /// agents have been observed to omit it; selection handles the absence
    /// per option kind (see [`PermissionRequest::auto_option`]).
    pub option_id: Option<String>,
    /// ACP option kind: `allow_once`, `allow_always`, `reject_once`,
    /// `reject_always`. Kept as a raw string — unknown kinds are data, not
    /// errors.
    pub kind: String,
    /// Human-readable option label, when provided.
    pub name: Option<String>,
}

/// Reference to the tool call a permission request gates.
///
/// Appears in two places with the same field vocabulary: the request's own
/// `params.toolCall`, and the `tool_call` session/update that announced the
/// call. Either side may be sparse (claude-code-acp sends only the id on the
/// request), so [`filled_from`](Self::filled_from) merges the two.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolCallRef {
    /// `toolCallId` — the correlation key.
    pub id: Option<String>,
    /// Human-readable title (e.g. the shell command line).
    pub title: Option<String>,
    /// ACP tool kind (`execute`, `edit`, `read`, `fetch`, …).
    pub kind: Option<String>,
    /// The tool's raw input payload, when provided (`rawInput`).
    pub raw_input: Option<Value>,
}

impl ToolCallRef {
    /// Parse from a `params.toolCall` object on a permission request.
    /// Returns `None` for a missing or non-object value.
    pub fn from_params(tool_call: &Value) -> Option<Self> {
        tool_call.as_object()?;
        Some(Self {
            id: str_field(tool_call, "toolCallId"),
            title: str_field(tool_call, "title"),
            kind: str_field(tool_call, "kind"),
            raw_input: tool_call.get("rawInput").cloned(),
        })
    }

    /// Parse from a `tool_call` session/update. Requires `toolCallId` —
    /// an announcement without the correlation key is useless here.
    pub fn from_update(update: &Value) -> Option<Self> {
        let id = str_field(update, "toolCallId")?;
        Some(Self {
            id: Some(id),
            title: str_field(update, "title"),
            kind: str_field(update, "kind"),
            raw_input: update.get("rawInput").cloned(),
        })
    }

    /// This reference with any missing field filled from `announced`.
    /// Fields present on `self` (the request side) win.
    pub fn filled_from(&self, announced: &ToolCallRef) -> ToolCallRef {
        ToolCallRef {
            id: self.id.clone().or_else(|| announced.id.clone()),
            title: self.title.clone().or_else(|| announced.title.clone()),
            kind: self.kind.clone().or_else(|| announced.kind.clone()),
            raw_input: self
                .raw_input
                .clone()
                .or_else(|| announced.raw_input.clone()),
        }
    }
}

/// The auto-selection the current policy makes for a permission request.
#[derive(Debug, PartialEq)]
pub enum AutoOption<'a> {
    /// An `allow_once` option was offered — approve with this `optionId`.
    AllowOnce(&'a str),
    /// No `allow_once`; fall back to rejecting with this `optionId`.
    RejectOnce(&'a str),
}

/// A typed `session/request_permission` request.
#[derive(Debug, Clone)]
pub struct PermissionRequest {
    /// JSON-RPC request id — numeric or string per JSON-RPC 2.0.
    pub id: Value,
    /// The decision options the agent offered.
    pub options: Vec<PermissionOption>,
    /// The tool call this request gates, when the agent provided
    /// `params.toolCall`. Often sparse — correlate via
    /// [`ToolCallRef::filled_from`].
    pub tool_call: Option<ToolCallRef>,
}

impl PermissionRequest {
    /// Parse the `params` of a `session/request_permission` whose JSON-RPC
    /// `id` was already extracted (the id must be stored as pending *before*
    /// any further parsing can fail, so teardown can still respond
    /// `cancelled` to it).
    pub fn parse_params(id: Value, params: &Value) -> Result<Self, String> {
        let options = params
            .get("options")
            .and_then(Value::as_array)
            .ok_or_else(|| "permission request missing options".to_owned())?
            .iter()
            .map(|opt| PermissionOption {
                option_id: str_field(opt, "optionId"),
                kind: str_field(opt, "kind").unwrap_or_default(),
                name: str_field(opt, "name"),
            })
            .collect();

        let tool_call = params.get("toolCall").and_then(ToolCallRef::from_params);

        Ok(Self {
            id,
            options,
            tool_call,
        })
    }

    /// The option the full-access policy selects: `allow_once`, falling back
    /// to `reject_once`.
    ///
    /// Quirks preserved from the previous inline implementation: an
    /// `allow_once` without an `optionId` is a protocol error, while a
    /// `reject_once` without one falls back to the literal `"reject"`.
    pub fn auto_option(&self) -> Result<AutoOption<'_>, String> {
        if let Some(allow) = self.find_kind("allow_once") {
            let option_id = allow
                .option_id
                .as_deref()
                .ok_or_else(|| "allow_once option missing optionId".to_owned())?;
            return Ok(AutoOption::AllowOnce(option_id));
        }
        if let Some(reject) = self.find_kind("reject_once") {
            return Ok(AutoOption::RejectOnce(
                reject.option_id.as_deref().unwrap_or("reject"),
            ));
        }
        Err("no suitable permission option found (neither allow_once nor reject_once)".to_owned())
    }

    /// Map a human decision (as recorded by the relay: `allow_once`,
    /// `allow_always`, `reject_once`, `cancel`) onto the response for this
    /// request's option set. `allow_always` degrades to `allow_once` when
    /// the agent offered no session-scoped option; an unmappable decision
    /// returns `None` and the caller answers `cancelled` (fail-safe: never
    /// approve on a mapping miss).
    pub fn response_for_decision(&self, decision: &str) -> Option<DecisionResponse<'_>> {
        match decision {
            "allow_once" => self
                .find_kind("allow_once")
                .and_then(|opt| opt.option_id.as_deref())
                .map(DecisionResponse::Selected),
            "allow_always" => self
                .find_kind("allow_always")
                .or_else(|| self.find_kind("allow_once"))
                .and_then(|opt| opt.option_id.as_deref())
                .map(DecisionResponse::Selected),
            "reject_once" => self.find_kind("reject_once").map(|opt| {
                DecisionResponse::Selected(opt.option_id.as_deref().unwrap_or("reject"))
            }),
            "cancel" => Some(DecisionResponse::Cancelled),
            _ => None,
        }
    }

    fn find_kind(&self, kind: &str) -> Option<&PermissionOption> {
        self.options.iter().find(|opt| opt.kind == kind)
    }
}

/// The wire response mapped from a human decision.
#[derive(Debug, PartialEq)]
pub enum DecisionResponse<'a> {
    /// Respond `outcome: selected` with this `optionId`.
    Selected(&'a str),
    /// Respond `outcome: cancelled`.
    Cancelled,
}

fn str_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn params_with_options(options: Value) -> Value {
        json!({ "sessionId": "sess-1", "options": options })
    }

    #[test]
    fn parses_numeric_and_string_ids() {
        let params = params_with_options(json!([
            { "optionId": "a", "kind": "allow_once", "name": "Allow" },
        ]));
        let numeric = PermissionRequest::parse_params(json!(7), &params).expect("numeric id");
        assert_eq!(numeric.id, json!(7));
        let string = PermissionRequest::parse_params(json!("perm-1"), &params).expect("string id");
        assert_eq!(string.id, json!("perm-1"));
    }

    #[test]
    fn missing_options_preserves_error_string() {
        let err = PermissionRequest::parse_params(json!(1), &json!({ "sessionId": "s" }))
            .expect_err("must fail");
        assert_eq!(err, "permission request missing options");
    }

    #[test]
    fn parses_options_and_tool_call() {
        let params = json!({
            "sessionId": "sess-1",
            "options": [
                { "optionId": "allow", "kind": "allow_once", "name": "Allow" },
                { "optionId": "always", "kind": "allow_always" },
                { "optionId": "no", "kind": "reject_once" },
            ],
            "toolCall": {
                "toolCallId": "call-9",
                "title": "rm -rf /tmp/scratch",
                "kind": "execute",
                "rawInput": { "command": "rm -rf /tmp/scratch" },
            },
        });
        let req = PermissionRequest::parse_params(json!(3), &params).expect("parse");
        assert_eq!(req.options.len(), 3);
        assert_eq!(req.options[0].kind, "allow_once");
        assert_eq!(req.options[1].option_id.as_deref(), Some("always"));
        let tc = req.tool_call.expect("toolCall parsed");
        assert_eq!(tc.id.as_deref(), Some("call-9"));
        assert_eq!(tc.kind.as_deref(), Some("execute"));
        assert_eq!(
            tc.raw_input,
            Some(json!({ "command": "rm -rf /tmp/scratch" }))
        );
    }

    #[test]
    fn auto_option_prefers_allow_once() {
        let params = params_with_options(json!([
            { "optionId": "no", "kind": "reject_once" },
            { "optionId": "yes", "kind": "allow_once" },
        ]));
        let req = PermissionRequest::parse_params(json!(1), &params).expect("parse");
        assert_eq!(req.auto_option(), Ok(AutoOption::AllowOnce("yes")));
    }

    #[test]
    fn auto_option_falls_back_to_reject_once() {
        let params = params_with_options(json!([
            { "optionId": "no", "kind": "reject_once" },
        ]));
        let req = PermissionRequest::parse_params(json!(1), &params).expect("parse");
        assert_eq!(req.auto_option(), Ok(AutoOption::RejectOnce("no")));
    }

    #[test]
    fn allow_once_without_option_id_is_an_error() {
        let params = params_with_options(json!([
            { "kind": "allow_once" },
            { "optionId": "no", "kind": "reject_once" },
        ]));
        let req = PermissionRequest::parse_params(json!(1), &params).expect("parse");
        assert_eq!(
            req.auto_option().expect_err("must fail"),
            "allow_once option missing optionId"
        );
    }

    #[test]
    fn reject_once_without_option_id_defaults_to_reject() {
        // Quirk preserved from the inline implementation.
        let params = params_with_options(json!([{ "kind": "reject_once" }]));
        let req = PermissionRequest::parse_params(json!(1), &params).expect("parse");
        assert_eq!(req.auto_option(), Ok(AutoOption::RejectOnce("reject")));
    }

    #[test]
    fn no_usable_option_preserves_error_string() {
        let params = params_with_options(json!([
            { "optionId": "always", "kind": "allow_always" },
        ]));
        let req = PermissionRequest::parse_params(json!(1), &params).expect("parse");
        assert_eq!(
            req.auto_option().expect_err("must fail"),
            "no suitable permission option found (neither allow_once nor reject_once)"
        );
    }

    #[test]
    fn tool_call_update_requires_id() {
        assert!(ToolCallRef::from_update(&json!({ "title": "ls", "kind": "execute" })).is_none());
        let tc = ToolCallRef::from_update(&json!({
            "toolCallId": "call-1",
            "title": "ls",
            "kind": "execute",
        }))
        .expect("id present");
        assert_eq!(tc.id.as_deref(), Some("call-1"));
    }

    #[test]
    fn filled_from_prefers_request_fields_and_fills_gaps() {
        // claude-code-acp shape: the request carries only the id; title/kind/
        // input arrived on the announcing tool_call update.
        let sparse = ToolCallRef {
            id: Some("call-2".into()),
            ..Default::default()
        };
        let announced = ToolCallRef {
            id: Some("call-2".into()),
            title: Some("cargo test".into()),
            kind: Some("execute".into()),
            raw_input: Some(json!({ "command": "cargo test" })),
        };
        let resolved = sparse.filled_from(&announced);
        assert_eq!(resolved.title.as_deref(), Some("cargo test"));
        assert_eq!(resolved.kind.as_deref(), Some("execute"));
        assert_eq!(resolved.raw_input, Some(json!({ "command": "cargo test" })));

        // Request-side fields win over the announcement.
        let request_side = ToolCallRef {
            id: Some("call-2".into()),
            title: Some("cargo test -p buzz-core".into()),
            ..Default::default()
        };
        let resolved = request_side.filled_from(&announced);
        assert_eq!(resolved.title.as_deref(), Some("cargo test -p buzz-core"));
    }

    #[test]
    fn gate_action_full_access_never_parks() {
        assert_eq!(
            gate_action(RuntimeMode::FullAccess, Some("execute")),
            GateAction::AutoSelect
        );
        assert_eq!(
            gate_action(RuntimeMode::FullAccess, None),
            GateAction::AutoSelect
        );
    }

    #[test]
    fn gate_action_supervised_always_parks() {
        for kind in [Some("execute"), Some("read"), Some("edit"), None] {
            assert_eq!(gate_action(RuntimeMode::Supervised, kind), GateAction::Park);
        }
    }

    #[test]
    fn gate_action_auto_accept_edits_parks_non_file_ops() {
        assert_eq!(
            gate_action(RuntimeMode::AutoAcceptEdits, Some("read")),
            GateAction::AutoSelect
        );
        assert_eq!(
            gate_action(RuntimeMode::AutoAcceptEdits, Some("edit")),
            GateAction::AutoSelect
        );
        assert_eq!(
            gate_action(RuntimeMode::AutoAcceptEdits, Some("execute")),
            GateAction::Park
        );
        // Uncorrelated tool calls fail toward the gate, never around it.
        assert_eq!(
            gate_action(RuntimeMode::AutoAcceptEdits, None),
            GateAction::Park
        );
    }

    #[test]
    fn request_kind_taxonomy_mapping() {
        assert_eq!(request_kind_for_tool(Some("execute")), "command");
        assert_eq!(request_kind_for_tool(Some("read")), "file-read");
        assert_eq!(request_kind_for_tool(Some("edit")), "file-change");
        assert_eq!(request_kind_for_tool(Some("delete")), "file-change");
        assert_eq!(request_kind_for_tool(Some("move")), "file-change");
        assert_eq!(request_kind_for_tool(Some("think")), "other");
        assert_eq!(request_kind_for_tool(None), "other");
    }

    #[test]
    fn detail_bounded_to_400_chars_with_fallback() {
        let long_title = "x".repeat(1000);
        let tc = ToolCallRef {
            id: Some("c".into()),
            title: Some(long_title),
            ..Default::default()
        };
        assert_eq!(detail_for_tool(Some(&tc), "fallback").chars().count(), 400);
        assert_eq!(detail_for_tool(None, "fallback"), "fallback");
        let blank = ToolCallRef {
            title: Some("   ".into()),
            ..Default::default()
        };
        assert_eq!(detail_for_tool(Some(&blank), "fallback"), "fallback");
    }

    #[test]
    fn response_for_decision_maps_the_acp_vocabulary() {
        let params = params_with_options(json!([
            { "optionId": "yes", "kind": "allow_once" },
            { "optionId": "always", "kind": "allow_always" },
            { "optionId": "no", "kind": "reject_once" },
        ]));
        let req = PermissionRequest::parse_params(json!(1), &params).expect("parse");
        assert_eq!(
            req.response_for_decision("allow_once"),
            Some(DecisionResponse::Selected("yes"))
        );
        assert_eq!(
            req.response_for_decision("allow_always"),
            Some(DecisionResponse::Selected("always"))
        );
        assert_eq!(
            req.response_for_decision("reject_once"),
            Some(DecisionResponse::Selected("no"))
        );
        assert_eq!(
            req.response_for_decision("cancel"),
            Some(DecisionResponse::Cancelled)
        );
        assert_eq!(req.response_for_decision("bogus"), None);

        // allow_always degrades to allow_once when unoffered; a mapping miss
        // (granted but no allow option at all) yields None → cancelled.
        let sparse = PermissionRequest::parse_params(
            json!(2),
            &params_with_options(json!([{ "optionId": "yes", "kind": "allow_once" }])),
        )
        .expect("parse");
        assert_eq!(
            sparse.response_for_decision("allow_always"),
            Some(DecisionResponse::Selected("yes"))
        );
        let rejecting_only = PermissionRequest::parse_params(
            json!(3),
            &params_with_options(json!([{ "optionId": "no", "kind": "reject_once" }])),
        )
        .expect("parse");
        assert_eq!(rejecting_only.response_for_decision("allow_once"), None);
    }
}
