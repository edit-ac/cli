#![allow(dead_code)]
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};
pub struct Fixture {
    pub root: PathBuf,
    pub path: PathBuf,
    pub payload: PathBuf,
    pub recovery: PathBuf,
}
impl Fixture {
    pub fn new(extension: &str, source: &str, edited: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "editac-local-workflow-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let path = root.join(format!("document.{extension}"));
        let payload = root.join("payload");
        fs::write(&path, source).unwrap();
        fs::write(&payload, edited).unwrap();
        let recovery = root.join("recovery");
        Self {
            root,
            path,
            payload,
            recovery,
        }
    }
    pub fn command(&self, mode: &str) -> String {
        let script =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/workflows/editor.sh");
        format!(
            "/bin/sh '{}' '{mode}' '{}' '{}'",
            script.display(),
            self.payload.display(),
            self.path.display()
        )
    }
    pub fn edit(&self, mode: &str, flags: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_editac"))
            .env_clear()
            .args(["edit"])
            .arg(&self.path)
            .args(["--editor", &self.command(mode), "--recovery-dir"])
            .arg(&self.recovery)
            .args(flags)
            .output()
            .unwrap()
    }
    pub fn session(&self) -> PathBuf {
        fs::read_dir(&self.recovery)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
    }
    pub fn recover(&self, output: Option<&std::path::Path>) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_editac"));
        c.env_clear().arg("recover").arg(self.session());
        if let Some(path) = output {
            c.arg("--output").arg(path);
        }
        c.output().unwrap()
    }
    pub fn source(&self) -> String {
        fs::read_to_string(&self.path).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
pub fn json(o: &Output) -> serde_json::Value {
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    serde_json::from_slice(&o.stdout).unwrap()
}
