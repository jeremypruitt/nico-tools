use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn help_lists_ops_doctor_correlate_subcommands() {
    Command::cargo_bin("nico")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("ops"))
        .stdout(predicate::str::contains("doctor"))
        .stdout(predicate::str::contains("correlate"));
}

// `nico ops` requires an interactive terminal (ADR-010); the test harness
// pipes stdout, so we expect exit code 3 and the TTY-guard message.
#[test]
fn ops_subcommand_exits_three_when_stdout_not_tty() {
    Command::cargo_bin("nico")
        .unwrap()
        .arg("ops")
        .assert()
        .code(3)
        .stderr(predicate::str::contains("interactive terminal"));
}

#[test]
fn no_subcommand_defaults_to_ops() {
    Command::cargo_bin("nico")
        .unwrap()
        .assert()
        .code(3)
        .stderr(predicate::str::contains("interactive terminal"));
}
