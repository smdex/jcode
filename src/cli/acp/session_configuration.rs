//! ACP model and reasoning effort configuration presentation.

use super::*;

pub(super) const CONFIG_ID_MODEL: &str = "model";
pub(super) const CONFIG_ID_EFFORT: &str = "reasoning_effort";
pub(super) const CONFIG_ID_MODE: &str = "mode";
pub(super) const CONFIG_ID_SUBAGENT_MODEL: &str = "subagent_model";
pub(super) const CONFIG_ID_COMPACTION: &str = "compaction_mode";

pub(super) fn acp_available_commands() -> Vec<Value> {
    vec![
        json!({
            "name": "model",
            "description": "Switch the model for this session, or show the current model",
            "input": { "hint": "model id (optional)" },
        }),
        json!({
            "name": "models",
            "description": "List models available from the active provider",
        }),
        json!({
            "name": "effort",
            "description": "Set reasoning effort, or show the current effort",
            "input": { "hint": "none|minimal|low|medium|high|xhigh|max|swarm (optional)" },
        }),
        json!({"name": "subagent", "description": "Launch a general subagent with streamed tool progress", "input": {"hint": "prompt"}}),
        json!({"name": "subagent-model", "description": "Set the session subagent model policy", "input": {"hint": "model id|inherit (optional)"}}),
        json!({"name": "skills", "description": "Reload installed skills in the current daemon session", "input": {"hint": "reload"}}),
    ]
}

pub(super) fn insert_session_configuration(result: &mut Value, state: &SessionUiState) {
    let Some(object) = result.as_object_mut() else {
        return;
    };
    let config_options = session_config_options(state);
    if !config_options.is_empty() {
        object.insert("configOptions".to_string(), Value::Array(config_options));
    }
    if let Some(models) = session_models(state) {
        insert_session_modes(object, state);
        object.insert("models".to_string(), models);
    }
}

pub(super) fn session_models(state: &SessionUiState) -> Option<Value> {
    let current = state.model.as_deref()?;
    let mut models = state.available_models.clone();
    if !models.iter().any(|candidate| candidate == current) {
        models.insert(0, current.to_string());
    }
    Some(json!({
        "availableModels": models
            .into_iter()
            .map(|model| json!({ "modelId": model, "name": model }))
            .collect::<Vec<_>>(),
        "currentModelId": current,
    }))
}

pub(super) fn session_mode(state: &SessionUiState) -> &str {
    match state.reasoning_effort.as_deref() {
        Some("swarm") => "swarm",
        Some("swarm-deep") => "swarm-deep",
        _ => "solo",
    }
}

pub(super) fn mode_options(state: &SessionUiState) -> Vec<Value> {
    let mut modes = vec![
        json!({"value": "solo", "name": "Solo", "description": "Normal agent execution with the selected provider effort"}),
    ];
    for effort in available_efforts(state)
        .into_iter()
        .filter(|effort| effort.starts_with("swarm"))
    {
        modes.push(json!({"value": effort, "name": if effort == "swarm" { "Swarm" } else { "Swarm Deep" }, "description": if effort == "swarm" { "Light parallel fan-out, only the root spawns workers" } else { "Deep task graph with recursive worker coordination" }}));
    }
    modes
}

fn insert_session_modes(object: &mut serde_json::Map<String, Value>, state: &SessionUiState) {
    object.insert("modes".into(), json!({
        "currentModeId": session_mode(state),
        "availableModes": mode_options(state).into_iter().map(|mode| json!({"id": mode["value"], "name": mode["name"], "description": mode["description"]})).collect::<Vec<_>>()
    }));
}

pub(super) fn available_efforts(state: &SessionUiState) -> Vec<&'static str> {
    crate::provider::inferred_reasoning_efforts(
        state.provider_name.as_deref(),
        state.model.as_deref(),
    )
}

/// Build the ACP `configOptions` array (model selector plus reasoning effort)
/// from the current session provider state. Empty when the daemon reported no
/// usable model state.
pub(super) fn session_config_options(state: &SessionUiState) -> Vec<Value> {
    let mut options = Vec::new();

    if let Some(model) = state.model.as_deref() {
        let mut models = state.available_models.clone();
        if !models.iter().any(|candidate| candidate == model) {
            models.insert(0, model.to_string());
        }
        let select_options: Vec<Value> = models
            .iter()
            .map(|name| json!({ "value": name, "name": name }))
            .collect();
        options.push(json!({
            "type": "select",
            "id": CONFIG_ID_MODEL,
            "name": "Model",
            "category": "model",
            "currentValue": model,
            "options": select_options,
        }));
        options.push(json!({
            "type": "select", "id": CONFIG_ID_MODE, "name": "Operation mode", "category": "mode",
            "description": "Native orchestration, not a permission or read-only policy. Leaving swarm selects medium effort.",
            "currentValue": session_mode(state), "options": mode_options(state),
        }));
        let mut subagent_models = vec![json!({"value": "inherit", "name": "Inherit active model"})];
        if let Some(model) = state
            .subagent_model
            .as_ref()
            .filter(|model| !models.contains(model))
        {
            models.push(model.clone());
        }
        subagent_models.extend(
            models
                .iter()
                .map(|model| json!({"value": model, "name": model})),
        );
        options.push(json!({
            "type": "select", "id": CONFIG_ID_SUBAGENT_MODEL, "name": "Subagent model", "category": "_subagent",
            "currentValue": state.subagent_model.as_deref().unwrap_or("inherit"), "options": subagent_models,
        }));
        options.push(json!({
            "type": "select", "id": CONFIG_ID_COMPACTION, "name": "Context compaction", "category": "_context",
            "currentValue": state.compaction_mode.as_str(),
            "options": (["reactive", "proactive", "semantic"].map(|mode| json!({"value": mode, "name": mode}))),
        }));
    }

    let efforts = available_efforts(state);
    if let Some(current) = state
        .reasoning_effort
        .as_deref()
        .filter(|effort| efforts.contains(effort))
    {
        let select_options: Vec<Value> = efforts
            .iter()
            .map(|name| json!({ "value": name, "name": name }))
            .collect();
        options.push(json!({
            "type": "select",
            "id": CONFIG_ID_EFFORT,
            "name": "Reasoning effort",
            "category": "thought_level",
            "currentValue": current,
            "options": select_options,
        }));
    }

    options
}
