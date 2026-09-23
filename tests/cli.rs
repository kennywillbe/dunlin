//! CLI tests: `hash-password` and `check-config`.

use std::io::Write;
use std::process::{Command, Stdio};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_dunlin"))
}

fn run_with_stdin(args: &[&str], stdin: &str) -> std::process::Output {
    let mut child = bin()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn hash_password_round_trip() {
    let out = run_with_stdin(&["hash-password"], "correcthorsebattery\n");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let hash = String::from_utf8(out.stdout).unwrap().trim().to_string();
    assert!(hash.starts_with("$argon2"));
    assert!(dunlin::auth::verify_password("correcthorsebattery", &hash).unwrap());
    assert!(!dunlin::auth::verify_password("nope", &hash).unwrap());
}

#[test]
fn hash_password_enforces_min_length() {
    let out = run_with_stdin(&["hash-password"], "short\n");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("12"), "{err}");
}

#[test]
fn check_config_accepts_valid_and_rejects_invalid() {
    let hash = dunlin::auth::hash_password("correcthorsebattery").unwrap();
    let dir = tempfile::tempdir().unwrap();

    let good = dir.path().join("good.toml");
    std::fs::write(
        &good,
        format!(
            "listen = \"127.0.0.1:8080\"\n[web]\npassword_hash = \"{hash}\"\n[systemd]\nenabled = false\n"
        ),
    )
    .unwrap();
    let out = bin()
        .args(["check-config", "--config"])
        .arg(&good)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let bad = dir.path().join("bad.toml");
    std::fs::write(&bad, "listen = \"not-an-address\"\n[web]\n").unwrap();
    let out = bin()
        .args(["check-config", "--config"])
        .arg(&bad)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("password_hash"), "{err}");
}
