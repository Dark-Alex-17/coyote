use super::{FunctionDeclaration, JsonSchema};
use crate::config::{RequestContext, Skill, SkillPolicy, paths};
use crate::utils::create_abort_signal;

use anyhow::{Result, bail};
use indexmap::IndexMap;
use log::warn;
use serde_json::{Value, json};

pub const SKILL_FUNCTION_PREFIX: &str = "skill__";

pub fn skill_function_declarations() -> Vec<FunctionDeclaration> {
    vec![
        FunctionDeclaration {
            name: format!("{SKILL_FUNCTION_PREFIX}list"),
            description:
                "List skills available in this context. Returns each skill's name, description, \
                 what tools and MCP servers it grants on load, and whether it is currently loaded. \
                 Call this to discover skills before using skill__load."
                    .to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::new()),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{SKILL_FUNCTION_PREFIX}load"),
            description:
                "Load a skill module into the current context. The skill's instructions and any \
                 tools or MCP servers it grants become active for subsequent turns. Call \
                 skill__unload when the skill's work is complete to keep the context lean."
                    .to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([(
                    "name".to_string(),
                    JsonSchema {
                        type_value: Some("string".to_string()),
                        description: Some("Name of the skill to load.".into()),
                        ..Default::default()
                    },
                )])),
                required: Some(vec!["name".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{SKILL_FUNCTION_PREFIX}unload"),
            description:
                "Unload a previously loaded skill, removing its instructions and granted tools \
                 from the context. Call this when the skill's work is complete."
                    .to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([(
                    "name".to_string(),
                    JsonSchema {
                        type_value: Some("string".to_string()),
                        description: Some("Name of the skill to unload.".into()),
                        ..Default::default()
                    },
                )])),
                required: Some(vec!["name".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
    ]
}

pub async fn handle_skill_tool(
    ctx: &mut RequestContext,
    cmd_name: &str,
    args: &Value,
) -> Result<Value> {
    let action = cmd_name
        .strip_prefix(SKILL_FUNCTION_PREFIX)
        .unwrap_or(cmd_name);

    let policy = SkillPolicy::effective(
        &ctx.app.config,
        ctx.role.as_ref(),
        ctx.agent.as_ref(),
        ctx.session.as_ref(),
    )?;

    if !policy.skills_enabled {
        return Ok(json!({
            "error": "Skills are disabled in this context"
        }));
    }

    match action {
        "list" => handle_list(ctx, &policy),
        "load" => handle_load(ctx, args, &policy).await,
        "unload" => handle_unload(ctx, args).await,
        _ => bail!("Unknown skill action: {action}"),
    }
}

fn handle_list(ctx: &RequestContext, policy: &SkillPolicy) -> Result<Value> {
    let mcp_on = ctx.app.config.mcp_server_support;

    let visible_names: Vec<String> = match ctx.app.config.visible_skills.as_deref() {
        Some(list) => list.to_vec(),
        None => paths::list_skills(),
    };

    let mut entries = Vec::new();
    for name in visible_names {
        if !policy.allows(&name) {
            continue;
        }

        let skill = match Skill::load(&name) {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to load skill '{name}' for listing: {e}");
                continue;
            }
        };
        if !skill.is_compatible(mcp_on) {
            warn!(
                "Skill '{name}' filtered from list: declares MCP servers but MCP support is disabled"
            );
            continue;
        }

        entries.push(json!({
            "name": skill.name(),
            "description": skill.description(),
            "grants_tools": skill.enabled_tools().unwrap_or_default(),
            "grants_mcp_servers": skill.enabled_mcp_servers().unwrap_or_default(),
            "loaded": ctx.skill_registry.is_loaded(skill.name()),
        }));
    }

    Ok(json!({"skills": entries}))
}

async fn handle_load(
    ctx: &mut RequestContext,
    args: &Value,
    policy: &SkillPolicy,
) -> Result<Value> {
    let name = match args.get("name").and_then(Value::as_str) {
        Some(n) if !n.is_empty() => n,
        _ => return Ok(json!({"error": "name is required"})),
    };

    if !policy.allows(name) {
        return Ok(json!({
            "error": format!("Skill '{name}' is not enabled in this context")
        }));
    }

    let skill = match Skill::load(name) {
        Ok(s) => s,
        Err(e) => {
            return Ok(json!({
                "error": format!("Failed to load skill '{name}': {e}")
            }));
        }
    };

    let function_calling_on = ctx.app.config.function_calling_support;
    let mcp_on = ctx.app.config.mcp_server_support;

    let tools_declared = skill
        .enabled_tools()
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let mcps_declared = skill
        .enabled_mcp_servers()
        .map(|v| !v.is_empty())
        .unwrap_or(false);

    if tools_declared && !function_calling_on {
        return Ok(json!({
            "error": format!(
                "Skill '{name}' requires function calling, which is disabled in this context"
            )
        }));
    }
    if mcps_declared && !mcp_on {
        return Ok(json!({
            "error": format!(
                "Skill '{name}' requires MCP servers, which are disabled in this context"
            )
        }));
    }

    if let Err(e) = ctx.skill_registry.insert(skill) {
        return Ok(json!({"error": e.to_string()}));
    }

    if let Err(e) = ctx.refresh_tool_scope(create_abort_signal()).await {
        if let Err(unload_err) = ctx.skill_registry.unload(name) {
            warn!("Failed to unload skill '{name}' during error recovery: {unload_err}");
        }

        return Ok(json!({
            "error": format!("Loaded skill '{name}' but failed to refresh tool scope: {e}")
        }));
    }

    Ok(json!({
        "status": "ok",
        "loaded": name,
        "message": format!("Skill '{name}' loaded")
    }))
}

async fn handle_unload(ctx: &mut RequestContext, args: &Value) -> Result<Value> {
    let name = match args.get("name").and_then(Value::as_str) {
        Some(n) if !n.is_empty() => n,
        _ => return Ok(json!({"error": "name is required"})),
    };

    if let Err(e) = paths::validate_skill_name(name) {
        return Ok(json!({"error": e.to_string()}));
    }

    let skill = match ctx.skill_registry.unload(name) {
        Ok(s) => s,
        Err(e) => return Ok(json!({"error": e.to_string()})),
    };

    if let Err(e) = ctx.refresh_tool_scope(create_abort_signal()).await {
        if let Err(insert_err) = ctx.skill_registry.insert(skill) {
            warn!("Failed to restore skill '{name}' after unload recovery: {insert_err}");
        }

        return Ok(json!({
            "error": format!(
                "Unloaded skill '{name}' but failed to refresh tool scope; restored: {e}"
            )
        }));
    }

    Ok(json!({
        "status": "ok",
        "unloaded": name
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declarations_have_three_entries() {
        let decls = skill_function_declarations();
        assert_eq!(decls.len(), 3);
    }

    #[test]
    fn declaration_names_use_skill_prefix() {
        let decls = skill_function_declarations();

        let names: Vec<&str> = decls.iter().map(|d| d.name.as_str()).collect();

        assert!(names.contains(&"skill__list"));
        assert!(names.contains(&"skill__load"));
        assert!(names.contains(&"skill__unload"));
    }

    #[test]
    fn load_and_unload_require_name_parameter() {
        let decls = skill_function_declarations();
        for action in ["load", "unload"] {
            let decl = decls
                .iter()
                .find(|d| d.name == format!("skill__{action}"))
                .expect("missing declaration");

            let required = decl
                .parameters
                .required
                .as_ref()
                .expect("required field missing");

            assert!(required.contains(&"name".to_string()));
        }
    }

    #[test]
    fn list_has_no_required_parameters() {
        let decls = skill_function_declarations();
        let list_decl = decls
            .iter()
            .find(|d| d.name == "skill__list")
            .expect("skill__list missing");

        let required = list_decl
            .parameters
            .required
            .as_ref()
            .map(|v| v.is_empty())
            .unwrap_or(true);

        assert!(required, "skill__list should have no required parameters");
    }
}
