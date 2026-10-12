//! Runs the built andris binary and checks what it prints.

// Test helpers, where a failed unwrap is a failed test. clippy's
// allow-unwrap-in-tests covers only #[test] functions themselves.
#![allow(clippy::unwrap_used)]

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

/// A ticket file of `contents`, mode 0600.
fn ticket_file(contents: &[u8]) -> NamedTempFile {
    let mut ticket = NamedTempFile::new().unwrap();
    ticket.write_all(contents).unwrap();
    std::fs::set_permissions(ticket.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
    ticket
}

/// Run andris on the synthetic platform with `ticket` and `extra`, and
/// return what it printed to stderr; it must have refused the
/// configuration, which is exit status 2.
fn refused(ticket: &NamedTempFile, extra: &[&str]) -> String {
    let path = ticket.path().to_str().unwrap();
    let mut args = vec!["--ticket-file", path, "--platform", "synthetic"];
    args.extend_from_slice(extra);
    let output = andris(&args);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        output.status.code(),
        Some(2),
        "andris {args:?} exited with {}: {stderr}",
        output.status
    );
    stderr
}

#[test]
fn refused_configuration_exits_with_status_two_and_says_why() {
    // A ticket file others can read is refused before anything starts.
    // Status 2, not 1, tells systemd not to restart andris over it.
    let mut ticket = NamedTempFile::new().unwrap();
    ticket.write_all(b"secret\n").unwrap();
    std::fs::set_permissions(ticket.path(), std::fs::Permissions::from_mode(0o644)).unwrap();

    let stderr = refused(&ticket, &[]);
    let error = stderr
        .lines()
        .find(|line| line.starts_with("Error: "))
        .unwrap_or_else(|| panic!("no Error: line in {stderr}"));
    assert!(error.contains("--ticket-file"), "{stderr}");
    assert!(stderr.contains("chmod 600"), "{stderr}");
}

#[test]
fn non_loopback_listener_without_tls_is_refused() {
    let ticket = ticket_file(b"a-long-enough-ticket\n");
    let stderr = refused(&ticket, &["--listen", "0.0.0.0:5900"]);
    assert!(stderr.contains("--listen 0.0.0.0:5900"), "{stderr}");
    assert!(stderr.contains("--tls-listen"), "{stderr}");
}

#[test]
fn empty_ticket_file_is_refused() {
    let ticket = ticket_file(b"");
    let stderr = refused(&ticket, &[]);
    assert!(stderr.contains("--ticket-file"), "{stderr}");
    assert!(stderr.contains("is empty"), "{stderr}");
}

#[test]
fn ticket_over_sixty_bytes_is_refused() {
    let ticket = ticket_file(&[b'a'; 61]);
    let stderr = refused(&ticket, &[]);
    assert!(stderr.contains("--ticket-file"), "{stderr}");
    assert!(stderr.contains("at most 60"), "{stderr}");
}
