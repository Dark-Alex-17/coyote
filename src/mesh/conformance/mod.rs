//! Conformance suite for the mesh wire protocol.
//!
//! The halves:
//!
//! - `vectors`: data-driven cases that feed valid, boundary and invalid inputs to this crate's
//!   own codecs (announce, propagation node announce, card, message body, knock body, canonical
//!   text) and assert the receiver action expected for each row. Rust only, every
//!   platform.
//! - `env_vectors`: the same for the request layer: frames, the Envelope, the dispatcher's
//!   stages, refusal and version-refusal decoding. Rust only, every platform.
//! - `link_vectors`: what only shows on a live link (size branches, the inbound caps,
//!   handler slots and timeouts, response correlation, version marks), run over the loopback
//!   fixtures of `request::tests::network`. The table is declared everywhere so its rows are
//!   listed on every platform; the executor is `#[cfg(unix)]` with the fixtures it drives.
//! - `share_vectors`: the wire-path grammar and the decoders that reuse it, the share set and
//!   the grant store on a share root built per row, the `/list` and `/fetch` handlers driven
//!   in-process over such a root, the requester's page and reply readers, the versioning
//!   of the stores they read (`WirePath`, `ShareSet`, `GrantStore`, `StoreSchema`,
//!   `ListServe`, `FetchServe`, `FetchClient`). Rust only, every platform.
//! - `access_vectors`: the `/access` request and reply tables, the rate rule over a bare
//!   `MeshSlot`, the LXMF carriage and its routing, the human's decision reply and the
//!   requester's correlation of it, and the envoy's outcome wording (`AccessRequest`,
//!   `AccessReply`, `AccessLxmf`, `Decision`, `Disposition`). Rust only, every
//!   platform.
//! - `live_vectors`: the file-sharing surface as two nodes see it: the list, access, decision
//!   and fetch cycle, access admission, the requester's reading of fetch replies, symlinks
//!   under the share root and the grant a reference attachment lends, run over the loopback
//!   node pair of `request::tests::network`. The table is declared everywhere so its rows are
//!   listed on every platform; the executor is `#[cfg(unix)]` with the fixtures it drives.
//! - `crate::repl::mesh_share_vectors`: the REPL's attachment predicate over a share root
//!   built per row (`Attachment`). Defined beside the predicate because its rows build the
//!   request context the mesh module may not name; listed here so the registry gathers it
//!   with the rest. Rust only, every platform.
//! - `interop`: the protocol exercised against the pinned Python Reticulum/LXMF reference,
//!   spawned as a subprocess. Those tests are `#[ignore]`d and gated on `COYOTE_MESH_INTEROP=1`;
//!   `scripts/mesh-interop/setup.sh` prepares the reference and prints the environment they need.
//! - `netns`: two of this crate's nodes in separate Linux network namespaces reaching each
//!   other through an external `rnsd` relay, the two-host topology on one machine. Also
//!   `#[ignore]`d and gated on `COYOTE_MESH_INTEROP=1`; `scripts/mesh-netns/setup.sh` (root)
//!   creates the namespaces.
//!
//! Every table row carries a descriptive name, which the failure messages print.
//!
//! Platform note: `interop`, `link_vectors::loopback` and `live_vectors::loopback` are
//! `#[cfg(unix)]` because the Python reference harness and the loopback fixtures they drive are
//! unix-only, not because the mesh is; `netns` is `#[cfg(target_os = "linux")]` because network
//! namespaces are a Linux kernel feature. `live_vectors::loopback` also holds the symlink rows,
//! whose fixtures create symlinks, and one `share_vectors` row branches inline on unix
//! permission bits; every vector table and `listed()` stays ungated, so the same rows are
//! listed on every platform. Product code under `src/mesh/` is never cfg-gated; those test
//! modules and sub-probes are the only exemption. Windows mesh behaviour is covered by the unit
//! tests, the platform-independent vectors and manual verification, not by the Python interop
//! matrix.

mod access_vectors;
mod env_vectors;
mod link_vectors;
mod live_vectors;
mod share_vectors;
mod vectors;

#[cfg(unix)]
mod interop;
#[cfg(target_os = "linux")]
mod netns;

/// Which side of a requirement a vector probes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A conforming input; the receiver accepts it and the decoded fields are checked.
    Valid,
    /// An input at a cap or an edge the spec fixes (exact length, last allowed value).
    Boundary,
    /// An input the spec names as malformed; the mandated refusal or silence is checked.
    Invalid,
}

/// One row of a module's vector table, as `all_listed` gathers it.
pub(crate) struct Listed {
    pub name: &'static str,
    pub kind: Kind,
    pub family: &'static str,
}

fn all_listed() -> Vec<Listed> {
    let mut listed = vectors::listed();
    listed.extend(env_vectors::listed());
    listed.extend(link_vectors::listed());
    listed.extend(share_vectors::listed());
    listed.extend(access_vectors::listed());
    listed.extend(live_vectors::listed());
    listed.extend(crate::repl::mesh_share_vectors::listed());
    listed
}

/// Each fixed-data row's wire payload, named `<table>/<row>`: the input the row hands the
/// code under test or, for an encode row, the output that code must produce.
pub(super) fn golden_inputs() -> Vec<(String, Vec<u8>)> {
    [
        ("vectors", vectors::golden_inputs()),
        ("env_vectors", env_vectors::golden_inputs()),
        ("share_vectors", share_vectors::golden_inputs()),
        ("access_vectors", access_vectors::golden_inputs()),
    ]
    .into_iter()
    .flat_map(|(table, rows)| {
        rows.into_iter()
            .map(move |(row, bytes)| (format!("{table}/{row}"), bytes))
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::{Kind, all_listed};
    use crate::mesh::request::{
        ACCESS_PATH, FETCH_PATH, KNOCK_PATH, LIST_PATH, MESSAGE_PATH, STATUS_PATH,
    };

    use std::path::Path;

    #[test]
    fn every_listed_row_has_a_name_a_kind_and_a_family() {
        let listed = all_listed();
        assert!(!listed.is_empty(), "no vectors are listed");
        assert!(
            listed.iter().any(|row| row.kind == Kind::Invalid),
            "no Invalid vector is listed"
        );
        if let Some(row) = listed.iter().find(|row| row.family.is_empty()) {
            panic!("a vector row has an empty family: {}", row.name);
        }
        if let Some(row) = listed.iter().find(|row| row.name.is_empty()) {
            panic!("a {} vector row has an empty name", row.family);
        }
    }

    /// The attachment family is defined beside the REPL predicate it drives; this runs it
    /// with the rest of the conformance slice so `cargo test mesh::conformance` covers every
    /// listed family.
    #[test]
    #[serial_test::serial]
    fn the_attachment_family_runs_with_the_conformance_slice() {
        crate::repl::mesh_share_vectors::run_all();
    }

    // The reference peer's served paths and the harness README's account of them, checked
    // on the files alone so the check runs on every platform, not only where the Python
    // reference can be spawned.
    const HARNESS_README: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/scripts/mesh-interop/README.md"
    ));
    const REFERENCE_PEER: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/scripts/mesh-interop/reference_peer.py"
    ));

    #[test]
    fn the_reference_peer_serves_exactly_the_paths_the_readme_says_it_does() {
        let gap = format!(
            "The reference peer does not implement `{KNOCK_PATH}`, `{LIST_PATH}`, `{FETCH_PATH}` or `{ACCESS_PATH}`;"
        );
        let unwrapped = HARNESS_README
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            unwrapped.contains(&gap),
            "scripts/mesh-interop/README.md no longer says {gap:?}"
        );
        // The README must also say what does cover those paths: the Rust-only vector
        // modules, each of which must exist and list rows.
        let covered_by = "the file-sharing paths are covered by Rust-only in-process conformance vectors in `src/mesh/conformance/share_vectors.rs`, `access_vectors.rs` and `live_vectors.rs`";
        assert!(
            unwrapped.contains(covered_by),
            "scripts/mesh-interop/README.md no longer says {covered_by:?}"
        );
        for (module, listed) in [
            ("share_vectors.rs", super::share_vectors::listed().len()),
            ("access_vectors.rs", super::access_vectors::listed().len()),
            ("live_vectors.rs", super::live_vectors::listed().len()),
        ] {
            assert!(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("src/mesh/conformance")
                    .join(module)
                    .is_file()
                    && listed > 0,
                "the README names {module} as a vector module, which must exist and list rows"
            );
        }

        let mut lines = REFERENCE_PEER.lines();
        let mut registered = Vec::new();
        while let Some(line) = lines.next() {
            if !line.contains(".register_request_handler(") {
                continue;
            }
            let args = lines
                .next()
                .unwrap_or_else(|| panic!("{line:?} has no argument line"));
            let (_, after_quote) = args
                .split_once('"')
                .unwrap_or_else(|| panic!("{args:?} does not open a path literal"));
            let (path, _) = after_quote
                .split_once('"')
                .unwrap_or_else(|| panic!("{args:?} does not close its path literal"));
            registered.push(path);
        }
        assert_eq!(
            registered,
            [STATUS_PATH, MESSAGE_PATH],
            "scripts/mesh-interop/reference_peer.py registers a different set of request paths"
        );
        for path in registered {
            assert!(
                unwrapped.contains(&format!("`{path}`")),
                "scripts/mesh-interop/README.md does not name the served path `{path}`"
            );
        }
    }
}
