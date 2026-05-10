use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn run(cwd: &Path, line: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_winbash"))
        .arg("-c")
        .arg(line)
        .current_dir(cwd)
        .env("WINBASH_NO_RC", "1")
        .output()
        .expect("winbash should launch")
}

fn run_with_home(cwd: &Path, home: &Path, line: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_winbash"))
        .arg("-c")
        .arg(line)
        .current_dir(cwd)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("WINBASH_RC", home.join(".winbashrc"))
        .output()
        .expect("winbash should launch")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "expected success, got {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        stdout(output),
        stderr(output)
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout)
        .replace("\r\n", "\n")
        .trim_end_matches('\n')
        .to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .replace("\r\n", "\n")
        .trim_end_matches('\n')
        .to_string()
}

fn stdout_lines(output: &Output) -> Vec<String> {
    stdout(output).lines().map(str::to_string).collect()
}

#[test]
fn command_sequences_and_conditionals_are_end_to_end() {
    let temp = tempfile::tempdir().expect("tempdir should be created");

    let output = run(temp.path(), "echo one; echo two");
    assert_success(&output);
    assert_eq!(stdout_lines(&output), ["one", "two"]);

    let output = run(temp.path(), "true && echo yes");
    assert_success(&output);
    assert_eq!(stdout_lines(&output), ["yes"]);

    let output = run(temp.path(), "false && echo no || echo fallback");
    assert_success(&output);
    assert_eq!(stdout_lines(&output), ["fallback"]);
}

#[test]
fn exit_status_and_last_status_are_end_to_end() {
    let temp = tempfile::tempdir().expect("tempdir should be created");

    let output = run(temp.path(), "true && false");
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout:\n{}",
        stdout(&output)
    );

    let output = run(temp.path(), "false; echo $?");
    assert_success(&output);
    assert_eq!(stdout_lines(&output), ["1"]);
}

#[test]
fn linux_style_variables_are_end_to_end() {
    let temp = tempfile::tempdir().expect("tempdir should be created");

    let output = run(temp.path(), "FOO=bar; echo $FOO; echo ${FOO}; echo %FOO%");
    assert_success(&output);
    assert_eq!(stdout_lines(&output), ["bar", "bar", "%FOO%"]);

    let output = run(temp.path(), "FOO=bar env");
    assert_success(&output);
    assert!(
        stdout(&output).lines().any(|line| line == "FOO=bar"),
        "temporary env assignment missing from env output:\n{}",
        stdout(&output)
    );
}

#[test]
fn pipelines_are_end_to_end() {
    let temp = tempfile::tempdir().expect("tempdir should be created");

    let output = run(temp.path(), "echo hello | cat");
    assert_success(&output);
    assert_eq!(stdout_lines(&output), ["hello"]);

    let output = run(temp.path(), "echo hello | cat && echo ok");
    assert_success(&output);
    assert_eq!(stdout_lines(&output), ["hello", "ok"]);
}

#[test]
fn redirection_is_end_to_end() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    let out_path = temp.path().join("out.txt");

    let output = run(temp.path(), "echo first > out.txt");
    assert_success(&output);
    assert_eq!(stdout(&output), "");

    let output = run(temp.path(), "echo second >> out.txt");
    assert_success(&output);

    let content = fs::read_to_string(&out_path)
        .expect("redirected output should exist")
        .replace("\r\n", "\n");
    assert_eq!(content.lines().collect::<Vec<_>>(), ["first", "second"]);

    let output = run(temp.path(), "cat < out.txt");
    assert_success(&output);
    assert_eq!(stdout_lines(&output), ["first", "second"]);
}

#[test]
fn glob_expansion_is_end_to_end() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    fs::write(temp.path().join("alpha.rs"), "").expect("alpha fixture should be written");
    fs::write(temp.path().join("beta.txt"), "").expect("beta fixture should be written");

    let output = run(temp.path(), "ls *.rs");
    assert_success(&output);
    let output = stdout(&output);

    assert!(output.contains("alpha.rs"), "stdout:\n{output}");
    assert!(!output.contains("beta.txt"), "stdout:\n{output}");
}

#[test]
fn winbashrc_is_executed_at_startup() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    let home = temp.path().join("home");
    let work = temp.path().join("work");
    fs::create_dir(&home).expect("home should be created");
    fs::create_dir(&work).expect("work should be created");
    fs::write(
        home.join(".winbashrc"),
        "export WINBASH_RC_VALUE=from_rc\n\
         alias rc_echo='echo from_alias'\n",
    )
    .expect("rc file should be written");

    let output = run_with_home(&work, &home, "echo $WINBASH_RC_VALUE; rc_echo");
    assert_success(&output);
    assert_eq!(stdout_lines(&output), ["from_rc", "from_alias"]);
}

#[test]
fn builtin_redirection_and_pipelines_are_end_to_end() {
    let temp = tempfile::tempdir().expect("tempdir should be created");

    let output = run(temp.path(), "pwd > pwd.txt; cat pwd.txt");
    assert_success(&output);
    assert_eq!(
        stdout_lines(&output),
        [temp.path().to_string_lossy().replace('\\', "/")]
    );

    let output = run(temp.path(), "pwd | cat");
    assert_success(&output);
    assert_eq!(
        stdout_lines(&output),
        [temp.path().to_string_lossy().replace('\\', "/")]
    );

    let output = run(temp.path(), "cd definitely-missing 2> err.txt; echo $?");
    assert_success(&output);
    assert_eq!(stdout_lines(&output), ["1"]);
    let err =
        fs::read_to_string(temp.path().join("err.txt")).expect("stderr redirect should exist");
    assert!(err.contains("cd: not a directory"), "stderr file:\n{err}");
}

#[test]
fn source_builtin_is_end_to_end() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    fs::write(
        temp.path().join("script.wbash"),
        "export SOURCED_VALUE=from_source\n\
         echo from_script\n\
         alias sourced_echo='echo from_alias'\n",
    )
    .expect("source script should be written");

    let output = run(
        temp.path(),
        "source script.wbash; echo $SOURCED_VALUE; sourced_echo",
    );
    assert_success(&output);
    assert_eq!(
        stdout_lines(&output),
        ["from_script", "from_source", "from_alias"]
    );

    let output = run(
        temp.path(),
        "source script.wbash > sourced.txt; echo $SOURCED_VALUE; cat sourced.txt",
    );
    assert_success(&output);
    assert_eq!(stdout_lines(&output), ["from_source", "from_script"]);
}

#[test]
fn command_substitution_is_end_to_end() {
    let temp = tempfile::tempdir().expect("tempdir should be created");

    let output = run(temp.path(), "FOO=local; echo before $(echo $FOO) after");
    assert_success(&output);
    assert_eq!(stdout_lines(&output), ["before local after"]);

    let output = run(temp.path(), "VALUE=$(echo assigned); echo $VALUE");
    assert_success(&output);
    assert_eq!(stdout_lines(&output), ["assigned"]);
}

#[cfg(windows)]
#[test]
fn extensionless_command_uses_pathext_shim_from_path() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    let shim = temp.path().join("winbash-shim.cmd");
    let extensionless = temp.path().join("winbash-shim");
    fs::write(&shim, "@echo off\r\necho shim-ok\r\n").expect("cmd shim should be written");
    fs::write(&extensionless, "# shell shim for non-Windows hosts\n")
        .expect("extensionless shim should be written");

    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(temp.path().to_path_buf()).chain(std::env::split_paths(&original_path)),
    )
    .expect("PATH should be joinable");

    let output = Command::new(env!("CARGO_BIN_EXE_winbash"))
        .arg("-c")
        .arg("winbash-shim")
        .current_dir(temp.path())
        .env("WINBASH_NO_RC", "1")
        .env("PATH", path)
        .output()
        .expect("winbash should launch");

    assert_success(&output);
    assert_eq!(stdout_lines(&output), ["shim-ok"]);
}
