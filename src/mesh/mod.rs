#![deny(unsafe_code)]

mod announce;
mod identity;
mod lock;
mod node;
mod peers;
mod r3;

pub(crate) use node::MeshSlot;

use std::path::{Path, PathBuf};

/// Where mesh state that is safe to lose lives: instance locks and the peer table.
pub(crate) fn mesh_cache_dir(cache_dir: &Path) -> PathBuf {
    cache_dir.join("mesh")
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::node::MeshPaths;
    #[cfg(unix)]
    use super::node::{MeshRuntime, NodeOptions};
    use crate::config::MeshConfig;
    #[cfg(unix)]
    use crate::config::Session;
    use crate::config::mesh_config::MeshInterface;

    #[cfg(unix)]
    use std::net::SocketAddr;
    use std::path::PathBuf;
    #[cfg(unix)]
    use std::sync::Arc;
    #[cfg(unix)]
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use std::{env, fs};
    #[cfg(unix)]
    use tokio::io::AsyncReadExt;
    #[cfg(unix)]
    use tokio::net::TcpListener;
    #[cfg(unix)]
    use tokio::task::JoinHandle;

    pub(crate) struct TempDir {
        pub(crate) path: PathBuf,
    }

    impl TempDir {
        pub(crate) fn new(tag: &str) -> Self {
            // Wall-clock nanos alone are not unique across parallel tests; a
            // process-wide counter makes every name distinct.
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!("coyote-mesh-{tag}-{nanos}-{seq}"));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// A TCP listener that accepts every connection and holds it open until the peer hangs
    /// up, which is all a `TcpClient` needs to report itself connected. The counter is the
    /// number of accepted streams the peer has closed. Unix-only with everything below it:
    /// starting a runtime mints an owner-only identity file, which only unix implements.
    #[cfg(unix)]
    pub(crate) async fn loopback_relay() -> (SocketAddr, JoinHandle<()>, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let closed = Arc::new(AtomicUsize::new(0));
        let counter = closed.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let counter = counter.clone();
                tokio::spawn(async move {
                    let mut sink = [0u8; 1024];
                    while matches!(stream.read(&mut sink).await, Ok(n) if n > 0) {}
                    counter.fetch_add(1, Ordering::SeqCst);
                });
            }
        });
        (addr, handle, closed)
    }

    pub(crate) fn private_config(port: u16) -> MeshConfig {
        MeshConfig {
            interfaces: vec![MeshInterface::Private {
                host: "127.0.0.1".to_string(),
                port,
            }],
            ..MeshConfig::default()
        }
    }

    pub(crate) fn mesh_paths(tmp: &TempDir) -> MeshPaths {
        MeshPaths {
            identity_path: tmp.path.join("config").join("mesh").join("identity.key"),
            cache_dir: tmp.path.join("cache"),
        }
    }

    #[cfg(unix)]
    pub(crate) struct StartedRuntime {
        pub(crate) runtime: Arc<MeshRuntime>,
        pub(crate) session: Session,
        pub(crate) relay_handle: JoinHandle<()>,
        _tmp: TempDir,
    }

    /// A runtime joined to a loopback relay, with its identity and cache under a temp dir.
    #[cfg(unix)]
    pub(crate) async fn started_runtime(tag: &str) -> StartedRuntime {
        let (addr, relay_handle, _) = loopback_relay().await;
        let tmp = TempDir::new(tag);
        let mut session = Session::default();
        let runtime = MeshRuntime::start(
            &private_config(addr.port()),
            true,
            &mut session,
            mesh_paths(&tmp),
            NodeOptions::default(),
        )
        .await
        .unwrap();
        StartedRuntime {
            runtime,
            session,
            relay_handle,
            _tmp: tmp,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn mesh_cache_dir_is_mesh_under_cache_dir() {
        assert_eq!(
            mesh_cache_dir(Path::new("/tmp/cache")),
            PathBuf::from("/tmp/cache/mesh")
        );
    }

    #[test]
    fn mesh_module_never_names_the_request_ctx() {
        fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    rust_sources(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    out.push(path);
                }
            }
        }

        let mut sources = Vec::new();
        rust_sources(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src")
                .join("mesh"),
            &mut sources,
        );
        // Assembled at runtime so this test's own text does not match the probes.
        let needles = [
            ["Request", "Context"].concat(),
            ["request", "_context"].concat(),
        ];
        for path in &sources {
            let source = fs::read_to_string(path).unwrap();
            for needle in &needles {
                assert!(
                    !source.contains(needle),
                    "{} must not reference {needle}",
                    path.display()
                );
            }
        }
        assert!(
            sources.len() >= 5,
            "expected the mesh sources, found {}",
            sources.len()
        );
    }
}
