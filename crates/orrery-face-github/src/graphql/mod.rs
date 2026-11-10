//! The GitHub GraphQL face: a real executor over a hand-grown subset of
//! GitHub's schema (`types`), mounted at `/api/graphql` — the GHES sibling of
//! `/api/v3`, never nested under it.
//!
//! The NOT-MOCKED rule translated to GraphQL: the endpoint itself is always
//! matched, so coverage gaps surface as *validation* errors — and a bare
//! "Unknown field" would be a plausible lie, because the field usually does
//! exist on real GitHub. `mark_unmocked_errors` therefore rewrites every
//! validation-shaped error into the self-describing orrery envelope (message
//! prefix + `extensions.orrery.not_mocked` + the same NOT MOCKED log line the
//! REST fallback emits), keeping "GitHub doesn't have this" and "Orrery
//! doesn't implement this yet" impossible to confuse. Execution-time errors
//! (an invalid transition, an unresolvable id) pass through vendor-faithful.

mod search;
mod types;

use std::collections::BTreeSet;

use async_graphql::parser::types::{
    BaseType, DocumentOperations, ExecutableDocument, OperationDefinition, Type,
};
use async_graphql::{EmptySubscription, Name, Schema, Value, Variables};
use axum::body::Body;
use axum::http::{Response, StatusCode};
use orrery_world::Caller;

use crate::GithubFace;

pub type GithubSchema = Schema<types::QueryRoot, types::MutationRoot, EmptySubscription>;

pub fn schema(face: GithubFace) -> GithubSchema {
    Schema::build(types::QueryRoot, types::MutationRoot, EmptySubscription)
        .data(face)
        .finish()
}

#[derive(serde::Deserialize)]
struct HttpRequest {
    query: String,
    #[serde(default)]
    variables: Option<serde_json::Value>,
    #[serde(default, rename = "operationName")]
    operation_name: Option<String>,
}

/// Execute one GraphQL POST body for `caller`. Always 200 with the standard
/// `{data, errors}` envelope (GitHub's behavior for query-level failures);
/// only an unparseable request body is an HTTP-level 400.
pub async fn handle(schema: &GithubSchema, caller: Caller, body: &[u8]) -> Response<Body> {
    let Ok(req) = serde_json::from_slice::<HttpRequest>(body) else {
        return crate::github_error(StatusCode::BAD_REQUEST, "Problems parsing JSON");
    };
    let parsed = async_graphql::parser::parse_query(&req.query);
    let mut greq = async_graphql::Request::new(req.query)
        .data(caller.actor.clone())
        .data(caller);
    // A document that doesn't parse is left for `execute` to parse again and
    // answer with its own error.
    if let Ok(mut doc) = parsed {
        empty_defaults_of_unknown_list_types(schema, &mut doc);
        greq.set_parsed_query(doc);
    }
    if let Some(vars) = req.variables {
        greq = greq.variables(Variables::from_json(vars));
    }
    if let Some(op) = req.operation_name {
        greq = greq.operation_name(op);
    }
    let resp = schema.execute(greq).await;
    let mut json = match serde_json::to_value(&resp) {
        Ok(v) => v,
        Err(e) => return crate::internal(e),
    };
    mark_unmocked_errors(&mut json);
    crate::json_response(StatusCode::OK, json)
}

/// The executor's default-value check panics on a list-typed variable whose
/// item type the schema lacks (it guards only a variable whose type is a bare
/// name), which drops the connection instead of answering. Each such default
/// becomes `[]`, which is valid for every list type, so the check has no item
/// to look up, and every other rule still sees a variable with a default. The
/// document then fails validation on the unknown type, as it does without the
/// default, so it never executes and the replaced value is never read.
///
/// `names()` holds every type name and also every field, argument, and enum
/// value name, so a name it lacks is certainly not a type; a default whose
/// type is known is never touched.
fn empty_defaults_of_unknown_list_types(schema: &GithubSchema, doc: &mut ExecutableDocument) {
    let operations: Vec<&mut OperationDefinition> = match &mut doc.operations {
        DocumentOperations::Single(op) => vec![&mut op.node],
        DocumentOperations::Multiple(ops) => ops.values_mut().map(|op| &mut op.node).collect(),
    };
    let mut names: Option<BTreeSet<String>> = None;
    for op in operations {
        for def in &mut op.variable_definitions {
            let def = &mut def.node;
            let (BaseType::List(item), Some(default)) =
                (&def.var_type.node.base, &mut def.default_value)
            else {
                continue;
            };
            let names = names.get_or_insert_with(|| schema.names().into_iter().collect());
            if !names.contains(item_type_name(item).as_str()) {
                default.node = Value::List(Vec::new());
            }
        }
    }
}

fn item_type_name(ty: &Type) -> &Name {
    match &ty.base {
        BaseType::Named(name) => name,
        BaseType::List(item) => item_type_name(item),
    }
}

/// Errors that mean "this query needs schema Orrery doesn't have" get the
/// self-describing envelope. The match is on the executor's validation
/// vocabulary ("Unknown field/type/argument/...", "Invalid value for ...")
/// plus anything a resolver already stamped with the `orrery:` prefix; a
/// valid-on-GitHub query can only fail those ways here by hitting a coverage
/// gap, so over-matching a genuinely malformed query is harmless — it would
/// have failed on GitHub too, just with a different label.
fn mark_unmocked_errors(response: &mut serde_json::Value) {
    let Some(errors) = response.get_mut("errors").and_then(|e| e.as_array_mut()) else {
        return;
    };
    for err in errors {
        let Some(message) = err.get("message").and_then(|m| m.as_str()) else {
            continue;
        };
        let coverage_gap = message.starts_with("orrery:")
            || message.contains("Unknown")
            || message.contains("Invalid value for");
        if !coverage_gap {
            continue;
        }
        tracing::warn!(%message, "NOT MOCKED: GraphQL surface gap");
        if !message.starts_with("orrery:") {
            err["message"] = serde_json::Value::String(format!("orrery (not mocked): {message}"));
        }
        err["extensions"]["orrery"] = serde_json::json!({
            "not_mocked": true,
            "hint": "If a consumer needs this GraphQL surface, add it to the surface inventory (spec/surface-github.md) and implement it.",
        });
    }
}
