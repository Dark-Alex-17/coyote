use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde_yaml::Value;

use crate::sandbox::kit::{self, CAP_CREDENTIAL, CAP_LIFECYCLE, KitDir};
use crate::sandbox::mixins::DiscoveredKit;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Base,
    Discovered,
    Generated,
}

struct Member {
    kit: KitDir,
    origin: Origin,
}

/// Checks the kits composed into one sandbox against each other: the base
/// kit, every discovered kit and, last, the generated `coyote-mcp` kit. Each
/// kit already passed its own validation; this catches what only the full
/// set reveals and returns the first failure. For a duplicated credential
/// `(service, phase)` or lifecycle file path the earliest member owns it, so
/// a collision caused by the generated kit is reported against the kit that
/// declared the entry first, naming the vault secret behind the colliding
/// service id (`generated_secret_names`).
///
/// Not checked, by design: Dockerfile `ENV` conflicts between kits (sbx
/// refuses those itself when it merges the composition, and a mixin kit
/// carries no Dockerfile here anyway) and install-phase network egress (each
/// kit's own install allow list stands; what an install hook fetches is not
/// judged against the other kits).
pub(crate) fn check(
    base: &KitDir,
    kits: &[DiscoveredKit],
    generated: Option<&KitDir>,
    generated_secret_names: &BTreeMap<String, String>,
) -> Result<()> {
    let mut members = vec![Member {
        kit: base.clone(),
        origin: Origin::Base,
    }];
    members.extend(kits.iter().map(|kit| Member {
        kit: KitDir {
            dir: kit.dir.clone(),
            handle: kit.handle.clone(),
            doc: kit.doc.clone(),
            dockerfile: None,
        },
        origin: Origin::Discovered,
    }));
    members.extend(generated.map(|kit| Member {
        kit: kit.clone(),
        origin: Origin::Generated,
    }));

    check_reserved_handles(&members)?;
    check_handles(&members)?;
    check_credential_phases(&members, generated_secret_names)?;
    check_lifecycle_files(&members)?;
    if let Some(kit) = generated {
        kit.check_mixin().with_context(|| {
            format!(
                "generated kit at {} is not a valid mixin kit",
                kit.dir.display()
            )
        })?;
    }
    Ok(())
}

fn check_handles(members: &[Member]) -> Result<()> {
    for (index, member) in members.iter().enumerate() {
        if let Some(seen) = members[..index]
            .iter()
            .find(|seen| seen.kit.handle == member.kit.handle)
        {
            bail!(
                "sbx kit handle '{}' is declared twice: {} and {}",
                member.kit.handle,
                seen.kit.dir.display(),
                member.kit.dir.display()
            );
        }
    }
    Ok(())
}

fn check_reserved_handles(members: &[Member]) -> Result<()> {
    for member in members
        .iter()
        .filter(|member| member.origin == Origin::Discovered)
    {
        if kit::is_reserved_handle(&member.kit.handle) {
            bail!(
                "sbx kit at {} uses the reserved handle '{}'",
                member.kit.dir.display(),
                member.kit.handle
            );
        }
    }
    Ok(())
}

fn check_credential_phases(
    members: &[Member],
    generated_secret_names: &BTreeMap<String, String>,
) -> Result<()> {
    let mut owners: BTreeMap<(&str, String), &Member> = BTreeMap::new();
    for member in members {
        for entry in member
            .kit
            .capabilities()
            .filter(|entry| kit::cap_type(entry) == Some(CAP_CREDENTIAL))
        {
            let config = entry.get("config");
            let Some(service) = config
                .and_then(|c| c.get("service"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            for phase in member.kit.credential_phases(config)? {
                let Some(owner) = owners.get(&(service, phase.clone())) else {
                    owners.insert((service, phase), member);
                    continue;
                };
                let remedy = if member.origin == Origin::Generated {
                    let secret = generated_secret_names
                        .get(service)
                        .map_or(service, String::as_str);
                    match owner.origin {
                        Origin::Base => format!(
                            "; rename the vault secret '{secret}' (or its env placeholder) so \
                             its sandbox service id differs from the base kit's '{service}'"
                        ),
                        _ => format!(
                            "; rename the vault secret '{secret}' (or its env placeholder) so \
                             its sandbox service id differs from the '{service}' declared by \
                             kit '{}' at {} or drop that credential from the kit",
                            owner.kit.handle,
                            owner.kit.dir.display()
                        ),
                    }
                } else {
                    String::new()
                };
                bail!(
                    "kit dir '{}' declares credential service '{service}' for phase '{phase}', \
                     already declared by kit dir '{}'{remedy}",
                    member.kit.dir.display(),
                    owner.kit.dir.display()
                );
            }
        }
    }
    Ok(())
}

fn check_lifecycle_files(members: &[Member]) -> Result<()> {
    let mut owners: BTreeMap<&str, &Member> = BTreeMap::new();
    for member in members {
        let paths = member
            .kit
            .capabilities()
            .filter(|entry| kit::cap_type(entry) == Some(CAP_LIFECYCLE))
            .filter_map(|entry| entry.get("config"))
            .filter_map(|config| config.get("files"))
            .filter_map(Value::as_sequence)
            .flatten()
            .filter_map(|file| file.get("path"))
            .filter_map(Value::as_str);
        for path in paths {
            if let Some(owner) = owners.get(path) {
                bail!(
                    "kit dir '{}' lifecycle file path '{path}' is already written by kit dir '{}'",
                    member.kit.dir.display(),
                    owner.kit.dir.display()
                );
            }
            owners.insert(path, member);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BASE_KIT_HANDLE;
    use crate::sandbox::kit::{KitSummary, SYNTAX_LINE};
    use crate::sandbox::mcp_credentials::{
        CredentialSpec, MCP_MIXIN_NAME, render_mixin_yaml, secret_service_id,
    };
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    const BASE_FILE_PATH: &str = "/home/agent/.config/base";

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(label: &str) -> Self {
            // The clock alone can collide between parallel tests sharing a label.
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = env::temp_dir().join(format!("coyote-preflight-{label}-{nanos}-{seq}"));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn kit_dir(root: &Path, handle: &str, yaml: &str) -> PathBuf {
        let dir = root.join(handle);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{handle}.yaml")), yaml).unwrap();
        dir
    }

    fn minimal(kind: &str) -> String {
        format!("{SYNTAX_LINE}\nschemaVersion: \"3\"\nkind: {kind}\n")
    }

    fn credential(service: &str, phase: &str) -> String {
        format!(
            "  - type: {CAP_CREDENTIAL}\n    config:\n      service: {service}\n{phase}      apiKey:\n        name: KEY\n        proxyManaged: true\n"
        )
    }

    fn files(paths: &[&str]) -> String {
        let mut out = format!("  - type: {CAP_LIFECYCLE}\n    config:\n      files:\n");
        for path in paths {
            out.push_str(&format!(
                "        - path: {path}\n          mode: \"0644\"\n          content: hi\n"
            ));
        }
        out
    }

    fn with_capabilities(kind: &str, capabilities: &[String]) -> String {
        format!("{}capabilities:\n{}", minimal(kind), capabilities.concat())
    }

    fn base_kit(root: &Path) -> KitDir {
        let yaml = with_capabilities(
            "workload",
            &[
                credential("openai", "      phase: runtime\n"),
                files(&[BASE_FILE_PATH]),
            ],
        );
        kit::load(&kit_dir(root, BASE_KIT_HANDLE, &yaml)).unwrap()
    }

    fn discovered(root: &Path, handle: &str, yaml: &str) -> DiscoveredKit {
        let kit = kit::load(&kit_dir(root, handle, yaml)).unwrap();
        DiscoveredKit {
            dir: kit.dir,
            handle: kit.handle,
            label: "config".to_string(),
            summary: KitSummary::default(),
            doc: kit.doc,
        }
    }

    fn mixin(root: &Path, handle: &str, capabilities: &[String]) -> DiscoveredKit {
        discovered(root, handle, &with_capabilities("mixin", capabilities))
    }

    fn generated(root: &Path, secret_name: &str) -> KitDir {
        let spec = CredentialSpec {
            secret_name: secret_name.to_string(),
            service_id: secret_service_id(secret_name),
            env_var: "KEY".to_string(),
            proxy_managed: true,
            inject: vec![],
            custom_hosts: vec![],
            custom_allow_entries: vec![],
            servers: vec!["srv".to_string()],
        };
        let yaml = render_mixin_yaml(&[spec], &[]).unwrap();
        kit::load(&kit_dir(root, MCP_MIXIN_NAME, &yaml)).unwrap()
    }

    fn secret_names(secret_name: &str) -> BTreeMap<String, String> {
        BTreeMap::from([(secret_service_id(secret_name), secret_name.to_string())])
    }

    fn err(
        base: &KitDir,
        kits: &[DiscoveredKit],
        generated: Option<&KitDir>,
        names: &BTreeMap<String, String>,
    ) -> String {
        format!("{:#}", check(base, kits, generated, names).unwrap_err())
    }

    fn err_plain(base: &KitDir, kits: &[DiscoveredKit]) -> String {
        err(base, kits, None, &BTreeMap::new())
    }

    #[test]
    fn rejects_duplicate_handle_across_discovered_kits() {
        let root = TempRoot::new("dup-handle");
        let base = base_kit(root.path());
        let a = mixin(&root.path().join("a"), "same", &[]);
        let b = mixin(&root.path().join("b"), "same", &[]);

        let msg = err_plain(&base, &[a.clone(), b.clone()]);

        assert!(msg.contains("'same'"), "{msg}");
        assert!(msg.contains(&a.dir.display().to_string()), "{msg}");
        assert!(msg.contains(&b.dir.display().to_string()), "{msg}");
    }

    #[test]
    fn rejects_duplicate_rag_prefixed_handle() {
        let root = TempRoot::new("dup-rag");
        let base = base_kit(root.path());
        let a = mixin(&root.path().join("a"), "rag-same", &[]);
        let b = mixin(&root.path().join("b"), "rag-same", &[]);

        let msg = err_plain(&base, &[a.clone(), b.clone()]);

        assert!(msg.contains("'rag-same'"), "{msg}");
        assert!(msg.contains(&a.dir.display().to_string()), "{msg}");
        assert!(msg.contains(&b.dir.display().to_string()), "{msg}");
    }

    /// The generated kit also carries this handle, so the reserved-handle
    /// sentence must win over the duplicate-handle one.
    #[test]
    fn rejects_discovered_kit_with_generated_handle() {
        let root = TempRoot::new("reserved-mcp");
        let base = base_kit(root.path());
        let kit = mixin(&root.path().join("user"), MCP_MIXIN_NAME, &[]);
        let mcp = generated(root.path(), "svc");
        let names = secret_names("svc");

        let msg = err(&base, std::slice::from_ref(&kit), Some(&mcp), &names);

        assert!(msg.contains("reserved handle 'coyote-mcp'"), "{msg}");
        assert!(msg.contains(&kit.dir.display().to_string()), "{msg}");
        assert!(!msg.contains("declared twice"), "{msg}");
    }

    #[test]
    fn rejects_discovered_kit_with_base_handle() {
        let root = TempRoot::new("reserved-base");
        let base = base_kit(root.path());
        let kit = mixin(&root.path().join("user"), BASE_KIT_HANDLE, &[]);

        let msg = err_plain(&base, std::slice::from_ref(&kit));

        assert!(msg.contains("reserved handle 'coyote'"), "{msg}");
        assert!(msg.contains(&kit.dir.display().to_string()), "{msg}");
        assert!(!msg.contains("declared twice"), "{msg}");
    }

    #[test]
    fn accepts_rag_prefixed_handle() {
        let root = TempRoot::new("rag-ok");
        let base = base_kit(root.path());
        let kit = mixin(root.path(), "rag-x", &[]);

        check(&base, &[kit], None, &BTreeMap::new()).unwrap();
    }

    #[test]
    fn rejects_user_credential_colliding_with_base() {
        let root = TempRoot::new("cred-base");
        let base = base_kit(root.path());
        let kit = mixin(
            root.path(),
            "user",
            &[credential("openai", "      phase: runtime\n")],
        );

        let msg = err_plain(&base, std::slice::from_ref(&kit));

        assert!(
            msg.contains("service 'openai' for phase 'runtime'"),
            "{msg}"
        );
        assert!(
            msg.contains(&format!(
                "already declared by kit dir '{}'",
                base.dir.display()
            )),
            "{msg}"
        );
        assert!(msg.contains(&kit.dir.display().to_string()), "{msg}");
        assert!(!msg.contains("rename the vault secret"), "{msg}");
    }

    #[test]
    fn rejects_two_user_kits_declaring_the_same_service() {
        let root = TempRoot::new("cred-users");
        let base = base_kit(root.path());
        let a = mixin(
            root.path(),
            "a",
            &[credential("svc", "      phase: runtime\n")],
        );
        let b = mixin(
            root.path(),
            "b",
            &[credential("svc", "      phase: runtime\n")],
        );

        let msg = err_plain(&base, &[a.clone(), b.clone()]);

        assert!(msg.contains("service 'svc' for phase 'runtime'"), "{msg}");
        assert!(
            msg.contains(&format!(
                "already declared by kit dir '{}'",
                a.dir.display()
            )),
            "{msg}"
        );
        assert!(msg.contains(&b.dir.display().to_string()), "{msg}");
    }

    #[test]
    fn accepts_same_service_on_a_different_phase() {
        let root = TempRoot::new("cred-phase");
        let base = base_kit(root.path());
        let kit = mixin(
            root.path(),
            "user",
            &[credential("openai", "      phase: install\n")],
        );

        check(&base, &[kit], None, &BTreeMap::new()).unwrap();
    }

    #[test]
    fn rejects_phase_sequence_colliding_on_one_element() {
        let root = TempRoot::new("cred-seq");
        let base = base_kit(root.path());
        let kit = mixin(
            root.path(),
            "user",
            &[credential("openai", "      phase: [install, runtime]\n")],
        );

        let msg = err_plain(&base, &[kit]);

        assert!(
            msg.contains("service 'openai' for phase 'runtime'"),
            "{msg}"
        );
    }

    #[test]
    fn treats_absent_phase_as_runtime() {
        let root = TempRoot::new("cred-default");
        let base = base_kit(root.path());
        let kit = mixin(root.path(), "user", &[credential("openai", "")]);

        let msg = err_plain(&base, &[kit]);

        assert!(
            msg.contains("service 'openai' for phase 'runtime'"),
            "{msg}"
        );
    }

    #[test]
    fn rejects_duplicate_file_path_across_kits() {
        let root = TempRoot::new("files-across");
        let base = base_kit(root.path());
        let kit = mixin(root.path(), "user", &[files(&[BASE_FILE_PATH])]);

        let msg = err_plain(&base, std::slice::from_ref(&kit));

        assert!(msg.contains(BASE_FILE_PATH), "{msg}");
        assert!(msg.contains(&base.dir.display().to_string()), "{msg}");
        assert!(msg.contains(&kit.dir.display().to_string()), "{msg}");
    }

    #[test]
    fn rejects_duplicate_credential_within_one_kit() {
        let root = TempRoot::new("cred-within");
        let base = base_kit(root.path());
        let kit = mixin(
            root.path(),
            "user",
            &[credential("svc", ""), credential("svc", "")],
        );

        let msg = err_plain(&base, std::slice::from_ref(&kit));

        assert!(msg.contains("service 'svc' for phase 'runtime'"), "{msg}");
        assert!(msg.contains(&kit.dir.display().to_string()), "{msg}");
    }

    #[test]
    fn rejects_duplicate_file_path_within_one_kit() {
        let root = TempRoot::new("files-within");
        let base = base_kit(root.path());
        let kit = mixin(root.path(), "user", &[files(&["/tmp/x", "/tmp/x"])]);

        let msg = err_plain(&base, std::slice::from_ref(&kit));

        assert!(msg.contains("'/tmp/x'"), "{msg}");
        assert!(msg.contains(&kit.dir.display().to_string()), "{msg}");
    }

    #[test]
    fn generated_collision_with_base_carries_remedy() {
        let root = TempRoot::new("gen-base");
        let base = base_kit(root.path());
        let mcp = generated(root.path(), "openai");
        let names = secret_names("openai");

        let msg = err(&base, &[], Some(&mcp), &names);

        assert!(
            msg.contains(&format!(
                "already declared by kit dir '{}'",
                base.dir.display()
            )),
            "{msg}"
        );
        assert!(
            msg.contains(
                "rename the vault secret 'openai' (or its env placeholder) so its sandbox \
                 service id differs from the base kit's 'openai'"
            ),
            "{msg}"
        );
    }

    #[test]
    fn generated_collision_with_user_kit_names_user_kit_as_owner() {
        let root = TempRoot::new("gen-user");
        let base = base_kit(root.path());
        let kit = mixin(root.path(), "user", &[credential("svc", "")]);
        let mcp = generated(root.path(), "svc");
        let names = secret_names("svc");

        let msg = err(&base, std::slice::from_ref(&kit), Some(&mcp), &names);

        assert!(
            msg.contains(&format!(
                "already declared by kit dir '{}'",
                kit.dir.display()
            )),
            "{msg}"
        );
        assert!(msg.contains(&mcp.dir.display().to_string()), "{msg}");
        assert!(
            msg.contains(&format!(
                "rename the vault secret 'svc' (or its env placeholder) so its sandbox \
                     service id differs from the 'svc' declared by kit 'user' at {} or drop \
                     that credential from the kit",
                kit.dir.display()
            )),
            "{msg}"
        );
    }

    #[test]
    fn generated_remedy_falls_back_to_the_service_id() {
        let root = TempRoot::new("gen-fallback");
        let base = base_kit(root.path());
        let mcp = generated(root.path(), "openai");

        let msg = err(&base, &[], Some(&mcp), &BTreeMap::new());

        assert!(msg.contains("rename the vault secret 'openai'"), "{msg}");
    }

    #[test]
    fn rejects_generated_kit_that_is_not_a_mixin() {
        let root = TempRoot::new("gen-kind");
        let base = base_kit(root.path());
        let mcp = kit::load(&kit_dir(root.path(), MCP_MIXIN_NAME, &minimal("workload"))).unwrap();

        let msg = err(&base, &[], Some(&mcp), &BTreeMap::new());

        assert!(
            msg.contains(&format!(
                "generated kit at {} is not a valid mixin kit",
                mcp.dir.display()
            )),
            "{msg}"
        );
        assert!(msg.contains("must have kind 'mixin'"), "{msg}");
    }

    #[test]
    fn accepts_distinct_mixins_and_a_non_colliding_generated_kit() {
        let root = TempRoot::new("happy");
        let base = base_kit(root.path());
        let a = mixin(
            root.path(),
            "a",
            &[credential("alpha", ""), files(&["/tmp/a"])],
        );
        let b = mixin(root.path(), "b", &[credential("beta", "")]);
        let mcp = generated(root.path(), "Gamma Key");
        let names = secret_names("Gamma Key");

        check(&base, &[a, b], Some(&mcp), &names).unwrap();
    }
}
