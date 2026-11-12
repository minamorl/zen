use std::process::{Command, Stdio};

#[test]
fn zen_help_shows_every_operation_without_stdin() {
    for argument in ["--help", "help"] {
        let output = Command::new(env!("CARGO_BIN_EXE_zen"))
            .arg(argument)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success(), "{argument}: {output:?}");
        assert!(output.stderr.is_empty(), "{argument}: {output:?}");
        let help = String::from_utf8(output.stdout).unwrap();
        for operation in ["apply", "compose", "marks", "merge"] {
            assert!(help.contains(&format!("zen {operation}")), "{help}");
        }
        for shape in [
            "{\"text\": string}",
            "{\"edit\": edit}",
            "{\"marks\": marks}",
            "{\"segments\": segments, \"conflicts\": integer}",
        ] {
            assert!(help.contains(shape), "{help}");
        }
    }
}

#[test]
fn unknown_operation_is_rejected_for_valid_input() {
    use std::io::Write;

    let mut child = Command::new(env!("CARGO_BIN_EXE_zen"))
        .arg("unknown")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"{}").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("\"code\":\"unknown_operation\""), "{error}");
}

#[test]
fn malformed_input_takes_priority_over_unknown_operation() {
    use std::io::Write;

    let mut child = Command::new(env!("CARGO_BIN_EXE_zen"))
        .arg("unknown")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"{").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("\"code\":\"invalid_json\""), "{error}");
}

#[test]
fn merge_driver_help_succeeds_without_merge_paths() {
    let output = Command::new(env!("CARGO_BIN_EXE_zen-git-merge"))
        .arg("--help")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("zen-git-merge %O %A %B %L %P"), "{help}");
}
