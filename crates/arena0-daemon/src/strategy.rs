//! Daemon-owned deterministic callout policies. The Host validates each answer.
use anyhow::anyhow;
use serde_json::Value;
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Strategy {
    FirstAllowed,
    Sample,
}

impl Strategy {
    pub(crate) const ALL: [Strategy; 2] = [Strategy::FirstAllowed, Strategy::Sample];

    /// Exact names only: "first-allowed", "sample".
    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "first-allowed" => Some(Self::FirstAllowed),
            "sample" => Some(Self::Sample),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::FirstAllowed => "first-allowed",
            Self::Sample => "sample",
        }
    }

    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::FirstAllowed => "Answers with the first value the callout's schema allows",
            Self::Sample => {
                "Plays the bundled example policy for the program, else the first allowed value"
            }
        }
    }

    pub(crate) fn answer(
        self,
        callout: &str,
        context: &Value,
        schema: &Value,
    ) -> anyhow::Result<Value> {
        match self {
            Self::FirstAllowed => first_allowed_answer(schema),
            Self::Sample => sample_answer(callout, context, schema),
        }
    }
}

fn first_allowed_answer(schema: &Value) -> anyhow::Result<Value> {
    let mut references = HashSet::new();
    first_allowed_value(schema, schema, &mut references, 0).ok_or_else(|| {
        anyhow!("built-in first-allowed requires a callout schema with an allowed enum")
    })
}

fn sample_answer(name: &str, context: &Value, schema: &Value) -> anyhow::Result<Value> {
    match name {
        "MakeMove" => context
            .get("legal_moves")
            .and_then(Value::as_str)
            .and_then(|moves| moves.split(',').map(str::trim).find(|mv| !mv.is_empty()))
            .map(|mv| Value::String(mv.to_owned()))
            .ok_or_else(|| anyhow!("sample chess policy received no legal moves")),
        "SubmitBid" => Ok(Value::from(0)),
        "SubmitOffer" => sample_offer(context),
        _ => first_allowed_answer(schema).map_err(|_| {
            anyhow!("built-in sample has no policy for callout '{name}' and its schema has no allowed enum")
        }),
    }
}

fn sample_offer(context: &Value) -> anyhow::Result<Value> {
    let tasks = context
        .get("tasks")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("sample contract-net policy requires a tasks array"))?;
    let maximum_capacity = context
        .get("maximum_capacity")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("sample contract-net policy requires maximum_capacity"))?;
    let maximum_cost = context
        .get("maximum_cost")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("sample contract-net policy requires maximum_cost"))?;
    let capacity = maximum_capacity.min(tasks.len() as u64);
    let mut capabilities = Vec::new();
    let mut bids = Vec::new();
    for (index, task) in tasks.iter().enumerate() {
        let capability = task
            .get("capability")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("sample contract-net task {index} has no capability"))?;
        if !capabilities.iter().any(|known| known == capability) {
            capabilities.push(capability.to_owned());
        }
        bids.push(serde_json::json!({
            "task": index,
            "cost": (index as u64 + 1).min(maximum_cost),
        }));
    }
    Ok(serde_json::json!({
        "capabilities": capabilities,
        "capacity": capacity,
        "bids": bids,
    }))
}

fn first_allowed_value(
    node: &Value,
    root: &Value,
    references: &mut HashSet<String>,
    depth: usize,
) -> Option<Value> {
    if depth > 32 {
        return None;
    }
    let object = node.as_object()?;
    if let Some(values) = object.get("enum").and_then(Value::as_array) {
        return values.first().cloned();
    }
    if let Some(value) = object.get("const") {
        return Some(value.clone());
    }
    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
        let pointer = reference.strip_prefix('#')?;
        if !references.insert(reference.to_owned()) {
            return None;
        }
        return root
            .pointer(pointer)
            .and_then(|value| first_allowed_value(value, root, references, depth + 1));
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(branches) = object.get(key).and_then(Value::as_array) {
            for branch in branches {
                if let Some(value) = first_allowed_value(branch, root, references, depth + 1) {
                    return Some(value);
                }
            }
        }
    }
    None
}
