//! CLI tests: `hash-password`, `hash-token` and `check-config`.

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

#[test]
fn check_config_after_the_config_flag() {
    // Used to start the server instead: the first argument was a flag, so
    // the command was never looked at.
    let hash = dunlin::auth::hash_password("correcthorsebattery").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("good.toml");
    std::fs::write(
        &path,
        format!(
            "listen = \"127.0.0.1:8080\"\n[web]\npassword_hash = \"{hash}\"\n[systemd]\nenabled = false\n"
        ),
    )
    .unwrap();
    let out = bin()
        .arg("--config")
        .arg(&path)
        .arg("check-config")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("is valid"));
}

#[test]
fn no_arguments_runs_the_server_with_dunlin_toml() {
    // In an empty directory the server fails to find its default config,
    // which shows it tried to start rather than printing the help.
    let dir = tempfile::tempdir().unwrap();
    let out = bin().current_dir(dir.path()).output().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("dunlin.toml"), "{err}");
}

#[test]
fn hash_token_prints_a_token_and_its_hash() {
    let out = bin().arg("hash-token").output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "{stdout}");
    let token = lines[0].strip_prefix("token: ").expect(&stdout);
    let hash = lines[1].strip_prefix("hash:  ").expect(&stdout);
    let is_hex64 = |s: &str| {
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    };
    assert!(is_hex64(token), "{token}");
    assert!(is_hex64(hash), "{hash}");
    use sha2::Digest;
    assert_eq!(hex::encode(sha2::Sha256::digest(token.as_bytes())), hash);
    assert!(String::from_utf8_lossy(&out.stderr).contains("[[api_keys]]"));

    // A fresh token every time.
    let again = bin().arg("hash-token").output().unwrap();
    assert_ne!(String::from_utf8(again.stdout).unwrap(), stdout);
}
