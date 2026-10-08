//! Runs the built andris binary and checks what it prints.

use std::process::Command;

#[test]
fn prints_banner_and_exits_successfully() {
    let output = Command::new(env!("CARGO_BIN_EXE_andris")).output().unwrap();
    assert!(
        output.status.success(),
        "andris exited with {}",
        output.status
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    let expected = format!("andris {}\n", env!("CARGO_PKG_VERSION"));
    assert_eq!(stdout, expected);
    assert!(stdout.starts_with("andris "));
    assert_eq!(
        stdout.lines().count(),
        1,
        "banner is not one line: {stdout:?}"
    );
}
