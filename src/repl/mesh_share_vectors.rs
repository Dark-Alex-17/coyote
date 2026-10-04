//! Requirement-id keyed vectors for the REPL's `attachment` predicate: the file the human
//! names held to the share rules of section 10.17, as MESH-SHARE-020 has it. Every row
//! runs in-process on a share root the row builds in a temporary directory, through the
//! request context the REPL hands the predicate.
//!
//! The family lives beside the predicate rather than with the other share vectors
//! because the mesh module may not name the request context, and these rows must build
//! one. The conformance registry in `crate::mesh::conformance` still lists the rows and
//! credits their executor, so the coverage report counts them with the rest.
//!
//! Every row names the id it exercises and the receiver action the spec mandates for it.
//! The rows read the config dir from the process env, so the executor serialises and holds
//! a config-dir guard.

use crate::config::{AppState, RequestContext, WorkingMode};
use crate::mesh::conformance::{Kind, Listed};
use crate::mesh::fetch::SINGLE_SEGMENT_FETCH_CEILING;
use crate::mesh::message::{PartLimits, RawPart};
use crate::mesh::shares::SHARES_FILE_VERSION;
use crate::mesh::test_support::{TempDir, snapshot_fixture};
use crate::repl::mesh::{AttachForm, Attachment, attachment};

use sha2::{Digest, Sha256};
use std::fmt::Debug;
use std::fs;
use std::sync::Arc;

const FAMILY: &str = "Attachment";

/// A scenario that builds its own fixture and reports the first expectation it misses.
type Check = fn() -> Result<(), String>;

/// One requirement id, one input, one mandated receiver action.
struct Vector {
    id: &'static str,
    kind: Kind,
    check: Check,
}

pub(crate) fn listed() -> Vec<Listed> {
    vectors()
        .iter()
        .map(|vector| Listed {
            id: vector.id,
            kind: vector.kind,
            family: FAMILY,
        })
        .collect()
}

// ---------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------

fn same<T: Debug + PartialEq>(what: &str, observed: T, expected: T) -> Result<(), String> {
    if observed == expected {
        Ok(())
    } else {
        Err(format!(
            "{what}: expected {expected:?}, observed {observed:?}"
        ))
    }
}

fn ensure(condition: bool, what: impl Into<String>) -> Result<(), String> {
    if condition { Ok(()) } else { Err(what.into()) }
}

fn missing_phrases(text: &str, phrases: &[&str]) -> Result<(), String> {
    let missing: Vec<&str> = phrases
        .iter()
        .copied()
        .filter(|phrase| !text.contains(phrase))
        .collect();
    ensure(
        missing.is_empty(),
        format!("refusal lacks {missing:?}: {text}"),
    )
}

/// A share file of this build's version carrying only the given deny list.
fn shares_yaml(deny: &[&str]) -> String {
    let mut text = format!("version: {SHARES_FILE_VERSION}\n");
    if !deny.is_empty() {
        text.push_str("deny:\n");
        for pattern in deny {
            text.push_str(&format!("- pattern: '{pattern}'\n"));
        }
    }
    text
}

fn sha256_of(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

// ---------------------------------------------------------------------------------------
// Checks: the attachment predicate
// ---------------------------------------------------------------------------------------

/// A share root named by a published snapshot, as the REPL has it with the mesh off,
/// its global share file carrying `deny`.
struct AttachRoot {
    _tmp: TempDir,
    ctx: RequestContext,
}

impl AttachRoot {
    fn build(tag: &str, files: &[(&str, &[u8])], deny: &[&str]) -> Result<Self, String> {
        let tmp = TempDir::new(tag);
        for (relative, bytes) in files {
            let path = tmp.path.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
        let ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Repl);
        let mut snapshot = snapshot_fixture();
        snapshot.cwd = tmp.path.clone();
        ctx.app.mesh.publish(snapshot);
        let (root, locations) = ctx
            .share_locations()
            .ok_or("the published snapshot names no share root")?;
        same("share root", root, tmp.path.clone())?;
        fs::create_dir_all(locations.global.parent().unwrap()).unwrap();
        fs::write(&locations.global, shares_yaml(deny)).unwrap();
        Ok(Self { _tmp: tmp, ctx })
    }

    fn attach(&self, path: &str, force: bool, inline_max: u64) -> Result<Attachment, String> {
        let limits = PartLimits {
            inline_max_bytes: inline_max,
        };
        attachment(
            &self.ctx,
            path,
            force,
            &limits,
            SINGLE_SEGMENT_FETCH_CEILING,
        )
        .map_err(|err| err.to_string())
    }
}

fn inline_of(attached: &Attachment, name: &str, bytes: &[u8]) -> Result<(), String> {
    same("form", attached.form, AttachForm::Inline)?;
    same("name", attached.name.as_str(), name)?;
    same("size", attached.size, bytes.len() as u64)?;
    same(
        "part",
        &attached.part,
        &RawPart::File {
            name: name.to_string(),
            size: bytes.len() as u64,
            sha256: sha256_of(bytes),
            bytes: Some(bytes.to_vec()),
            reference: None,
        },
    )
}

fn refused_with(observed: Result<Attachment, String>, phrases: &[&str]) -> Result<(), String> {
    match observed {
        Ok(attached) => Err(format!(
            "expected a refusal naming {phrases:?}, observed {:?} `{}`",
            attached.form, attached.name
        )),
        Err(text) => missing_phrases(&text, phrases),
    }
}

fn an_inline_attachment_travels_whatever_the_allow_and_deny_lists_say() -> Result<(), String> {
    let fx = AttachRoot::build(
        "attach-inline-bypass",
        &[
            ("docs/notes.md", b"notes\n".as_slice()),
            ("docs/private.md", b"mine\n".as_slice()),
        ],
        &["docs/private.md"],
    )?;
    let unlisted = fx.attach("docs/notes.md", false, 1024)?;
    inline_of(&unlisted, "docs/notes.md", b"notes\n")?;
    let denied = fx.attach("docs/private.md", false, 1024)?;
    inline_of(&denied, "docs/private.md", b"mine\n")
}

fn an_attachment_under_the_protected_set_is_refused_with_or_without_force() -> Result<(), String> {
    let fx = AttachRoot::build(
        "attach-protected",
        &[
            (".coyote/settings.yaml", b"x\n".as_slice()),
            (".git/HEAD", b"ref: refs/heads/main\n".as_slice()),
        ],
        &[],
    )?;
    for path in [".coyote/settings.yaml", ".git/HEAD"] {
        for force in [false, true] {
            refused_with(
                fx.attach(path, force, 1024),
                &["never shared", "`--force`", "nothing was sent."],
            )
            .map_err(|err| format!("{path} force={force}: {err}"))?;
        }
    }
    Ok(())
}

fn the_built_in_deny_refuses_an_attachment_until_force_lifts_it() -> Result<(), String> {
    let fx = AttachRoot::build(
        "attach-builtin-deny",
        &[(".env", b"SECRET=1\n".as_slice())],
        &[],
    )?;
    refused_with(
        fx.attach(".env", false, 1024),
        &["built-in deny", "`--force` attaches it anyway"],
    )?;
    let forced = fx.attach(".env", true, 1024)?;
    inline_of(&forced, ".env", b"SECRET=1\n")
}

fn a_file_above_the_inline_limit_travels_by_reference() -> Result<(), String> {
    let big = vec![0x5a; 1025];
    let fx = AttachRoot::build("attach-reference", &[("docs/big.bin", &big)], &[])?;
    let at_limit = fx.attach("docs/big.bin", false, 1025)?;
    inline_of(&at_limit, "docs/big.bin", &big)?;
    let referenced = fx.attach("docs/big.bin", false, 1024)?;
    same("form", referenced.form, AttachForm::Reference)?;
    same("size", referenced.size, 1025)?;
    same(
        "part",
        &referenced.part,
        &RawPart::File {
            name: "docs/big.bin".to_string(),
            size: 1025,
            sha256: sha256_of(&big),
            bytes: None,
            reference: Some("docs/big.bin".to_string()),
        },
    )
}

fn a_reference_the_serving_side_would_not_serve_is_refused() -> Result<(), String> {
    let big = vec![0x5a; 1025];
    let fx = AttachRoot::build(
        "attach-unservable-reference",
        &[("docs/private.md", &big), (".env", &big)],
        &["docs/private.md"],
    )?;
    refused_with(
        fx.attach("docs/private.md", false, 1024),
        &[
            "`docs/private.md` would travel as a reference, which this node would not serve (a deny rule names it); lift the rule first; nothing was sent.",
        ],
    )?;
    refused_with(
        fx.attach(".env", true, 1024),
        &[
            "`.env` would travel as a reference, which this node would not serve (the built-in deny names it); lift the rule first; nothing was sent.",
        ],
    )
}

// ---------------------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------------------

fn vectors() -> Vec<Vector> {
    use Kind::{Boundary, Invalid, Valid};
    vec![
        row(
            "MESH-SHARE-020",
            Valid,
            an_inline_attachment_travels_whatever_the_allow_and_deny_lists_say,
        ),
        row(
            "MESH-SHARE-020",
            Invalid,
            an_attachment_under_the_protected_set_is_refused_with_or_without_force,
        ),
        row(
            "MESH-SHARE-020",
            Invalid,
            the_built_in_deny_refuses_an_attachment_until_force_lifts_it,
        ),
        row(
            "MESH-SHARE-020",
            Boundary,
            a_file_above_the_inline_limit_travels_by_reference,
        ),
        row(
            "MESH-SHARE-020",
            Invalid,
            a_reference_the_serving_side_would_not_serve_is_refused,
        ),
    ]
}

fn row(id: &'static str, kind: Kind, check: Check) -> Vector {
    Vector { id, kind, check }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn attachments_are_held_to_section_10_17_as_the_human_named_them() {
        let _guard = crate::testing::TestConfigDirGuard::new("conformance-attachment");
        let rows = vectors();
        let ran = rows.len();
        let failures: Vec<String> = rows
            .iter()
            .filter_map(|vector| {
                (vector.check)()
                    .err()
                    .map(|detail| format!("{} [{FAMILY}, {:?}]: {detail}", vector.id, vector.kind))
            })
            .collect();
        assert!(ran > 0, "no {FAMILY} vectors");
        assert!(
            failures.is_empty(),
            "{} of {ran} {FAMILY} vectors failed:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}
