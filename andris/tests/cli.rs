//! Runs the built andris binary and checks what it prints.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};

use tempfile::NamedTempFile;

fn andris(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_andris"))
        .args(args)
        .env_remove("DISPLAY")
        .env_remove("RUST_LOG")
        .output()
        .expect("andris could not be run")
}

#[test]
fn version_prints_banner_and_exits_successfully() {
    let output = andris(&["--version"]);
    assert!(
        output.status.success(),
        "andris exited with {}",
        output.status
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    let expected = format!("andris {}\n", env!("CARGO_PKG_VERSION"));
    assert_eq!(stdout, expected);
}

#[test]
fn ticket_file_is_required() {
    let output = andris(&[]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("--ticket-file"), "{stderr}");
}

#[test]
fn ticket_file_open_to_others_is_refused() {
    let mut ticket = NamedTempFile::new().unwrap();
    ticket.write_all(b"secret\n").unwrap();
    std::fs::set_permissions(ticket.path(), std::fs::Permissions::from_mode(0o644)).unwrap();

    let path = ticket.path().to_str().unwrap();
    let output = andris(&["--ticket-file", path, "--platform", "synthetic"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("--ticket-file"), "{stderr}");
    assert!(stderr.contains("chmod 600"), "{stderr}");
}
