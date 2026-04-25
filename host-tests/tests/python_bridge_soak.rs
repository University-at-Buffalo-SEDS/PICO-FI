use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("host-tests crate must live under the repo root")
        .to_path_buf()
}

fn repo_python(repo_root: &Path) -> PathBuf {
    let venv_python = repo_root.join("venv").join("bin").join("python");
    if venv_python.is_file() {
        return venv_python;
    }
    PathBuf::from("python3")
}

fn run_python_unittest(repo_root: &Path, modules: &[&str]) {
    let python = repo_python(repo_root);
    let output = Command::new(&python)
        .current_dir(repo_root)
        .arg("-m")
        .arg("unittest")
        .args(modules)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {:?}: {err}", python));

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!(
            "python unittest failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status.code(),
            stdout,
            stderr
        );
    }
}

#[test]
fn host_bridge_framing_tests_pass() {
    let repo_root = repo_root();
    run_python_unittest(
        &repo_root,
        &[
            "host.python.test_sedsprintf_router_common",
            "host.python.test_telemetry_cli",
            "host.python.test_bridge_framing",
            "host.python.test_transport_framing_roundtrip",
        ],
    );
}

#[test]
fn host_bridge_transport_soak_passes() {
    let repo_root = repo_root();
    run_python_unittest(&repo_root, &["host.python.test_bridge_transport_soak"]);
}
