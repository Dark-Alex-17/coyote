// Scaffolding for the mesh runtime: the first consumer of this module removes the allow.
#![allow(dead_code)]
#![deny(unsafe_code)]

mod identity;
mod lock;

#[cfg(test)]
mod test_support {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use std::{env, fs};

    pub(super) struct TempDir {
        pub(super) path: PathBuf,
    }

    impl TempDir {
        pub(super) fn new(tag: &str) -> Self {
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
}
