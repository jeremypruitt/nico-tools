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

#[test]
fn ops_subcommand_exits_three_with_not_yet_notice() {
    Command::cargo_bin("nico")
        .unwrap()
        .arg("ops")
        .assert()
        .code(3)
        .stderr(predicate::str::contains("not yet"));
}

#[test]
fn no_subcommand_defaults_to_ops() {
    Command::cargo_bin("nico")
        .unwrap()
        .assert()
        .code(3)
        .stderr(predicate::str::contains("not yet"));
}
