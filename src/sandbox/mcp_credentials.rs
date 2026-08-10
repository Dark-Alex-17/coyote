use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use url::Url;

use crate::vault::SECRET_RE;

pub(crate) const MCP_MIXIN_NAME: &str = "coyote-mcp";

const ENV_VAR_PREFIX: &str = "COYOTE_SECRET_";
const SERVICE_ID_MAX_LEN: usize = 63;

pub(crate) fn secret_service_id(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_was_dash = false;
    for ch in name.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_lowercase() || lower.is_ascii_digit() {
            out.push(lower);
            last_was_dash = false;
        } else if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }

    let mut id: String = out
        .trim_matches('-')
        .chars()
        .take(SERVICE_ID_MAX_LEN)
        .collect();
    while id.ends_with('-') {
        id.pop();
    }

    id
}

pub(crate) fn sandbox_secret_env_var(name: &str) -> String {
    let mut out = String::from(ENV_VAR_PREFIX);
    for ch in name.chars() {
        let upper = ch.to_ascii_uppercase();
        if upper.is_ascii_uppercase() || upper.is_ascii_digit() {
            out.push(upper);
        } else {
            out.push('_');
        }
    }

    out
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub(crate) struct InjectRule {
    pub domain: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
}

impl InjectRule {
    fn effective_header(&self) -> String {
        self.header
            .as_deref()
            .unwrap_or("Authorization")
            .to_ascii_lowercase()
    }
}

/// One credential to register with sbx and declare in the generated mixin.
#[derive(Debug)]
pub(crate) struct CredentialSpec {
    pub secret_name: String,
    pub service_id: String,
    pub env_var: String,
    pub proxy_managed: bool,
    pub inject: Vec<InjectRule>,
    pub servers: Vec<String>,
}

struct Occurrence {
    secret_name: String,
    /// `Some` when the occurrence can be rewritten by the sandbox proxy.
    inject: Option<InjectRule>,
}

struct PlaceholderMatch {
    name: String,
    start: usize,
    end: usize,
}

/// Collects every `{{placeholder}}` across all MCP servers and folds them into
/// one credential per distinct secret name. A secret is proxy-managed only when
/// every occurrence of it (across all servers) is a proxy-injectable header
/// value AND no other secret targets the same (domain, header) injection slot
/// (proxy inject rules are keyed by domain + header, not by server).
pub(crate) fn collect_credentials(
    servers: &serde_json::Map<String, Value>,
) -> Result<Vec<CredentialSpec>> {
    struct Aggregate {
        all_injectable: bool,
        inject: BTreeSet<InjectRule>,
        servers: BTreeSet<String>,
    }

    let mut by_secret: BTreeMap<String, Aggregate> = BTreeMap::new();
    for (server_name, config) in servers {
        for occurrence in collect_server_occurrences(config)? {
            let agg = by_secret
                .entry(occurrence.secret_name)
                .or_insert_with(|| Aggregate {
                    all_injectable: true,
                    inject: BTreeSet::new(),
                    servers: BTreeSet::new(),
                });
            agg.servers.insert(server_name.clone());
            match occurrence.inject {
                Some(rule) => {
                    agg.inject.insert(rule);
                }
                None => agg.all_injectable = false,
            }
        }
    }

    // The proxy injects per (domain, header), not per server: sbx kits v2
    // spec supports multiple credentials on one domain as long as they
    // write different headers. A conflict exists only when two different
    // secrets target the SAME header on the same domain, then the proxy
    // cannot tell which credential a given request needs, and it could
    // clobber a header Coyote resolved from the environment. Demote
    // every secret involved in such a collision to env-based resolution.
    let mut secrets_by_slot: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for (secret_name, agg) in &by_secret {
        for rule in &agg.inject {
            secrets_by_slot
                .entry((rule.domain.clone(), rule.effective_header()))
                .or_default()
                .insert(secret_name.clone());
        }
    }

    let mut slot_conflicted: BTreeSet<String> = BTreeSet::new();
    for ((domain, header), secrets) in &secrets_by_slot {
        if secrets.len() > 1 {
            eprintln!(
                "MCP secrets {} all target the '{header}' header for '{domain}'. The \
                 sandbox proxy cannot tell which one a given request needs, so these \
                 secrets will be resolved from environment variables inside the \
                 sandbox instead.",
                quoted_list(secrets)
            );
            slot_conflicted.extend(secrets.iter().cloned());
        }
    }

    let mut credentials = Vec::with_capacity(by_secret.len());
    let mut secrets_by_service_id: BTreeMap<String, String> = BTreeMap::new();
    let mut secrets_by_env_var: BTreeMap<String, String> = BTreeMap::new();
    for (secret_name, agg) in by_secret {
        let service_id = secret_service_id(&secret_name);
        if service_id.is_empty() {
            bail!(
                "Secret name '{secret_name}' referenced by MCP server(s) {} sanitizes to an \
                 empty sbx service id",
                quoted_list(&agg.servers)
            );
        }

        if let Some(existing) =
            secrets_by_service_id.insert(service_id.clone(), secret_name.clone())
        {
            bail!(
                "MCP secrets '{existing}' and '{secret_name}' both sanitize to sbx service id \
                 '{service_id}'; rename one"
            );
        }

        let env_var = sandbox_secret_env_var(&secret_name);
        if let Some(existing) = secrets_by_env_var.insert(env_var.clone(), secret_name.clone()) {
            bail!(
                "MCP secrets '{existing}' and '{secret_name}' both sanitize to env var \
                 '{env_var}'; rename one"
            );
        }

        let proxy_managed = agg.all_injectable && !slot_conflicted.contains(&secret_name);
        credentials.push(CredentialSpec {
            secret_name,
            service_id,
            env_var,
            proxy_managed,
            inject: if proxy_managed {
                agg.inject.into_iter().collect()
            } else {
                Vec::new()
            },
            servers: agg.servers.into_iter().collect(),
        });
    }

    credentials.sort_by(|a, b| a.service_id.cmp(&b.service_id));

    Ok(credentials)
}

pub(crate) fn quoted_list<'a, I: IntoIterator<Item = &'a String>>(items: I) -> String {
    items
        .into_iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn collect_server_occurrences(server: &Value) -> Result<Vec<Occurrence>> {
    let Some(fields) = server.as_object() else {
        return Ok(Vec::new());
    };

    let https_domain = fields
        .get("url")
        .and_then(Value::as_str)
        .and_then(parse_https_domain);

    let mut out = Vec::new();
    for (key, value) in fields {
        if key == "headers"
            && let Some(headers) = value.as_object()
        {
            for (header, header_value) in headers {
                match header_value.as_str() {
                    Some(s) => classify_header_value(header, s, https_domain.as_deref(), &mut out)?,
                    None => collect_env_occurrences(header_value, &mut out)?,
                }
            }
            continue;
        }

        collect_env_occurrences(value, &mut out)?;
    }

    Ok(out)
}

fn collect_env_occurrences(value: &Value, out: &mut Vec<Occurrence>) -> Result<()> {
    match value {
        Value::String(s) => {
            for m in placeholders(s)? {
                out.push(Occurrence {
                    secret_name: m.name,
                    inject: None,
                });
            }
        }
        Value::Object(map) => {
            for v in map.values() {
                collect_env_occurrences(v, out)?;
            }
        }
        Value::Array(arr) => {
            for v in arr {
                collect_env_occurrences(v, out)?;
            }
        }
        _ => {}
    }

    Ok(())
}

/// A header value is proxy-injectable only when the server URL is https with a
/// parseable host, the value holds exactly one placeholder, and the literal
/// remainder contains no `%`.
fn classify_header_value(
    header: &str,
    value: &str,
    https_domain: Option<&str>,
    out: &mut Vec<Occurrence>,
) -> Result<()> {
    let matches = placeholders(value)?;
    if matches.is_empty() {
        return Ok(());
    }

    let rule = match (https_domain, matches.as_slice()) {
        (Some(domain), [only]) => {
            let remainder = format!("{}{}", &value[..only.start], &value[only.end..]);
            if remainder.contains('%') {
                None
            } else {
                Some(build_inject_rule(domain, header, value, only))
            }
        }
        _ => None,
    };

    match rule {
        Some(rule) => {
            let m = matches.into_iter().next().expect("checked non-empty");
            out.push(Occurrence {
                secret_name: m.name,
                inject: Some(rule),
            });
        }
        None => {
            for m in matches {
                out.push(Occurrence {
                    secret_name: m.name,
                    inject: None,
                });
            }
        }
    }

    Ok(())
}

fn build_inject_rule(domain: &str, header: &str, value: &str, m: &PlaceholderMatch) -> InjectRule {
    let is_bearer = header.eq_ignore_ascii_case("authorization")
        && value[..m.start].eq_ignore_ascii_case("Bearer ")
        && value[m.end..].is_empty();

    if is_bearer {
        InjectRule {
            domain: domain.to_string(),
            header: None,
            format: None,
            scheme: Some("bearer".to_string()),
        }
    } else {
        InjectRule {
            domain: domain.to_string(),
            header: Some(header.to_string()),
            format: Some(format!("{}%s{}", &value[..m.start], &value[m.end..])),
            scheme: None,
        }
    }
}

/// Returns the inject-rule `domain` for an https URL: the bare host on the
/// default port (443), or `host:port` for any other port. This mirrors the
/// enforced allow-list entry grammar. Bracketed IPv6 hosts and non-https
/// schemes are rejected so their occurrences fall back to env-based
/// resolution.
fn parse_https_domain(raw: &str) -> Option<String> {
    let url = Url::parse(raw).ok()?;
    if url.scheme() != "https" {
        return None;
    }

    let host = url.host_str()?;
    if host.is_empty() || host.starts_with('[') {
        return None;
    }

    match url.port() {
        None => Some(host.to_string()),
        Some(port) => Some(format!("{host}:{port}")),
    }
}

/// Collects a network allow-list entry for every remote MCP server `url`
/// (http/https), so user-configured servers are reachable regardless of how,
/// or whether, their credentials are provisioned. Https on the default port
/// yields a bare host; any other port is spelled `host:port` (both enforced
/// entry formats). Bracketed IPv6 hosts are skipped — the sbx kit v2 spec's
/// allow grammar only covers named hosts.
pub(crate) fn collect_server_allow_entries(
    servers: &serde_json::Map<String, Value>,
) -> Vec<String> {
    let mut out = BTreeSet::new();
    for config in servers.values() {
        let Some(url) = config.get("url").and_then(Value::as_str) else {
            continue;
        };
        if let Some(entry) = allow_entry_for_url(url) {
            out.insert(entry);
        }
    }

    out.into_iter().collect()
}

/// The single definition of the sbx kit v2 allow-list entry grammar: https on
/// the default port yields a bare host, anything else is spelled `host:port`.
/// Bracketed IPv6 hosts and non-http(s) schemes have no representation in the
/// grammar and yield `None`.
pub(crate) fn allow_entry_for_url(raw: &str) -> Option<String> {
    let url = Url::parse(raw).ok()?;
    let scheme = url.scheme();
    if scheme != "https" && scheme != "http" {
        return None;
    }

    let host = url.host_str()?;
    if host.is_empty() || host.starts_with('[') {
        return None;
    }

    match url.port_or_known_default() {
        Some(443) if scheme == "https" => Some(host.to_string()),
        Some(port) => Some(format!("{host}:{port}")),
        None => None,
    }
}

fn placeholders(text: &str) -> Result<Vec<PlaceholderMatch>> {
    let mut out = Vec::new();
    for caps in SECRET_RE.captures_iter(text) {
        let caps = caps.context("Failed to scan for secret placeholders")?;
        let full = caps.get(0).expect("capture group 0 always exists");
        out.push(PlaceholderMatch {
            name: caps[1].trim().to_string(),
            start: full.start(),
            end: full.end(),
        });
    }

    Ok(out)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CredentialsMixin {
    schema_version: &'static str,
    kind: &'static str,
    name: String,
    description: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    credentials: Vec<CredentialEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    permissions: Option<Permissions>,
}

#[derive(Serialize)]
pub(crate) struct CredentialEntry {
    pub service: String,
    pub description: String,
    #[serde(rename = "apiKey")]
    pub api_key: ApiKey,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ApiKey {
    pub name: String,
    pub proxy_managed: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub inject: Vec<InjectRule>,
}

#[derive(Serialize)]
struct Permissions {
    network: Network,
}

#[derive(Serialize)]
struct Network {
    allow: Vec<String>,
}

/// Serializes one sbx kit v2 mixin document.
///
/// Every `inject[].domain` is unioned into `permissions.network.allow`: sbx
/// does not derive allow entries from inject rules, so a rule whose domain is
/// not allowed would be dead. Enforcing it here keeps the invariant in one
/// place for every mixin Coyote generates.
pub(crate) fn render_mixin_document(
    name: &str,
    description: &str,
    credentials: Vec<CredentialEntry>,
    extra_allow_entries: &[String],
) -> Result<String> {
    let mut allow: BTreeSet<String> = credentials
        .iter()
        .flat_map(|c| c.api_key.inject.iter().map(|r| r.domain.clone()))
        .collect();
    allow.extend(extra_allow_entries.iter().cloned());

    let mixin = CredentialsMixin {
        schema_version: "2",
        kind: "mixin",
        name: name.to_string(),
        description: description.to_string(),
        credentials,
        permissions: (!allow.is_empty()).then(|| Permissions {
            network: Network {
                allow: allow.into_iter().collect(),
            },
        }),
    };

    serde_yaml::to_string(&mixin).context("Failed to serialize generated sandbox mixin")
}

pub(crate) fn render_mixin_yaml(
    credentials: &[CredentialSpec],
    server_allow_entries: &[String],
) -> Result<String> {
    let entries = credentials
        .iter()
        .map(|c| CredentialEntry {
            service: c.service_id.clone(),
            description: format!(
                "Coyote vault secret '{}', used by MCP server(s) {}",
                c.secret_name,
                quoted_list(&c.servers)
            ),
            api_key: ApiKey {
                name: c.env_var.clone(),
                proxy_managed: c.proxy_managed,
                inject: c.inject.clone(),
            },
        })
        .collect();

    render_mixin_document(
        MCP_MIXIN_NAME,
        "Auto-generated by Coyote at launch: allows network egress to the user's remote MCP \
         servers and declares their credentials so Docker Sandboxes binds them (bindings are \
         approved on first interactive run). Values are pre-seeded from Coyote's vault via \
         `sbx secret set`.",
        entries,
        server_allow_entries,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Map, json};

    fn servers(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn secret_service_id_lowercases_and_dashes() {
        assert_eq!(secret_service_id("GITHUB_PAT"), "github-pat");
    }

    #[test]
    fn secret_service_id_collapses_and_trims() {
        assert_eq!(secret_service_id("__My..Token__"), "my-token");
    }

    #[test]
    fn secret_service_id_truncates_without_trailing_dash() {
        let long = format!("{}-{}", "a".repeat(62), "b".repeat(10));

        let id = secret_service_id(&long);

        assert_eq!(id.len(), 62, "trailing dash at char 63 must be dropped");
        assert!(!id.ends_with('-'));
    }

    #[test]
    fn secret_service_id_all_invalid_yields_empty() {
        assert_eq!(secret_service_id("..."), "");
    }

    #[test]
    fn sandbox_secret_env_var_uppercases_and_underscores() {
        assert_eq!(
            sandbox_secret_env_var("GITHUB_PAT"),
            "COYOTE_SECRET_GITHUB_PAT"
        );
        assert_eq!(
            sandbox_secret_env_var("notion-token"),
            "COYOTE_SECRET_NOTION_TOKEN"
        );
    }

    #[test]
    fn collects_every_placeholder_not_just_the_first() {
        let servers = servers(json!({
            "multi": {
                "command": "run",
                "args": ["--token", "{{TOKEN_A}}"],
                "env": {
                    "FIRST": "{{TOKEN_B}}",
                    "COMBINED": "{{TOKEN_C}}:{{TOKEN_D}}"
                }
            }
        }));

        let creds = collect_credentials(&servers).unwrap();

        let names: Vec<&str> = creds.iter().map(|c| c.secret_name.as_str()).collect();

        assert_eq!(names, vec!["TOKEN_A", "TOKEN_B", "TOKEN_C", "TOKEN_D"]);
        assert!(creds.iter().all(|c| !c.proxy_managed));
    }

    #[test]
    fn bearer_authorization_header_is_proxy_managed_with_bearer_scheme() {
        let servers = servers(json!({
            "github": {
                "url": "https://api.githubcopilot.com/mcp/",
                "headers": { "Authorization": "Bearer {{GITHUB_PAT}}" }
            }
        }));

        let creds = collect_credentials(&servers).unwrap();
        assert_eq!(creds.len(), 1);
        let cred = &creds[0];
        assert_eq!(cred.service_id, "github-pat");
        assert_eq!(cred.env_var, "COYOTE_SECRET_GITHUB_PAT");
        assert!(cred.proxy_managed);
        assert_eq!(
            cred.inject,
            vec![InjectRule {
                domain: "api.githubcopilot.com".to_string(),
                header: None,
                format: None,
                scheme: Some("bearer".to_string()),
            }]
        );
    }

    #[test]
    fn custom_header_becomes_format_rule() {
        let servers = servers(json!({
            "notion": {
                "url": "https://mcp.notion.com/sse",
                "headers": { "X-Api-Key": "token {{notion-token}}" }
            }
        }));

        let creds = collect_credentials(&servers).unwrap();

        assert_eq!(creds.len(), 1);
        assert!(creds[0].proxy_managed);
        assert_eq!(
            creds[0].inject,
            vec![InjectRule {
                domain: "mcp.notion.com".to_string(),
                header: Some("X-Api-Key".to_string()),
                format: Some("token %s".to_string()),
                scheme: None,
            }]
        );
    }

    #[test]
    fn lowercase_bearer_prefix_is_proxy_managed() {
        let servers = servers(json!({
            "svc": {
                "url": "https://api.example.com/mcp",
                "headers": { "authorization": "bearer {{KEY}}" }
            }
        }));

        let creds = collect_credentials(&servers).unwrap();

        assert_eq!(creds.len(), 1);
        assert!(creds[0].proxy_managed);
        assert_eq!(creds[0].inject[0].scheme.as_deref(), Some("bearer"));
        assert!(creds[0].inject[0].header.is_none());
    }

    #[test]
    fn non_default_https_port_is_proxy_managed_with_port_in_domain() {
        let servers = servers(json!({
            "svc": {
                "url": "https://api.example.com:8443/mcp",
                "headers": { "Authorization": "Bearer {{KEY}}" }
            }
        }));

        let creds = collect_credentials(&servers).unwrap();

        assert_eq!(creds.len(), 1);
        assert!(creds[0].proxy_managed);
        assert_eq!(
            creds[0].inject,
            vec![InjectRule {
                domain: "api.example.com:8443".to_string(),
                header: None,
                format: None,
                scheme: Some("bearer".to_string()),
            }]
        );
    }

    #[test]
    fn port_inject_domain_is_included_in_rendered_allow_list() {
        let servers = servers(json!({
            "svc": {
                "url": "https://api.example.com:8443/mcp",
                "headers": { "Authorization": "Bearer {{KEY}}" }
            }
        }));

        let creds = collect_credentials(&servers).unwrap();
        let allow = collect_server_allow_entries(&servers);
        let yaml = render_mixin_yaml(&creds, &allow).unwrap();
        let value: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();

        let rendered_allow = value["permissions"]["network"]["allow"]
            .as_sequence()
            .unwrap();

        assert_eq!(
            rendered_allow
                .iter()
                .filter(|v| v.as_str() == Some("api.example.com:8443"))
                .count(),
            1,
            "host:port must appear exactly once (inject domain deduped against server allow entry)"
        );
    }

    #[test]
    fn explicit_default_port_is_omitted_from_domain() {
        let servers = servers(json!({
            "svc": {
                "url": "https://api.example.com:443/mcp",
                "headers": { "Authorization": "Bearer {{KEY}}" }
            }
        }));

        let creds = collect_credentials(&servers).unwrap();

        assert_eq!(creds[0].inject[0].domain, "api.example.com");
    }

    #[test]
    fn multi_placeholder_header_falls_back_to_env() {
        let servers = servers(json!({
            "svc": {
                "url": "https://api.example.com/mcp",
                "headers": { "Authorization": "Basic {{USER}}:{{PASS}}" }
            }
        }));

        let creds = collect_credentials(&servers).unwrap();

        assert_eq!(creds.len(), 2);
        assert!(creds.iter().all(|c| !c.proxy_managed));
        assert!(creds.iter().all(|c| c.inject.is_empty()));
    }

    #[test]
    fn percent_in_header_remainder_falls_back_to_env() {
        let servers = servers(json!({
            "svc": {
                "url": "https://api.example.com/mcp",
                "headers": { "X-Key": "100%-{{KEY}}" }
            }
        }));

        let creds = collect_credentials(&servers).unwrap();

        assert!(!creds[0].proxy_managed);
    }

    #[test]
    fn non_https_url_falls_back_to_env() {
        let servers = servers(json!({
            "svc": {
                "url": "http://api.example.com/mcp",
                "headers": { "Authorization": "Bearer {{KEY}}" }
            }
        }));

        let creds = collect_credentials(&servers).unwrap();

        assert!(!creds[0].proxy_managed);
    }

    #[test]
    fn url_placeholder_is_env_based() {
        let servers = servers(json!({
            "svc": { "url": "https://api.example.com/mcp?key={{KEY}}" }
        }));

        let creds = collect_credentials(&servers).unwrap();

        assert_eq!(creds[0].secret_name, "KEY");
        assert!(!creds[0].proxy_managed);
    }

    #[test]
    fn env_occurrence_in_another_server_demotes_proxy_managed() {
        let servers = servers(json!({
            "remote": {
                "url": "https://api.example.com/mcp",
                "headers": { "Authorization": "Bearer {{SHARED}}" }
            },
            "local": {
                "command": "run",
                "env": { "SHARED": "{{SHARED}}" }
            }
        }));

        let creds = collect_credentials(&servers).unwrap();

        assert_eq!(creds.len(), 1);
        assert!(!creds[0].proxy_managed);
        assert!(creds[0].inject.is_empty());
        assert_eq!(creds[0].servers, vec!["local", "remote"]);
    }

    #[test]
    fn identical_inject_rules_across_servers_are_deduped() {
        let servers = servers(json!({
            "a": {
                "url": "https://api.example.com/one",
                "headers": { "Authorization": "Bearer {{KEY}}" }
            },
            "b": {
                "url": "https://api.example.com/two",
                "headers": { "Authorization": "Bearer {{KEY}}" }
            }
        }));

        let creds = collect_credentials(&servers).unwrap();

        assert_eq!(creds.len(), 1);
        assert!(creds[0].proxy_managed);
        assert_eq!(creds[0].inject.len(), 1);
    }

    #[test]
    fn same_domain_different_secrets_demotes_both_to_env() {
        let servers = servers(json!({
            "github-personal": {
                "url": "https://api.githubcopilot.com/mcp/",
                "headers": { "Authorization": "Bearer {{GITHUB_PAT_PERSONAL}}" }
            },
            "github-work": {
                "url": "https://api.githubcopilot.com/mcp/",
                "headers": { "Authorization": "Bearer {{GITHUB_PAT_WORK}}" }
            }
        }));

        let creds = collect_credentials(&servers).unwrap();

        assert_eq!(creds.len(), 2);
        assert!(
            creds.iter().all(|c| !c.proxy_managed),
            "secrets sharing an inject domain must both fall back to env"
        );
        assert!(creds.iter().all(|c| c.inject.is_empty()));
    }

    #[test]
    fn slot_conflict_demotes_secret_on_all_its_domains() {
        let servers = servers(json!({
            "a-shared": {
                "url": "https://shared.example.com/mcp",
                "headers": { "Authorization": "Bearer {{KEY_A}}" }
            },
            "a-solo": {
                "url": "https://solo.example.com/mcp",
                "headers": { "Authorization": "Bearer {{KEY_A}}" }
            },
            "b-shared": {
                "url": "https://shared.example.com/mcp",
                "headers": { "Authorization": "Bearer {{KEY_B}}" }
            }
        }));

        let creds = collect_credentials(&servers).unwrap();

        assert_eq!(creds.len(), 2);
        assert!(
            creds
                .iter()
                .all(|c| !c.proxy_managed && c.inject.is_empty()),
            "a secret demoted by a slot conflict must lose ALL its inject rules, \
             not just the conflicting one"
        );
    }

    #[test]
    fn different_headers_on_same_domain_stay_proxy_managed() {
        let servers = servers(json!({
            "svc": {
                "url": "https://api.example.com/mcp",
                "headers": {
                    "Authorization": "Bearer {{KEY_A}}",
                    "X-Api-Key": "{{KEY_B}}"
                }
            }
        }));

        let creds = collect_credentials(&servers).unwrap();
        assert_eq!(creds.len(), 2);
        assert!(
            creds.iter().all(|c| c.proxy_managed),
            "v2 allows multiple credentials per domain when they target \
             different headers; neither secret may be demoted"
        );
        let a = creds.iter().find(|c| c.secret_name == "KEY_A").unwrap();
        assert_eq!(a.inject[0].scheme.as_deref(), Some("bearer"));
        let b = creds.iter().find(|c| c.secret_name == "KEY_B").unwrap();
        assert_eq!(b.inject[0].header.as_deref(), Some("X-Api-Key"));
    }

    #[test]
    fn bearer_scheme_conflicts_with_explicit_authorization_header() {
        let servers = servers(json!({
            "bearer-style": {
                "url": "https://api.example.com/mcp",
                "headers": { "Authorization": "Bearer {{KEY_A}}" }
            },
            "token-style": {
                "url": "https://api.example.com/mcp",
                "headers": { "authorization": "token {{KEY_B}}" }
            }
        }));

        let creds = collect_credentials(&servers).unwrap();

        assert_eq!(creds.len(), 2);
        assert!(
            creds
                .iter()
                .all(|c| !c.proxy_managed && c.inject.is_empty()),
            "`scheme: bearer` writes the Authorization header, so it must \
             conflict with an explicit (case-insensitive) Authorization rule"
        );
    }

    #[test]
    fn env_based_secret_header_occurrence_demotes_injectable_sharer() {
        let servers = servers(json!({
            "remote": {
                "url": "https://api.example.com/mcp",
                "headers": { "Authorization": "Bearer {{KEY_A}}" }
            },
            "remote-twin": {
                "url": "https://api.example.com/mcp",
                "headers": { "Authorization": "Bearer {{KEY_B}}" }
            },
            "local": {
                "command": "run",
                "env": { "KEY_B": "{{KEY_B}}" }
            }
        }));

        let creds = collect_credentials(&servers).unwrap();
        assert_eq!(creds.len(), 2);
        let a = creds.iter().find(|c| c.secret_name == "KEY_A").unwrap();
        assert!(
            !a.proxy_managed && a.inject.is_empty(),
            "KEY_B is env-based, but its header occurrence on the shared domain \
             must still demote KEY_A — the proxy would rewrite KEY_B's requests too"
        );
    }

    #[test]
    fn unsanitizable_secret_name_is_an_error() {
        let servers = servers(json!({
            "svc": { "env": { "KEY": "{{...}}" } }
        }));

        let err = collect_credentials(&servers).unwrap_err().to_string();

        assert!(err.contains("empty sbx service id"), "got: {err}");
    }

    #[test]
    fn colliding_sanitized_secret_names_are_an_error() {
        let servers = servers(json!({
            "svc": { "env": { "A": "{{MY_KEY}}", "B": "{{my-key}}" } }
        }));

        let err = collect_credentials(&servers).unwrap_err().to_string();

        assert!(
            err.contains("'MY_KEY'") && err.contains("'my-key'"),
            "error must name both secrets, got: {err}"
        );
        assert!(
            err.contains("service id 'my-key'"),
            "error must name the colliding service id, got: {err}"
        );
    }

    #[test]
    fn rendered_mixin_declares_proxy_managed_credential_and_permissions() {
        let servers = servers(json!({
            "github": {
                "url": "https://api.githubcopilot.com/mcp/",
                "headers": { "Authorization": "Bearer {{GITHUB_PAT}}" }
            }
        }));

        let creds = collect_credentials(&servers).unwrap();
        let yaml = render_mixin_yaml(&creds, &[]).unwrap();
        let value: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(value["schemaVersion"].as_str(), Some("2"));
        assert_eq!(value["kind"].as_str(), Some("mixin"));
        assert_eq!(value["name"].as_str(), Some("coyote-mcp"));

        let cred = &value["credentials"][0];
        assert_eq!(cred["service"].as_str(), Some("github-pat"));
        let description = cred["description"].as_str().unwrap();
        assert!(
            description.contains("'GITHUB_PAT'") && description.contains("'github'"),
            "binding-prompt description must name the vault secret and its server(s), got: {description}"
        );
        assert_eq!(
            cred["apiKey"]["name"].as_str(),
            Some("COYOTE_SECRET_GITHUB_PAT")
        );
        assert_eq!(cred["apiKey"]["proxyManaged"].as_bool(), Some(true));
        let inject = &cred["apiKey"]["inject"][0];
        assert_eq!(inject["domain"].as_str(), Some("api.githubcopilot.com"));
        assert_eq!(inject["scheme"].as_str(), Some("bearer"));
        assert!(inject.get("header").is_none());
        assert!(inject.get("format").is_none());

        assert_eq!(
            value["permissions"]["network"]["allow"][0].as_str(),
            Some("api.githubcopilot.com")
        );
    }

    #[test]
    fn rendered_mixin_omits_inject_and_permissions_for_env_based_secrets() {
        let servers = servers(json!({
            "local": { "command": "run", "env": { "KEY": "{{NOTION_TOKEN}}" } }
        }));

        let creds = collect_credentials(&servers).unwrap();
        let yaml = render_mixin_yaml(&creds, &[]).unwrap();
        let value: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();

        let cred = &value["credentials"][0];
        assert_eq!(cred["apiKey"]["proxyManaged"].as_bool(), Some(false));
        assert!(cred["apiKey"].get("inject").is_none());
        assert!(value.get("permissions").is_none());
    }

    #[test]
    fn rendered_mixin_is_deterministic() {
        let servers = servers(json!({
            "b": {
                "url": "https://b.example.com/mcp",
                "headers": { "Authorization": "Bearer {{ZULU}}" }
            },
            "a": {
                "url": "https://a.example.com/mcp",
                "headers": { "Authorization": "Bearer {{ALPHA}}" }
            }
        }));

        let creds = collect_credentials(&servers).unwrap();
        let services: Vec<&str> = creds.iter().map(|c| c.service_id.as_str()).collect();

        assert_eq!(services, vec!["alpha", "zulu"]);
        assert_eq!(
            render_mixin_yaml(&creds, &[]).unwrap(),
            render_mixin_yaml(&creds, &[]).unwrap()
        );
    }

    #[test]
    fn allow_entry_for_url_formats() {
        assert_eq!(
            allow_entry_for_url("https://api.example.com/mcp"),
            Some("api.example.com".to_string())
        );
        assert_eq!(
            allow_entry_for_url("https://api.example.com:443/mcp"),
            Some("api.example.com".to_string())
        );
        assert_eq!(
            allow_entry_for_url("https://api.example.com:8443/mcp"),
            Some("api.example.com:8443".to_string())
        );
        assert_eq!(
            allow_entry_for_url("http://api.example.com/mcp"),
            Some("api.example.com:80".to_string())
        );
        assert_eq!(allow_entry_for_url("ws://api.example.com/mcp"), None);
        assert_eq!(allow_entry_for_url("https://[::1]:8443/mcp"), None);
        assert_eq!(allow_entry_for_url("not a url"), None);
    }

    #[test]
    fn collect_server_allow_entries_covers_secretless_servers_and_dedupes() {
        let servers = servers(json!({
            "no-secret": { "url": "https://open.example.com/mcp" },
            "no-secret-twin": { "url": "https://open.example.com/mcp" },
            "with-secret": {
                "url": "https://api.example.com/mcp",
                "headers": { "Authorization": "Bearer {{KEY}}" }
            },
            "stdio": { "command": "uvx", "args": ["some-mcp-server"] }
        }));

        assert_eq!(
            collect_server_allow_entries(&servers),
            vec![
                "api.example.com".to_string(),
                "open.example.com".to_string()
            ]
        );
    }

    #[test]
    fn demoted_secret_server_domain_stays_in_allow() {
        let servers = servers(json!({
            "a": {
                "url": "https://shared.example.com/mcp",
                "headers": { "Authorization": "Bearer {{KEY_A}}" }
            },
            "b": {
                "url": "https://shared.example.com/mcp",
                "headers": { "Authorization": "Bearer {{KEY_B}}" }
            }
        }));

        let creds = collect_credentials(&servers).unwrap();
        assert!(creds.iter().all(|c| !c.proxy_managed), "precondition");

        let entries = collect_server_allow_entries(&servers);
        let yaml = render_mixin_yaml(&creds, &entries).unwrap();
        let value: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(
            value["permissions"]["network"]["allow"][0].as_str(),
            Some("shared.example.com"),
            "demoting a credential must not cut the server's egress"
        );
    }

    #[test]
    fn mixin_with_no_credentials_still_declares_network_allow() {
        let servers = servers(json!({
            "open": { "url": "https://open.example.com/mcp" }
        }));

        let creds = collect_credentials(&servers).unwrap();
        assert!(creds.is_empty(), "precondition: no secrets referenced");

        let entries = collect_server_allow_entries(&servers);
        let yaml = render_mixin_yaml(&creds, &entries).unwrap();
        let value: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();

        assert!(
            value.get("credentials").is_none(),
            "empty credentials list must be omitted under strict decoding hygiene"
        );
        assert_eq!(
            value["permissions"]["network"]["allow"][0].as_str(),
            Some("open.example.com")
        );
    }
}
