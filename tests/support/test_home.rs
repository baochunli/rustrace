//! Fixture-owned application homes. Keep the guard alive while children run.
use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

pub struct TestHome {
    pub root: PathBuf,
}

impl TestHome {
    pub fn new(checks_enabled: bool) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "rustrace-test-home-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("state/rustrace")).unwrap();
        fs::create_dir_all(root.join("config")).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        fs::write(
            root.join("state/rustrace/update-state.json"),
            serde_json::to_vec(&rustrace::update::UpdateState {
                checks_enabled,
                ..rustrace::update::UpdateState::default()
            })
            .unwrap(),
        )
        .unwrap();
        Self { root }
    }

    pub fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        // Preserve installed tooling when changing HOME, without writing there.
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            for (key, fallback) in [("RUSTUP_HOME", ".rustup"), ("CARGO_HOME", ".cargo")] {
                command.env(
                    key,
                    std::env::var_os(key).unwrap_or_else(|| home.join(fallback).into_os_string()),
                );
            }
        }
        command
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"));
        command
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
