use std::collections::{BTreeSet, HashMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use glob::{MatchOptions, glob_with};
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{Config, Context, Editor, Helper};

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler};
#[cfg(windows)]
use windows_sys::core::BOOL;

type Aliases = Arc<RwLock<HashMap<String, String>>>;
type ShellVars = HashMap<String, ShellVar>;

const BUILTINS: &[&str] = &[
    ".", "alias", "cd", "exit", "export", "help", "pwd", "source", "unalias", "unset",
];

const GIT_PROMPT_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const INTERRUPT_MONITOR_INTERVAL: Duration = Duration::from_millis(50);
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
#[cfg(windows)]
const WINBOOL_FALSE: BOOL = 0;
#[cfg(windows)]
const WINBOOL_TRUE: BOOL = 1;

type ForegroundJobs = Arc<Mutex<HashSet<u32>>>;

static INTERRUPT_STATE: OnceLock<InterruptState> = OnceLock::new();

const UUTILS: &[&str] = &[
    "arch",
    "b2sum",
    "base32",
    "base64",
    "basename",
    "basenc",
    "cat",
    "cksum",
    "comm",
    "cp",
    "csplit",
    "cut",
    "date",
    "dd",
    "df",
    "dir",
    "dirname",
    "du",
    "echo",
    "env",
    "expand",
    "expr",
    "factor",
    "false",
    "fmt",
    "fold",
    "head",
    "hostname",
    "join",
    "link",
    "ln",
    "logname",
    "ls",
    "md5sum",
    "mkdir",
    "mktemp",
    "more",
    "mv",
    "nl",
    "nohup",
    "nproc",
    "numfmt",
    "od",
    "paste",
    "pathchk",
    "printenv",
    "printf",
    "pwd",
    "readlink",
    "realpath",
    "rm",
    "rmdir",
    "seq",
    "sha1sum",
    "sha224sum",
    "sha256sum",
    "sha384sum",
    "sha512sum",
    "shred",
    "shuf",
    "sleep",
    "sort",
    "split",
    "stat",
    "stdbuf",
    "stty",
    "sum",
    "sync",
    "tac",
    "tail",
    "tee",
    "test",
    "timeout",
    "touch",
    "tr",
    "true",
    "truncate",
    "tsort",
    "tty",
    "uname",
    "unexpand",
    "uniq",
    "unlink",
    "uptime",
    "users",
    "vdir",
    "wc",
    "who",
    "whoami",
    "yes",
];

#[derive(Debug, Clone, Copy)]
enum LineResult {
    Continue(i32),
    Exit(i32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    Pipe,
    Sequence,
    AndIf,
    OrIf,
    RedirectIn,
    RedirectOut { append: bool },
    RedirectErr { append: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandListItem {
    connector: Option<Connector>,
    source: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Connector {
    Sequence,
    And,
    Or,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pipeline {
    commands: Vec<CommandSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct CommandSpec {
    assignments: Vec<Assignment>,
    args: Vec<String>,
    stdin: Option<PathBuf>,
    stdout: Option<Redirect>,
    stderr: Option<Redirect>,
}

impl CommandSpec {
    fn has_redirects(&self) -> bool {
        self.stdin.is_some() || self.stdout.is_some() || self.stderr.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Redirect {
    path: PathBuf,
    append: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Assignment {
    name: String,
    value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellVar {
    value: String,
    exported: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RedirectKind {
    Stdin,
    Stdout { append: bool },
    Stderr { append: bool },
}

struct Shell {
    aliases: Aliases,
    vars: ShellVars,
    previous_dir: Option<PathBuf>,
    last_status: i32,
    git_prompt_cache: GitPromptCache,
    interrupts: InterruptState,
}

#[derive(Clone)]
struct ShellSnapshot {
    aliases: HashMap<String, String>,
    vars: ShellVars,
    previous_dir: Option<PathBuf>,
    last_status: i32,
    cwd: Option<PathBuf>,
}

#[derive(Clone)]
struct InterruptState {
    jobs: ForegroundJobs,
    interrupted: Arc<AtomicBool>,
}

struct ForegroundInterruptGuard {
    installed: bool,
}

impl Drop for ForegroundInterruptGuard {
    fn drop(&mut self) {
        if self.installed {
            uninstall_foreground_interrupt_handler();
        }
    }
}

impl Shell {
    fn new() -> Self {
        Self {
            aliases: Arc::new(RwLock::new(HashMap::new())),
            vars: shell_vars_from_process(),
            previous_dir: None,
            last_status: 0,
            git_prompt_cache: GitPromptCache::default(),
            interrupts: interrupt_state(),
        }
    }

    fn helper(&self) -> WinbashHelper {
        WinbashHelper {
            aliases: Arc::clone(&self.aliases),
            vars: self.vars.clone(),
        }
    }

    fn load_startup_files(&mut self) {
        if env::var_os("WINBASH_NO_RC").is_some() {
            return;
        }

        if let Some(path) = env::var_os("WINBASH_ZSHRC").map(PathBuf::from) {
            self.load_alias_file(&path);
        } else if let Some(home) = dirs::home_dir() {
            // Keep .zshrc import conservative; .winbashrc is the native startup script.
            self.load_alias_file(&home.join(".zshrc"));
        }

        if let Some(path) = env::var_os("WINBASH_RC").map(PathBuf::from) {
            self.load_shell_file(&path);
        } else if let Some(home) = dirs::home_dir() {
            self.load_shell_file(&home.join(".winbashrc"));
        }
    }

    fn load_alias_file(&mut self, path: &Path) {
        let Ok(contents) = fs::read_to_string(path) else {
            return;
        };

        for (line_no, raw_line) in contents.lines().enumerate() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') || !line.starts_with("alias ") {
                continue;
            }

            match parse_line(line).and_then(|args| self.define_aliases(&args[1..])) {
                Ok(()) => {}
                Err(error) => eprintln!(
                    "winbash: ignored {}:{}: {}",
                    path.display(),
                    line_no + 1,
                    error
                ),
            }
        }
    }

    fn load_shell_file(&mut self, path: &Path) {
        let Ok(contents) = fs::read_to_string(path) else {
            return;
        };

        for (line_no, raw_line) in contents.lines().enumerate() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            match self.run_line(line) {
                Ok(LineResult::Continue(_)) => {}
                Ok(LineResult::Exit(status)) => {
                    self.last_status = status;
                    break;
                }
                Err(error) => eprintln!(
                    "winbash: ignored {}:{}: {}",
                    path.display(),
                    line_no + 1,
                    error
                ),
            }
        }
    }

    fn run_interactive(&mut self) -> io::Result<i32> {
        let config = Config::builder().auto_add_history(true).build();
        let mut editor = Editor::<WinbashHelper, DefaultHistory>::with_config(config)
            .map_err(io::Error::other)?;
        editor.set_helper(Some(self.helper()));

        let history_path = history_path();
        if let Some(path) = &history_path {
            let _ = editor.load_history(path);
        }

        let mut last_status = 0;
        loop {
            let prompt = self.prompt();
            editor.set_helper(Some(self.helper()));
            match editor.readline(&prompt) {
                Ok(line) => match self.run_line(&line) {
                    Ok(LineResult::Continue(status)) => last_status = status,
                    Ok(LineResult::Exit(status)) => {
                        last_status = status;
                        break;
                    }
                    Err(error) => {
                        eprintln!("winbash: {error}");
                        last_status = 1;
                        self.last_status = 1;
                    }
                },
                Err(ReadlineError::Interrupted) => {
                    self.clear_interrupt();
                    println!("^C");
                    last_status = 130;
                    self.last_status = 130;
                }
                Err(ReadlineError::Eof) => {
                    println!();
                    break;
                }
                Err(error) => {
                    eprintln!("winbash: readline failed: {error}");
                    last_status = 1;
                    self.last_status = 1;
                    break;
                }
            }
        }

        if let Some(path) = &history_path {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = editor.save_history(path);
        }

        Ok(last_status)
    }

    fn prompt(&self) -> String {
        let cwd = prompt_cwd();
        let git = env::current_dir()
            .ok()
            .and_then(|cwd| self.git_prompt_cache.segment(&cwd))
            .unwrap_or_default();
        let status = if self.last_status == 0 {
            String::new()
        } else {
            format!(" !{}", self.last_status)
        };

        format!("{cwd}{git}{status} % ")
    }

    fn expand_command_substitutions(&mut self, line: &str) -> Result<String, String> {
        let mut expanded = String::new();
        let mut index = 0;
        let mut quote = None;

        while index < line.len() {
            let ch = line[index..]
                .chars()
                .next()
                .expect("index is inside string");

            match quote {
                Some('\'') => {
                    expanded.push(ch);
                    index += ch.len_utf8();
                    if ch == '\'' {
                        quote = None;
                    }
                }
                Some('"') => {
                    if ch == '"' {
                        quote = None;
                        expanded.push(ch);
                        index += ch.len_utf8();
                    } else if ch == '\\' {
                        copy_escaped_char(line, &mut index, &mut expanded);
                    } else if line[index..].starts_with("$(") {
                        let end = find_command_substitution_end(line, index + 2)?;
                        let command = &line[index + 2..end];
                        expanded.push_str(&self.capture_command_substitution(command)?);
                        index = end + 1;
                    } else {
                        expanded.push(ch);
                        index += ch.len_utf8();
                    }
                }
                Some(_) => unreachable!("only single and double quotes are used"),
                None => {
                    if ch == '\'' || ch == '"' {
                        quote = Some(ch);
                        expanded.push(ch);
                        index += ch.len_utf8();
                    } else if ch == '\\' {
                        copy_escaped_char(line, &mut index, &mut expanded);
                    } else if line[index..].starts_with("$(") {
                        let end = find_command_substitution_end(line, index + 2)?;
                        let command = &line[index + 2..end];
                        expanded.push_str(&self.capture_command_substitution(command)?);
                        index = end + 1;
                    } else {
                        expanded.push(ch);
                        index += ch.len_utf8();
                    }
                }
            }
        }

        if let Some(quote) = quote {
            return Err(format!("unterminated {quote} quote"));
        }

        Ok(expanded)
    }

    fn capture_command_substitution(&mut self, command: &str) -> Result<String, String> {
        let snapshot = self.snapshot();
        let result = self.run_line_capture_stdout(command);
        self.restore_snapshot(snapshot);

        let (_, output) = result?;
        let mut text = String::from_utf8_lossy(&output).replace("\r\n", "\n");
        while text.ends_with('\n') {
            text.pop();
        }

        Ok(text)
    }

    fn run_line(&mut self, line: &str) -> Result<LineResult, String> {
        let command_list = split_command_list(line)?;
        if command_list.is_empty() {
            return Ok(LineResult::Continue(0));
        }

        let mut status = self.last_status;
        for item in command_list {
            let should_execute = match item.connector {
                None | Some(Connector::Sequence) => true,
                Some(Connector::And) => status == 0,
                Some(Connector::Or) => status != 0,
            };

            if !should_execute {
                continue;
            }

            let source = self.expand_command_substitutions(&item.source)?;
            let mut pipeline = parse_pipeline(&source, &self.vars, self.last_status)?;
            if pipeline.commands.is_empty() {
                status = 0;
                self.last_status = status;
                continue;
            }

            self.expand_pipeline_aliases(&mut pipeline)?;
            match self.run_pipeline_or_builtin(&pipeline)? {
                LineResult::Continue(next_status) => {
                    status = next_status;
                    self.last_status = next_status;
                }
                LineResult::Exit(exit_status) => {
                    self.last_status = exit_status;
                    return Ok(LineResult::Exit(exit_status));
                }
            }
        }

        Ok(LineResult::Continue(status))
    }

    fn run_pipeline_or_builtin(&mut self, pipeline: &Pipeline) -> Result<LineResult, String> {
        let (result, _) = self.run_pipeline_internal(pipeline, false, true)?;
        Ok(result)
    }

    fn run_line_capture_stdout(&mut self, line: &str) -> Result<(i32, Vec<u8>), String> {
        let (result, output) = self.run_line_capture_stdout_with_exit(line, false)?;
        let status = match result {
            LineResult::Continue(status) | LineResult::Exit(status) => status,
        };

        Ok((status, output))
    }

    fn run_line_capture_stdout_with_exit(
        &mut self,
        line: &str,
        allow_exit: bool,
    ) -> Result<(LineResult, Vec<u8>), String> {
        let command_list = split_command_list(line)?;
        if command_list.is_empty() {
            return Ok((LineResult::Continue(0), Vec::new()));
        }

        let mut status = self.last_status;
        let mut output = Vec::new();
        for item in command_list {
            let should_execute = match item.connector {
                None | Some(Connector::Sequence) => true,
                Some(Connector::And) => status == 0,
                Some(Connector::Or) => status != 0,
            };

            if !should_execute {
                continue;
            }

            let source = self.expand_command_substitutions(&item.source)?;
            let mut pipeline = parse_pipeline(&source, &self.vars, self.last_status)?;
            if pipeline.commands.is_empty() {
                status = 0;
                self.last_status = status;
                continue;
            }

            self.expand_pipeline_aliases(&mut pipeline)?;
            let (result, mut captured) = self.run_pipeline_internal(&pipeline, true, allow_exit)?;
            output.append(&mut captured);
            status = match result {
                LineResult::Continue(status) | LineResult::Exit(status) => status,
            };
            self.last_status = status;
            if allow_exit && matches!(result, LineResult::Exit(_)) {
                return Ok((result, output));
            }
        }

        Ok((LineResult::Continue(status), output))
    }

    fn run_pipeline_internal(
        &mut self,
        pipeline: &Pipeline,
        capture_stdout: bool,
        allow_exit: bool,
    ) -> Result<(LineResult, Vec<u8>), String> {
        if pipeline.commands.is_empty() {
            return Ok((LineResult::Continue(0), Vec::new()));
        }

        if pipeline.commands.len() == 1
            && pipeline.commands[0].args.is_empty()
            && !pipeline.commands[0].assignments.is_empty()
        {
            validate_assignment_only_redirects(&pipeline.commands[0])?;
            self.set_assignment_vars(&pipeline.commands[0].assignments);
            self.last_status = 0;
            return Ok((LineResult::Continue(0), Vec::new()));
        }

        if pipeline.commands.len() == 1 && command_is_builtin(&pipeline.commands[0]) {
            return self.run_single_builtin(&pipeline.commands[0], capture_stdout, allow_exit);
        }

        if pipeline_contains_builtin(pipeline) {
            return self.run_materialized_pipeline_with_builtins(pipeline, capture_stdout);
        }

        let (status, output) = if capture_stdout {
            self.run_external_pipeline(pipeline, true)?
        } else if pipeline.commands.len() == 1 && !pipeline.commands[0].has_redirects() {
            (self.run_external(&pipeline.commands[0]), Vec::new())
        } else {
            (self.run_pipeline(pipeline)?, Vec::new())
        };
        Ok((LineResult::Continue(status), output))
    }

    fn expand_pipeline_aliases(&self, pipeline: &mut Pipeline) -> Result<(), String> {
        for command in &mut pipeline.commands {
            if command.args.is_empty() {
                continue;
            }
            command.args = self.expand_aliases(std::mem::take(&mut command.args))?;
            if command.args.is_empty() {
                return Err("alias expanded to an empty command".to_string());
            }
        }

        Ok(())
    }

    fn expand_aliases(&self, mut args: Vec<String>) -> Result<Vec<String>, String> {
        let mut seen = HashSet::new();

        for _ in 0..16 {
            let Some(command) = args.first() else {
                return Ok(args);
            };

            let replacement = {
                let aliases = self.aliases.read().expect("aliases lock poisoned");
                aliases.get(command).cloned()
            };

            let Some(replacement) = replacement else {
                return Ok(args);
            };

            if !seen.insert(command.clone()) {
                return Err(format!("recursive alias detected at '{command}'"));
            }

            let mut expanded = parse_line_expanding(&replacement, &self.vars, self.last_status)?;
            expanded.extend(args.into_iter().skip(1));
            args = expanded;
        }

        Err("alias expansion exceeded 16 steps".to_string())
    }

    fn run_single_builtin(
        &mut self,
        command_spec: &CommandSpec,
        capture_stdout: bool,
        allow_exit: bool,
    ) -> Result<(LineResult, Vec<u8>), String> {
        if !command_spec.assignments.is_empty() {
            return Err("assignment prefixes before builtins are not supported yet".to_string());
        }
        validate_builtin_stdin(command_spec)?;

        let mut stdout = output_writer_for(
            command_spec.stdout.as_ref(),
            StandardStream::Stdout,
            capture_stdout,
        )?;
        let mut stderr =
            output_writer_for(command_spec.stderr.as_ref(), StandardStream::Stderr, false)?;
        let mut result = self.run_builtin_command(&command_spec.args, &mut stdout, &mut stderr)?;
        stdout.flush().map_err(|error| error.to_string())?;
        stderr.flush().map_err(|error| error.to_string())?;

        if !allow_exit && let LineResult::Exit(status) = result {
            result = LineResult::Continue(status);
        }

        Ok((result, stdout.into_buffer()))
    }

    fn run_builtin_command(
        &mut self,
        args: &[String],
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> Result<LineResult, String> {
        match self.run_builtin(args, stdout, stderr) {
            Ok(Some(result)) => Ok(result),
            Ok(None) => Ok(LineResult::Continue(127)),
            Err(error) => {
                writeln!(stderr, "winbash: {error}").map_err(|error| error.to_string())?;
                Ok(LineResult::Continue(1))
            }
        }
    }

    fn run_builtin(
        &mut self,
        args: &[String],
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> Result<Option<LineResult>, String> {
        match args[0].as_str() {
            "alias" => {
                if args.len() == 1 {
                    self.print_aliases(stdout)?;
                } else {
                    self.define_aliases(&args[1..])?;
                }
                Ok(Some(LineResult::Continue(0)))
            }
            "cd" => {
                self.cd(args.get(1).map(String::as_str), stdout)?;
                Ok(Some(LineResult::Continue(0)))
            }
            "exit" => {
                let status = args
                    .get(1)
                    .map(|value| value.parse::<i32>())
                    .transpose()
                    .map_err(|_| "exit status must be an integer".to_string())?
                    .unwrap_or(0);
                Ok(Some(LineResult::Exit(status)))
            }
            "export" => {
                if args.len() == 1 {
                    self.print_exported_vars(stdout)?;
                } else {
                    self.export_vars(&args[1..])?;
                }
                Ok(Some(LineResult::Continue(0)))
            }
            "help" => {
                write_help(stdout)?;
                Ok(Some(LineResult::Continue(0)))
            }
            "pwd" => {
                writeln!(
                    stdout,
                    "{}",
                    to_shell_path(&env::current_dir().map_err(|error| error.to_string())?)
                )
                .map_err(|error| error.to_string())?;
                Ok(Some(LineResult::Continue(0)))
            }
            "source" | "." => self.source_builtin(args, stdout, stderr).map(Some),
            "unalias" => {
                self.remove_aliases(&args[1..])?;
                Ok(Some(LineResult::Continue(0)))
            }
            "unset" => {
                self.unset_vars(&args[1..])?;
                Ok(Some(LineResult::Continue(0)))
            }
            _ => Ok(None),
        }
    }

    fn set_assignment_vars(&mut self, assignments: &[Assignment]) {
        for assignment in assignments {
            let exported = self
                .vars
                .get(&assignment.name)
                .is_some_and(|var| var.exported);
            self.vars.insert(
                assignment.name.clone(),
                ShellVar {
                    value: assignment.value.clone(),
                    exported,
                },
            );
        }
    }

    fn export_vars(&mut self, specs: &[String]) -> Result<(), String> {
        for spec in specs {
            if let Some(assignment) = parse_assignment(spec) {
                self.vars.insert(
                    assignment.name,
                    ShellVar {
                        value: assignment.value,
                        exported: true,
                    },
                );
            } else {
                validate_var_name(spec)?;
                self.vars
                    .entry(spec.clone())
                    .and_modify(|var| var.exported = true)
                    .or_insert_with(|| ShellVar {
                        value: String::new(),
                        exported: true,
                    });
            }
        }

        Ok(())
    }

    fn unset_vars(&mut self, names: &[String]) -> Result<(), String> {
        if names.is_empty() {
            return Err("unset requires at least one name".to_string());
        }

        for name in names {
            validate_var_name(name)?;
            self.vars.remove(name);
        }

        Ok(())
    }

    fn print_exported_vars(&self, stdout: &mut dyn Write) -> Result<(), String> {
        let mut names: Vec<_> = self
            .vars
            .iter()
            .filter_map(|(name, var)| var.exported.then_some(name))
            .collect();
        names.sort();

        for name in names {
            let value = &self.vars[name].value;
            writeln!(stdout, "export {}='{}'", name, shell_quote_single(value))
                .map_err(|error| error.to_string())?;
        }

        Ok(())
    }

    fn define_aliases(&mut self, specs: &[String]) -> Result<(), String> {
        if specs.is_empty() {
            return Err("alias requires NAME=VALUE".to_string());
        }

        let mut aliases = self.aliases.write().expect("aliases lock poisoned");
        for spec in specs {
            let Some((name, value)) = spec.split_once('=') else {
                return Err(format!("alias '{spec}' must use NAME=VALUE"));
            };
            validate_alias_name(name)?;
            aliases.insert(name.to_string(), value.to_string());
        }

        Ok(())
    }

    fn remove_aliases(&mut self, names: &[String]) -> Result<(), String> {
        if names.is_empty() {
            return Err("unalias requires at least one name".to_string());
        }

        let mut aliases = self.aliases.write().expect("aliases lock poisoned");
        for name in names {
            aliases.remove(name);
        }

        Ok(())
    }

    fn print_aliases(&self, stdout: &mut dyn Write) -> Result<(), String> {
        let aliases = self.aliases.read().expect("aliases lock poisoned");
        let mut names: Vec<_> = aliases.keys().collect();
        names.sort();

        for name in names {
            writeln!(
                stdout,
                "alias {}='{}'",
                name,
                aliases[name].replace('\'', "'\\''")
            )
            .map_err(|error| error.to_string())?;
        }

        Ok(())
    }

    fn source_builtin(
        &mut self,
        args: &[String],
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> Result<LineResult, String> {
        let Some(path) = args.get(1) else {
            return Err("source requires a file path".to_string());
        };
        if args.len() > 2 {
            return Err("source accepts exactly one file path".to_string());
        }

        self.source_shell_file(&expand_home_path(path), stdout, stderr)
    }

    fn source_shell_file(
        &mut self,
        path: &Path,
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> Result<LineResult, String> {
        let contents = fs::read_to_string(path)
            .map_err(|error| format!("source: {}: {error}", path.display()))?;

        let mut status = self.last_status;
        for (line_no, raw_line) in contents.lines().enumerate() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            match self.run_line_capture_stdout_with_exit(line, true) {
                Ok((LineResult::Continue(next_status), output)) => {
                    stdout
                        .write_all(&output)
                        .map_err(|error| error.to_string())?;
                    status = next_status;
                    self.last_status = next_status;
                }
                Ok((LineResult::Exit(exit_status), output)) => {
                    stdout
                        .write_all(&output)
                        .map_err(|error| error.to_string())?;
                    self.last_status = exit_status;
                    return Ok(LineResult::Exit(exit_status));
                }
                Err(error) => {
                    writeln!(
                        stderr,
                        "winbash: {}:{}: {}",
                        path.display(),
                        line_no + 1,
                        error
                    )
                    .map_err(|error| error.to_string())?;
                    status = 1;
                    self.last_status = 1;
                }
            }
        }

        Ok(LineResult::Continue(status))
    }

    fn cd(&mut self, target: Option<&str>, stdout: &mut dyn Write) -> Result<(), String> {
        let current_dir = env::current_dir().map_err(|error| error.to_string())?;
        let destination = match target {
            None | Some("") | Some("~") => {
                dirs::home_dir().ok_or_else(|| "could not determine home directory".to_string())?
            }
            Some("-") => self
                .previous_dir
                .clone()
                .ok_or_else(|| "OLDPWD is not set".to_string())?,
            Some(path) => expand_home_path(path),
        };

        let destination = normalize_cd_target(&destination);
        if !destination.is_dir() {
            return Err(format!("cd: not a directory: {}", destination.display()));
        }

        env::set_current_dir(&destination)
            .map_err(|error| format!("cd: {}: {error}", destination.display()))?;
        self.previous_dir = Some(current_dir);
        self.vars.insert(
            "OLDPWD".to_string(),
            ShellVar {
                value: to_shell_path(
                    self.previous_dir
                        .as_ref()
                        .expect("previous_dir was just set"),
                ),
                exported: true,
            },
        );
        self.vars.insert(
            "PWD".to_string(),
            ShellVar {
                value: env::current_dir()
                    .map(|cwd| to_shell_path(&cwd))
                    .map_err(|error| error.to_string())?,
                exported: true,
            },
        );

        if target == Some("-") {
            writeln!(
                stdout,
                "{}",
                to_shell_path(&env::current_dir().map_err(|error| error.to_string())?)
            )
            .map_err(|error| error.to_string())?;
        }

        Ok(())
    }

    fn run_external(&self, command_spec: &CommandSpec) -> i32 {
        let Some((command_name, raw_args)) = command_spec.args.split_first() else {
            return 0;
        };

        let child_env = child_env_for(&self.vars, &command_spec.assignments);
        let expanded_args = expand_globs(raw_args);
        let mut command = command_for(command_name, &child_env);
        command
            .args(expanded_args)
            .env_clear()
            .envs(&child_env)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        self.clear_interrupt();
        let interrupt_guard = foreground_interrupt_guard();
        configure_foreground_child(&mut command, interrupt_guard.installed);
        match command.spawn() {
            Ok(mut child) => {
                self.register_child(&child);
                let result = child.wait();
                self.unregister_child_id(child.id());
                match result {
                    Ok(_) if self.take_interrupt() => 130,
                    Ok(status) => status.code().unwrap_or(1),
                    Err(error) => {
                        eprintln!("winbash: failed to wait for {command_name}: {error}");
                        126
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                write_shell_error(
                    command_spec.stderr.as_ref(),
                    &format!("command not found: {command_name}"),
                );
                127
            }
            Err(error) => {
                write_shell_error(
                    command_spec.stderr.as_ref(),
                    &format!("failed to run {command_name}: {error}"),
                );
                126
            }
        }
    }

    fn run_pipeline(&self, pipeline: &Pipeline) -> Result<i32, String> {
        validate_pipeline_stdio(pipeline)?;

        let mut children: Vec<(String, Child)> = Vec::new();
        let mut previous_stdout: Option<ChildStdout> = None;
        let last_index = pipeline.commands.len().saturating_sub(1);
        self.clear_interrupt();
        let interrupt_guard = foreground_interrupt_guard();

        for (index, command_spec) in pipeline.commands.iter().enumerate() {
            let Some((command_name, raw_args)) = command_spec.args.split_first() else {
                return Err("empty command in pipeline".to_string());
            };

            let child_env = child_env_for(&self.vars, &command_spec.assignments);
            let mut command = command_for(command_name, &child_env);
            command
                .args(expand_globs(raw_args))
                .env_clear()
                .envs(&child_env)
                .stderr(match &command_spec.stderr {
                    Some(redirect) => open_output_stdio(redirect)?,
                    None => Stdio::inherit(),
                });

            if let Some(stdin_path) = &command_spec.stdin {
                command.stdin(open_input_stdio(stdin_path)?);
            } else if let Some(stdout) = previous_stdout.take() {
                command.stdin(Stdio::from(stdout));
            } else {
                command.stdin(Stdio::inherit());
            }

            if let Some(redirect) = &command_spec.stdout {
                command.stdout(open_output_stdio(redirect)?);
            } else if index < last_index {
                command.stdout(Stdio::piped());
            } else {
                command.stdout(Stdio::inherit());
            }

            configure_foreground_child(&mut command, interrupt_guard.installed);
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    write_shell_error(
                        command_spec.stderr.as_ref(),
                        &format!("command not found: {command_name}"),
                    );
                    wait_for_children(&mut children);
                    return Ok(127);
                }
                Err(error) => {
                    wait_for_children(&mut children);
                    return Err(format!("failed to run {command_name}: {error}"));
                }
            };

            previous_stdout = if index < last_index {
                child.stdout.take()
            } else {
                None
            };
            self.register_child(&child);
            children.push((command_name.clone(), child));
        }

        let mut last_status = 0;
        for (index, (command_name, mut child)) in children.into_iter().enumerate() {
            let child_id = child.id();
            match child.wait() {
                Ok(status) if index == last_index => {
                    last_status = status.code().unwrap_or(1);
                }
                Ok(_) => {}
                Err(error) if index == last_index => {
                    return Err(format!("failed to wait for {command_name}: {error}"));
                }
                Err(error) => {
                    eprintln!("winbash: failed to wait for {command_name}: {error}");
                }
            }
            self.unregister_child_id(child_id);
        }

        if self.take_interrupt() {
            Ok(130)
        } else {
            Ok(last_status)
        }
    }

    fn run_external_pipeline(
        &self,
        pipeline: &Pipeline,
        capture_final_stdout: bool,
    ) -> Result<(i32, Vec<u8>), String> {
        validate_pipeline_stdio(pipeline)?;

        let mut children: Vec<(String, Child)> = Vec::new();
        let mut previous_stdout: Option<ChildStdout> = None;
        let last_index = pipeline.commands.len().saturating_sub(1);
        self.clear_interrupt();
        let interrupt_guard = foreground_interrupt_guard();

        for (index, command_spec) in pipeline.commands.iter().enumerate() {
            let Some((command_name, raw_args)) = command_spec.args.split_first() else {
                return Err("empty command in pipeline".to_string());
            };

            let child_env = child_env_for(&self.vars, &command_spec.assignments);
            let mut command = command_for(command_name, &child_env);
            command
                .args(expand_globs(raw_args))
                .env_clear()
                .envs(&child_env)
                .stderr(match &command_spec.stderr {
                    Some(redirect) => open_output_stdio(redirect)?,
                    None => Stdio::inherit(),
                });

            if let Some(stdin_path) = &command_spec.stdin {
                command.stdin(open_input_stdio(stdin_path)?);
            } else if let Some(stdout) = previous_stdout.take() {
                command.stdin(Stdio::from(stdout));
            } else {
                command.stdin(Stdio::inherit());
            }

            if let Some(redirect) = &command_spec.stdout {
                command.stdout(open_output_stdio(redirect)?);
            } else if index < last_index || capture_final_stdout {
                command.stdout(Stdio::piped());
            } else {
                command.stdout(Stdio::inherit());
            }

            configure_foreground_child(&mut command, interrupt_guard.installed);
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    write_shell_error(
                        command_spec.stderr.as_ref(),
                        &format!("command not found: {command_name}"),
                    );
                    wait_for_children(&mut children);
                    return Ok((127, Vec::new()));
                }
                Err(error) => {
                    wait_for_children(&mut children);
                    return Err(format!("failed to run {command_name}: {error}"));
                }
            };

            previous_stdout = if index < last_index {
                child.stdout.take()
            } else {
                None
            };
            self.register_child(&child);
            children.push((command_name.clone(), child));
        }

        let mut captured_stdout = Vec::new();
        let mut last_status = 0;

        if capture_final_stdout && let Some((command_name, child)) = children.pop() {
            let child_id = child.id();
            match child.wait_with_output() {
                Ok(output) => {
                    last_status = output.status.code().unwrap_or(1);
                    captured_stdout = output.stdout;
                }
                Err(error) => {
                    self.unregister_child_id(child_id);
                    return Err(format!("failed to wait for {command_name}: {error}"));
                }
            }
            self.unregister_child_id(child_id);
        }

        for (index, (command_name, mut child)) in children.into_iter().enumerate() {
            let child_id = child.id();
            match child.wait() {
                Ok(status) if !capture_final_stdout && index == last_index => {
                    last_status = status.code().unwrap_or(1);
                }
                Ok(_) => {}
                Err(error) if !capture_final_stdout && index == last_index => {
                    self.unregister_child_id(child_id);
                    return Err(format!("failed to wait for {command_name}: {error}"));
                }
                Err(error) => {
                    eprintln!("winbash: failed to wait for {command_name}: {error}");
                }
            }
            self.unregister_child_id(child_id);
        }

        if self.take_interrupt() {
            Ok((130, captured_stdout))
        } else {
            Ok((last_status, captured_stdout))
        }
    }

    fn run_materialized_pipeline_with_builtins(
        &mut self,
        pipeline: &Pipeline,
        capture_final_stdout: bool,
    ) -> Result<(LineResult, Vec<u8>), String> {
        validate_pipeline_stdio(pipeline)?;

        let mut previous_stdout: Option<Vec<u8>> = None;
        let mut last_result = LineResult::Continue(0);
        let mut captured_stdout = Vec::new();
        let last_index = pipeline.commands.len().saturating_sub(1);

        for (index, command_spec) in pipeline.commands.iter().enumerate() {
            let is_last = index == last_index;
            let input = if let Some(stdin_path) = &command_spec.stdin {
                Some(
                    fs::read(stdin_path)
                        .map_err(|error| format!("{}: {error}", stdin_path.display()))?,
                )
            } else {
                previous_stdout.take()
            };
            let capture_command_stdout =
                command_spec.stdout.is_none() && (!is_last || capture_final_stdout);

            if command_is_builtin(command_spec) {
                let snapshot = (!is_last || capture_final_stdout).then(|| self.snapshot());
                let (result, output) =
                    self.run_single_builtin(command_spec, capture_command_stdout, false)?;
                if let Some(snapshot) = snapshot {
                    self.restore_snapshot(snapshot);
                }
                if !is_last {
                    previous_stdout = Some(output);
                } else {
                    last_result = result;
                    captured_stdout = output;
                }
                drop(input);
                continue;
            }

            let (status, output) =
                self.run_external_materialized(command_spec, input, capture_command_stdout)?;
            if !is_last {
                previous_stdout = Some(output);
            } else {
                last_result = LineResult::Continue(status);
                captured_stdout = output;
            }
        }

        Ok((last_result, captured_stdout))
    }

    fn run_external_materialized(
        &self,
        command_spec: &CommandSpec,
        input: Option<Vec<u8>>,
        capture_stdout: bool,
    ) -> Result<(i32, Vec<u8>), String> {
        let Some((command_name, raw_args)) = command_spec.args.split_first() else {
            return Ok((0, Vec::new()));
        };

        let child_env = child_env_for(&self.vars, &command_spec.assignments);
        let mut command = command_for(command_name, &child_env);
        command
            .args(expand_globs(raw_args))
            .env_clear()
            .envs(&child_env)
            .stderr(match &command_spec.stderr {
                Some(redirect) => open_output_stdio(redirect)?,
                None => Stdio::inherit(),
            });

        if input.is_some() {
            command.stdin(Stdio::piped());
        } else {
            command.stdin(Stdio::inherit());
        }

        if let Some(redirect) = &command_spec.stdout {
            command.stdout(open_output_stdio(redirect)?);
        } else if capture_stdout {
            command.stdout(Stdio::piped());
        } else {
            command.stdout(Stdio::inherit());
        }

        self.clear_interrupt();
        let interrupt_guard = foreground_interrupt_guard();
        configure_foreground_child(&mut command, interrupt_guard.installed);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                write_shell_error(
                    command_spec.stderr.as_ref(),
                    &format!("command not found: {command_name}"),
                );
                return Ok((127, Vec::new()));
            }
            Err(error) => return Err(format!("failed to run {command_name}: {error}")),
        };

        self.register_child(&child);
        if let Some(input) = input
            && let Some(mut stdin) = child.stdin.take()
        {
            stdin.write_all(&input).map_err(|error| error.to_string())?;
        }

        let child_id = child.id();
        let result = if capture_stdout {
            child.wait_with_output().map(|output| {
                (
                    output.status.code().unwrap_or(1),
                    if command_spec.stdout.is_some() {
                        Vec::new()
                    } else {
                        output.stdout
                    },
                )
            })
        } else {
            child
                .wait()
                .map(|status| (status.code().unwrap_or(1), Vec::new()))
        };
        self.unregister_child_id(child_id);

        let result =
            result.map_err(|error| format!("failed to wait for {command_name}: {error}"))?;
        if self.take_interrupt() {
            Ok((130, result.1))
        } else {
            Ok(result)
        }
    }

    fn register_child(&self, child: &Child) {
        if let Ok(mut jobs) = self.interrupts.jobs.lock() {
            jobs.insert(child.id());
        }
    }

    fn unregister_child_id(&self, child_id: u32) {
        if let Ok(mut jobs) = self.interrupts.jobs.lock() {
            jobs.remove(&child_id);
        }
    }

    fn clear_interrupt(&self) {
        self.interrupts.interrupted.store(false, Ordering::SeqCst);
    }

    fn take_interrupt(&self) -> bool {
        self.interrupts.interrupted.swap(false, Ordering::SeqCst)
    }

    fn snapshot(&self) -> ShellSnapshot {
        ShellSnapshot {
            aliases: self.aliases.read().expect("aliases lock poisoned").clone(),
            vars: self.vars.clone(),
            previous_dir: self.previous_dir.clone(),
            last_status: self.last_status,
            cwd: env::current_dir().ok(),
        }
    }

    fn restore_snapshot(&mut self, snapshot: ShellSnapshot) {
        *self.aliases.write().expect("aliases lock poisoned") = snapshot.aliases;
        self.vars = snapshot.vars;
        self.previous_dir = snapshot.previous_dir;
        self.last_status = snapshot.last_status;
        if let Some(cwd) = snapshot.cwd {
            let _ = env::set_current_dir(cwd);
        }
    }
}

struct WinbashHelper {
    aliases: Aliases,
    vars: ShellVars,
}

impl Helper for WinbashHelper {}

impl Completer for WinbashHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let start = token_start(line, pos);
        let token = &line[start..pos];
        let context = completion_context(&line[..start]);

        let pairs = if token.starts_with('$') {
            complete_variables(token, &self.vars)
        } else if context.command_position && !is_pathish(token) {
            complete_commands(token, &self.aliases)
        } else if context.command_name.as_deref() == Some("cd") && context.arg_index <= 1 {
            complete_paths_with_mode(token, PathCompletionMode::DirectoriesOnly)
        } else {
            complete_paths(token)
        };

        Ok((start, pairs))
    }
}

impl Hinter for WinbashHelper {
    type Hint = String;

    fn hint(&self, _line: &str, _pos: usize, _ctx: &Context<'_>) -> Option<String> {
        None
    }
}

impl Highlighter for WinbashHelper {}

impl Validator for WinbashHelper {
    fn validate(&self, _ctx: &mut ValidationContext<'_>) -> rustyline::Result<ValidationResult> {
        Ok(ValidationResult::Valid(None))
    }
}

fn main() {
    let mut shell = Shell::new();
    shell.load_startup_files();

    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let status = match args.as_slice() {
        [flag, command] if flag == "-c" => {
            let line = command.to_string_lossy();
            match shell.run_line(&line) {
                Ok(LineResult::Continue(status)) | Ok(LineResult::Exit(status)) => status,
                Err(error) => {
                    eprintln!("winbash: {error}");
                    1
                }
            }
        }
        [] => match shell.run_interactive() {
            Ok(status) => status,
            Err(error) => {
                eprintln!("winbash: {error}");
                1
            }
        },
        _ => {
            eprintln!("usage: winbash [-c COMMAND]");
            2
        }
    };

    std::process::exit(status);
}

fn copy_escaped_char(line: &str, index: &mut usize, output: &mut String) {
    let ch = line[*index..]
        .chars()
        .next()
        .expect("index is inside string");
    output.push(ch);
    *index += ch.len_utf8();

    if *index < line.len() {
        let next = line[*index..]
            .chars()
            .next()
            .expect("index is inside string");
        output.push(next);
        *index += next.len_utf8();
    }
}

fn find_command_substitution_end(line: &str, start: usize) -> Result<usize, String> {
    let mut index = start;
    let mut depth = 1;
    let mut quote = None;

    while index < line.len() {
        let ch = line[index..]
            .chars()
            .next()
            .expect("index is inside string");

        match quote {
            Some('\'') => {
                index += ch.len_utf8();
                if ch == '\'' {
                    quote = None;
                }
            }
            Some('"') => {
                if ch == '"' {
                    quote = None;
                    index += ch.len_utf8();
                } else if ch == '\\' {
                    index += ch.len_utf8();
                    if index < line.len() {
                        let next = line[index..]
                            .chars()
                            .next()
                            .expect("index is inside string");
                        index += next.len_utf8();
                    }
                } else if line[index..].starts_with("$(") {
                    depth += 1;
                    index += 2;
                } else if ch == ')' && depth > 1 {
                    depth -= 1;
                    index += ch.len_utf8();
                } else {
                    index += ch.len_utf8();
                }
            }
            Some(_) => unreachable!("only single and double quotes are used"),
            None => {
                if ch == '\'' || ch == '"' {
                    quote = Some(ch);
                    index += ch.len_utf8();
                } else if ch == '\\' {
                    index += ch.len_utf8();
                    if index < line.len() {
                        let next = line[index..]
                            .chars()
                            .next()
                            .expect("index is inside string");
                        index += next.len_utf8();
                    }
                } else if line[index..].starts_with("$(") {
                    depth += 1;
                    index += 2;
                } else if ch == ')' {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(index);
                    }
                    index += ch.len_utf8();
                } else {
                    index += ch.len_utf8();
                }
            }
        }
    }

    Err("unterminated command substitution".to_string())
}

fn split_command_list(line: &str) -> Result<Vec<CommandListItem>, String> {
    let mut items = Vec::new();
    let mut segment_start = 0;
    let mut next_connector = None;
    let mut quote = None;
    let mut token_started = false;
    let mut substitution_depth = 0;
    let mut chars = line.char_indices().peekable();

    while let Some((index, ch)) = chars.next() {
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                }
            }
            Some('"') => {
                if ch == '"' {
                    quote = None;
                } else if ch == '\\' {
                    chars.next();
                }
            }
            Some(_) => unreachable!("only single and double quotes are used"),
            None => {
                if ch == '\'' || ch == '"' {
                    quote = Some(ch);
                    token_started = true;
                } else if ch == '\\' {
                    chars.next();
                    token_started = true;
                } else if ch == '$' && chars.peek().is_some_and(|(_, next)| *next == '(') {
                    chars.next();
                    substitution_depth += 1;
                    token_started = true;
                } else if substitution_depth > 0 {
                    if ch == ')' {
                        substitution_depth -= 1;
                    }
                    token_started = true;
                } else if ch == '#' && !token_started {
                    break;
                } else if ch.is_whitespace() {
                    token_started = false;
                } else if ch == ';' {
                    push_command_segment(
                        &mut items,
                        &mut next_connector,
                        &line[segment_start..index],
                    )?;
                    next_connector = Some(Connector::Sequence);
                    segment_start = index + ch.len_utf8();
                    token_started = false;
                } else if ch == '&' {
                    if chars.peek().is_some_and(|(_, next)| *next == '&') {
                        let (_, next) = chars.next().expect("peeked char exists");
                        push_command_segment(
                            &mut items,
                            &mut next_connector,
                            &line[segment_start..index],
                        )?;
                        next_connector = Some(Connector::And);
                        segment_start = index + ch.len_utf8() + next.len_utf8();
                        token_started = false;
                    } else {
                        return Err("unsupported '&'; use && for conditional execution".to_string());
                    }
                } else if ch == '|' {
                    if chars.peek().is_some_and(|(_, next)| *next == '|') {
                        let (_, next) = chars.next().expect("peeked char exists");
                        push_command_segment(
                            &mut items,
                            &mut next_connector,
                            &line[segment_start..index],
                        )?;
                        next_connector = Some(Connector::Or);
                        segment_start = index + ch.len_utf8() + next.len_utf8();
                        token_started = false;
                    } else {
                        token_started = true;
                    }
                } else {
                    token_started = true;
                }
            }
        }
    }

    if let Some(quote) = quote {
        return Err(format!("unterminated {quote} quote"));
    }
    if substitution_depth > 0 {
        return Err("unterminated command substitution".to_string());
    }

    let tail = line[segment_start..].trim();
    if tail.is_empty() {
        match next_connector {
            Some(Connector::And) => return Err("missing command after &&".to_string()),
            Some(Connector::Or) => return Err("missing command after ||".to_string()),
            Some(Connector::Sequence) | None => {}
        }
    } else {
        items.push(CommandListItem {
            connector: next_connector,
            source: tail.to_string(),
        });
    }

    Ok(items)
}

fn push_command_segment(
    items: &mut Vec<CommandListItem>,
    next_connector: &mut Option<Connector>,
    segment: &str,
) -> Result<(), String> {
    let source = segment.trim();
    if source.is_empty() {
        return Err("missing command before control operator".to_string());
    }

    items.push(CommandListItem {
        connector: *next_connector,
        source: source.to_string(),
    });
    *next_connector = None;
    Ok(())
}

fn parse_pipeline(line: &str, vars: &ShellVars, last_status: i32) -> Result<Pipeline, String> {
    let tokens = tokenize_line(line, vars, last_status, true)?;
    let mut commands = Vec::new();
    let mut current = CommandSpec::default();
    let mut pending_redirect = None;

    for token in tokens {
        match token {
            Token::Word(word) => {
                if let Some(kind) = pending_redirect.take() {
                    apply_redirect(&mut current, kind, word)?;
                } else {
                    current.args.push(word);
                }
            }
            Token::Pipe => {
                if let Some(kind) = pending_redirect {
                    return Err(format!("missing target for {}", redirect_label(kind)));
                }
                pending_redirect = None;

                split_leading_assignments(&mut current)?;
                if current.args.is_empty() {
                    return Err("missing command before pipe".to_string());
                }

                commands.push(current);
                current = CommandSpec::default();
            }
            Token::Sequence | Token::AndIf | Token::OrIf => {
                return Err("control operator is not valid inside a pipeline".to_string());
            }
            Token::RedirectIn => {
                set_pending_redirect(&mut pending_redirect, RedirectKind::Stdin)?;
            }
            Token::RedirectOut { append } => {
                set_pending_redirect(&mut pending_redirect, RedirectKind::Stdout { append })?;
            }
            Token::RedirectErr { append } => {
                set_pending_redirect(&mut pending_redirect, RedirectKind::Stderr { append })?;
            }
        }
    }

    if let Some(kind) = pending_redirect {
        return Err(format!("missing target for {}", redirect_label(kind)));
    }

    split_leading_assignments(&mut current)?;
    if current.args.is_empty() {
        if commands.is_empty() {
            if current.assignments.is_empty() {
                return Ok(Pipeline { commands });
            }
            commands.push(current);
            return Ok(Pipeline { commands });
        }
        return Err("missing command after pipe".to_string());
    }

    commands.push(current);
    Ok(Pipeline { commands })
}

fn apply_redirect(
    command: &mut CommandSpec,
    kind: RedirectKind,
    target: String,
) -> Result<(), String> {
    if target.is_empty() {
        return Err(format!("missing target for {}", redirect_label(kind)));
    }

    let redirect = Redirect {
        path: expand_home_path(&target),
        append: matches!(
            kind,
            RedirectKind::Stdout { append: true } | RedirectKind::Stderr { append: true }
        ),
    };

    match kind {
        RedirectKind::Stdin => command.stdin = Some(redirect.path),
        RedirectKind::Stdout { .. } => command.stdout = Some(redirect),
        RedirectKind::Stderr { .. } => command.stderr = Some(redirect),
    }

    Ok(())
}

fn split_leading_assignments(command: &mut CommandSpec) -> Result<(), String> {
    let assignment_count = command
        .args
        .iter()
        .take_while(|arg| is_assignment_word(arg))
        .count();

    if assignment_count == 0 {
        return Ok(());
    }

    let args = command.args.split_off(assignment_count);
    let assignment_words = std::mem::replace(&mut command.args, args);
    for word in assignment_words {
        let assignment =
            parse_assignment(&word).ok_or_else(|| format!("invalid assignment syntax: {word}"))?;
        command.assignments.push(assignment);
    }

    Ok(())
}

fn set_pending_redirect(
    pending_redirect: &mut Option<RedirectKind>,
    kind: RedirectKind,
) -> Result<(), String> {
    if let Some(previous) = pending_redirect {
        return Err(format!("missing target for {}", redirect_label(*previous)));
    }

    *pending_redirect = Some(kind);
    Ok(())
}

fn redirect_label(kind: RedirectKind) -> &'static str {
    match kind {
        RedirectKind::Stdin => "<",
        RedirectKind::Stdout { append: false } => ">",
        RedirectKind::Stdout { append: true } => ">>",
        RedirectKind::Stderr { append: false } => "2>",
        RedirectKind::Stderr { append: true } => "2>>",
    }
}

fn parse_line(line: &str) -> Result<Vec<String>, String> {
    let vars = ShellVars::new();
    tokenize_line(line, &vars, 0, false)?
        .into_iter()
        .map(token_to_word)
        .collect()
}

fn parse_line_expanding(
    line: &str,
    vars: &ShellVars,
    last_status: i32,
) -> Result<Vec<String>, String> {
    tokenize_line(line, vars, last_status, true)?
        .into_iter()
        .map(token_to_word)
        .collect()
}

fn token_to_word(token: Token) -> Result<String, String> {
    match token {
        Token::Word(word) => Ok(word),
        Token::Pipe => Err("pipe is not valid here".to_string()),
        Token::Sequence => Err("sequence operator is not valid here".to_string()),
        Token::AndIf => Err("&& is not valid here".to_string()),
        Token::OrIf => Err("|| is not valid here".to_string()),
        Token::RedirectIn => Err("input redirection is not valid here".to_string()),
        Token::RedirectOut { append: false } => {
            Err("output redirection is not valid here".to_string())
        }
        Token::RedirectOut { append: true } => {
            Err("append redirection is not valid here".to_string())
        }
        Token::RedirectErr { append: false } => {
            Err("stderr redirection is not valid here".to_string())
        }
        Token::RedirectErr { append: true } => {
            Err("stderr append redirection is not valid here".to_string())
        }
    }
}

fn tokenize_line(
    line: &str,
    vars: &ShellVars,
    last_status: i32,
    expand_vars: bool,
) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = line.chars().peekable();
    let mut token_started = false;
    let mut quote = None;

    while let Some(ch) = chars.next() {
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                } else {
                    current.push(ch);
                }
            }
            Some('"') => {
                if ch == '"' {
                    quote = None;
                } else if ch == '$' && expand_vars {
                    current.push_str(&expand_variable(&mut chars, vars, last_status)?);
                    token_started = true;
                } else if ch == '\\' {
                    if chars
                        .peek()
                        .is_some_and(|next| matches!(next, '"' | '$') || next.is_whitespace())
                    {
                        current.push(chars.next().expect("peeked char exists"));
                    } else {
                        current.push(ch);
                    }
                } else {
                    current.push(ch);
                }
            }
            Some(_) => unreachable!("only single and double quotes are used"),
            None => {
                if ch == '\'' || ch == '"' {
                    quote = Some(ch);
                    token_started = true;
                } else if ch.is_whitespace() {
                    if token_started {
                        tokens.push(Token::Word(current.clone()));
                        current.clear();
                        token_started = false;
                    }
                } else if ch == ';'
                    || ch == '&'
                    || ch == '|'
                    || ch == '<'
                    || ch == '>'
                    || (ch == '2' && !token_started && matches!(chars.peek(), Some('>')))
                {
                    if token_started {
                        tokens.push(Token::Word(current.clone()));
                        current.clear();
                        token_started = false;
                    }

                    match ch {
                        ';' => tokens.push(Token::Sequence),
                        '&' => {
                            if matches!(chars.peek(), Some('&')) {
                                chars.next();
                                tokens.push(Token::AndIf);
                            } else {
                                return Err(
                                    "unsupported '&'; use && for conditional execution".to_string()
                                );
                            }
                        }
                        '|' => {
                            if matches!(chars.peek(), Some('|')) {
                                chars.next();
                                tokens.push(Token::OrIf);
                            } else {
                                tokens.push(Token::Pipe);
                            }
                        }
                        '<' => tokens.push(Token::RedirectIn),
                        '>' => {
                            let append = matches!(chars.peek(), Some('>'));
                            if append {
                                chars.next();
                            }
                            tokens.push(Token::RedirectOut { append });
                        }
                        '2' => {
                            chars.next();
                            let append = matches!(chars.peek(), Some('>'));
                            if append {
                                chars.next();
                            }
                            tokens.push(Token::RedirectErr { append });
                        }
                        _ => unreachable!("operator branch only matches shell operators"),
                    }
                } else if ch == '\\' {
                    if chars.peek().is_some_and(|next| {
                        matches!(next, '\'' | '"' | '$' | ';' | '&' | '|' | '<' | '>')
                            || next.is_whitespace()
                    }) {
                        current.push(chars.next().expect("peeked char exists"));
                    } else {
                        current.push(ch);
                    }
                    token_started = true;
                } else if ch == '$' && expand_vars {
                    let expanded = expand_variable(&mut chars, vars, last_status)?;
                    if token_started || !expanded.is_empty() {
                        current.push_str(&expanded);
                        token_started = true;
                    }
                } else if ch == '#' && !token_started {
                    break;
                } else {
                    current.push(ch);
                    token_started = true;
                }
            }
        }
    }

    if let Some(quote) = quote {
        return Err(format!("unterminated {quote} quote"));
    }

    if token_started {
        tokens.push(Token::Word(current));
    }

    Ok(tokens)
}

fn expand_variable<I>(
    chars: &mut std::iter::Peekable<I>,
    vars: &ShellVars,
    last_status: i32,
) -> Result<String, String>
where
    I: Iterator<Item = char>,
{
    match chars.peek().copied() {
        Some('?') => {
            chars.next();
            Ok(last_status.to_string())
        }
        Some('{') => {
            chars.next();
            let mut name = String::new();
            for ch in chars.by_ref() {
                if ch == '}' {
                    validate_var_name(&name)?;
                    return Ok(var_value(vars, &name).unwrap_or_default());
                }
                name.push(ch);
            }
            Err("unterminated ${...} expansion".to_string())
        }
        Some(ch) if is_var_name_start(ch) => {
            let mut name = String::new();
            while let Some(ch) = chars.peek().copied() {
                if is_var_name_char(ch) {
                    name.push(ch);
                    chars.next();
                } else {
                    break;
                }
            }
            Ok(var_value(vars, &name).unwrap_or_default())
        }
        _ => Ok("$".to_string()),
    }
}

fn shell_vars_from_process() -> ShellVars {
    let mut vars = ShellVars::new();

    for (name, value) in env::vars_os() {
        let name = normalize_env_name(&name.to_string_lossy());
        let mut value = value.to_string_lossy().to_string();
        if cfg!(windows) && matches!(name.as_str(), "HOME" | "PWD" | "OLDPWD") {
            value = value.replace('\\', "/");
        }
        vars.insert(
            name,
            ShellVar {
                value,
                exported: true,
            },
        );
    }

    if !vars.contains_key("HOME")
        && let Some(home) = dirs::home_dir()
    {
        vars.insert(
            "HOME".to_string(),
            ShellVar {
                value: to_shell_path(&home),
                exported: true,
            },
        );
    }

    if let Ok(cwd) = env::current_dir() {
        vars.insert(
            "PWD".to_string(),
            ShellVar {
                value: to_shell_path(&cwd),
                exported: true,
            },
        );
    }

    vars
}

fn normalize_env_name(name: &str) -> String {
    if cfg!(windows) && name.eq_ignore_ascii_case("Path") {
        "PATH".to_string()
    } else {
        name.to_string()
    }
}

fn to_shell_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn child_env_for(vars: &ShellVars, assignments: &[Assignment]) -> HashMap<String, String> {
    let mut env = HashMap::new();
    for (name, var) in vars {
        if var.exported {
            env.insert(name.clone(), var.value.clone());
        }
    }

    for assignment in assignments {
        env.insert(assignment.name.clone(), assignment.value.clone());
    }

    env
}

fn var_value(vars: &ShellVars, name: &str) -> Option<String> {
    vars.get(name).map(|var| var.value.clone()).or_else(|| {
        cfg!(windows).then(|| {
            vars.iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, var)| var.value.clone())
        })?
    })
}

fn parse_assignment(word: &str) -> Option<Assignment> {
    let (name, value) = word.split_once('=')?;
    validate_var_name(name).ok()?;
    Some(Assignment {
        name: name.to_string(),
        value: value.to_string(),
    })
}

fn is_assignment_word(word: &str) -> bool {
    word.split_once('=')
        .is_some_and(|(name, _)| validate_var_name(name).is_ok())
}

fn validate_var_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err("variable name cannot be empty".to_string());
    };

    if !is_var_name_start(first) || !chars.all(is_var_name_char) {
        return Err(format!("invalid variable name: {name}"));
    }

    Ok(())
}

fn is_var_name_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

fn is_var_name_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn shell_quote_single(value: &str) -> String {
    value.replace('\'', "'\\''")
}

fn validate_alias_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("alias name cannot be empty".to_string());
    }

    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
    {
        return Err(format!("invalid alias name: {name}"));
    }

    Ok(())
}

fn validate_pipeline_stdio(pipeline: &Pipeline) -> Result<(), String> {
    for (index, command) in pipeline.commands.iter().enumerate() {
        if command.stdout.is_some() && index + 1 < pipeline.commands.len() {
            return Err("output redirection before a pipe is not supported yet".to_string());
        }
    }

    Ok(())
}

fn validate_assignment_only_redirects(command: &CommandSpec) -> Result<(), String> {
    if let Some(path) = &command.stdin {
        fs::File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    }
    if let Some(redirect) = &command.stdout {
        drop(open_output_file(redirect)?);
    }
    if let Some(redirect) = &command.stderr {
        drop(open_output_file(redirect)?);
    }

    Ok(())
}

fn validate_builtin_stdin(command: &CommandSpec) -> Result<(), String> {
    if let Some(path) = &command.stdin {
        fs::File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    }

    Ok(())
}

fn command_is_builtin(command: &CommandSpec) -> bool {
    command
        .args
        .first()
        .is_some_and(|name| BUILTINS.contains(&name.as_str()))
}

fn pipeline_contains_builtin(pipeline: &Pipeline) -> bool {
    pipeline.commands.iter().any(command_is_builtin)
}

fn open_input_stdio(path: &Path) -> Result<Stdio, String> {
    fs::File::open(path)
        .map(Stdio::from)
        .map_err(|error| format!("{}: {error}", path.display()))
}

fn open_output_file(redirect: &Redirect) -> Result<fs::File, String> {
    fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(redirect.append)
        .truncate(!redirect.append)
        .open(&redirect.path)
        .map_err(|error| format!("{}: {error}", redirect.path.display()))
}

fn open_output_stdio(redirect: &Redirect) -> Result<Stdio, String> {
    open_output_file(redirect).map(Stdio::from)
}

enum StandardStream {
    Stdout,
    Stderr,
}

enum WriteTarget {
    Stdout(io::Stdout),
    Stderr(io::Stderr),
    File(fs::File),
    Buffer(Vec<u8>),
}

impl WriteTarget {
    fn into_buffer(self) -> Vec<u8> {
        match self {
            Self::Buffer(buffer) => buffer,
            Self::Stdout(_) | Self::Stderr(_) | Self::File(_) => Vec::new(),
        }
    }
}

impl Write for WriteTarget {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Stdout(stdout) => stdout.write(buf),
            Self::Stderr(stderr) => stderr.write(buf),
            Self::File(file) => file.write(buf),
            Self::Buffer(buffer) => buffer.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Stdout(stdout) => stdout.flush(),
            Self::Stderr(stderr) => stderr.flush(),
            Self::File(file) => file.flush(),
            Self::Buffer(buffer) => buffer.flush(),
        }
    }
}

fn output_writer_for(
    redirect: Option<&Redirect>,
    default_stream: StandardStream,
    capture: bool,
) -> Result<WriteTarget, String> {
    if let Some(redirect) = redirect {
        return open_output_file(redirect).map(WriteTarget::File);
    }

    if capture {
        return Ok(WriteTarget::Buffer(Vec::new()));
    }

    Ok(match default_stream {
        StandardStream::Stdout => WriteTarget::Stdout(io::stdout()),
        StandardStream::Stderr => WriteTarget::Stderr(io::stderr()),
    })
}

fn write_shell_error(stderr_redirect: Option<&Redirect>, message: &str) {
    if let Some(redirect) = stderr_redirect
        && let Ok(mut file) = open_output_file(redirect)
    {
        let _ = writeln!(file, "winbash: {message}");
        return;
    }

    eprintln!("winbash: {message}");
}

fn wait_for_children(children: &mut [(String, Child)]) {
    let interrupts = interrupt_state();
    for (command_name, child) in children {
        let child_id = child.id();
        if let Err(error) = child.wait() {
            eprintln!("winbash: failed to wait for {command_name}: {error}");
        }
        if let Ok(mut jobs) = interrupts.jobs.lock() {
            jobs.remove(&child_id);
        }
    }
}

fn configure_foreground_child(command: &mut Command, interrupt_handler_installed: bool) {
    #[cfg(windows)]
    if interrupt_handler_installed {
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }

    #[cfg(not(windows))]
    let _ = (command, interrupt_handler_installed);
}

fn expand_globs(args: &[String]) -> Vec<String> {
    args.iter()
        .flat_map(|arg| {
            if !has_glob_chars(arg) {
                return vec![arg.clone()];
            }

            let pattern = expand_home_path(arg);
            let pattern = pattern.to_string_lossy().to_string();
            let options = MatchOptions {
                case_sensitive: !cfg!(windows),
                require_literal_separator: false,
                require_literal_leading_dot: false,
            };

            let Ok(paths) = glob_with(&pattern, options) else {
                return vec![arg.clone()];
            };

            let mut matches: Vec<String> = paths
                .filter_map(Result::ok)
                .map(|path| path.to_string_lossy().to_string())
                .collect();
            matches.sort();

            if matches.is_empty() {
                vec![arg.clone()]
            } else {
                matches
            }
        })
        .collect()
}

fn has_glob_chars(arg: &str) -> bool {
    arg.chars().any(|ch| matches!(ch, '*' | '?' | '[' | ']'))
}

fn command_for(command_name: &str, child_env: &HashMap<String, String>) -> Command {
    if UUTILS.contains(&command_name)
        && let Some(command) = uutils_command(command_name, child_env)
    {
        return command;
    }

    Command::new(command_name)
}

fn uutils_command(command_name: &str, child_env: &HashMap<String, String>) -> Option<Command> {
    if let Some(path) = child_env.get("WINBASH_UUTILS_DIR").map(PathBuf::from) {
        let exe = executable_in_dir(&path, command_name);
        if exe.is_file() {
            return Some(Command::new(exe));
        }
    }

    if let Some(path) = child_env.get("WINBASH_COREUTILS").map(PathBuf::from)
        && path.is_file()
    {
        let mut command = Command::new(path);
        command.arg(command_name);
        return Some(command);
    }

    if let Some(path) = which_in_shell_path(command_name, child_env) {
        return Some(Command::new(path));
    }

    if let Some(path) = which_in_shell_path("coreutils", child_env) {
        let mut command = Command::new(path);
        command.arg(command_name);
        return Some(command);
    }

    None
}

fn which_in_shell_path(command_name: &str, child_env: &HashMap<String, String>) -> Option<PathBuf> {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match child_env.get("PATH") {
        Some(path) => which::which_in(command_name, Some(path), cwd).ok(),
        None => which::which(command_name).ok(),
    }
}

fn executable_in_dir(dir: &Path, name: &str) -> PathBuf {
    if cfg!(windows) {
        dir.join(format!("{name}.exe"))
    } else {
        dir.join(name)
    }
}

fn expand_home_path(path: &str) -> PathBuf {
    let Some(home) = dirs::home_dir() else {
        return PathBuf::from(path);
    };

    if path == "~" {
        return home;
    }

    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        return home.join(rest);
    }

    PathBuf::from(path)
}

fn normalize_cd_target(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn prompt_cwd() -> String {
    let Ok(cwd) = env::current_dir() else {
        return "?".to_string();
    };

    if let Some(home) = dirs::home_dir() {
        if cwd == home {
            return "~".to_string();
        }

        if let Ok(rest) = cwd.strip_prefix(&home) {
            let rest = to_shell_path(rest);
            return if rest.is_empty() {
                "~".to_string()
            } else {
                format!("~/{rest}")
            };
        }
    }

    to_shell_path(&cwd)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitRepo {
    worktree: PathBuf,
    git_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitPromptStatus {
    branch: String,
    dirty: bool,
}

#[derive(Clone, Default)]
struct GitPromptCache {
    inner: Arc<Mutex<HashMap<PathBuf, GitPromptCacheEntry>>>,
}

#[derive(Clone)]
struct GitPromptCacheEntry {
    status: Option<GitPromptStatus>,
    refreshing: bool,
    last_refresh: Instant,
}

impl GitPromptCache {
    fn segment(&self, cwd: &Path) -> Option<String> {
        let repo = find_git_repo(cwd)?;
        let branch = read_git_branch(&repo.git_dir)?;
        let dirty = self.cached_dirty_or_refresh(repo, branch.clone());
        let dirty = if dirty { "*" } else { "" };

        Some(format!(" {branch}{dirty}"))
    }

    fn cached_dirty_or_refresh(&self, repo: GitRepo, branch: String) -> bool {
        let now = Instant::now();
        let mut should_refresh = false;
        let dirty = {
            let mut entries = self.inner.lock().expect("git prompt cache lock poisoned");
            let entry = entries
                .entry(repo.worktree.clone())
                .or_insert(GitPromptCacheEntry {
                    status: None,
                    refreshing: false,
                    last_refresh: now - GIT_PROMPT_REFRESH_INTERVAL,
                });

            let dirty = entry
                .status
                .as_ref()
                .filter(|status| status.branch == branch)
                .is_some_and(|status| status.dirty);
            if !entry.refreshing
                && (entry.status.is_none()
                    || entry
                        .status
                        .as_ref()
                        .is_some_and(|status| status.branch != branch)
                    || now.duration_since(entry.last_refresh) >= GIT_PROMPT_REFRESH_INTERVAL)
            {
                entry.refreshing = true;
                should_refresh = true;
            }
            dirty
        };

        if should_refresh {
            self.spawn_refresh(repo, branch);
        }

        dirty
    }

    fn spawn_refresh(&self, repo: GitRepo, branch: String) {
        let inner = Arc::clone(&self.inner);
        thread::spawn(move || {
            let dirty = git_worktree_is_dirty(&repo.worktree);
            let mut entries = inner.lock().expect("git prompt cache lock poisoned");
            entries.insert(
                repo.worktree,
                GitPromptCacheEntry {
                    status: Some(GitPromptStatus { branch, dirty }),
                    refreshing: false,
                    last_refresh: Instant::now(),
                },
            );
        });
    }
}

#[cfg(test)]
fn git_prompt_segment(cwd: &Path) -> Option<String> {
    let status = git_prompt_status(cwd)?;
    let dirty = if status.dirty { "*" } else { "" };
    Some(format!(" {}{}", status.branch, dirty))
}

#[cfg(test)]
fn git_prompt_status(cwd: &Path) -> Option<GitPromptStatus> {
    let repo = find_git_repo(cwd)?;
    let branch = read_git_branch(&repo.git_dir)?;
    let dirty = git_worktree_is_dirty(&repo.worktree);

    Some(GitPromptStatus { branch, dirty })
}

fn find_git_repo(cwd: &Path) -> Option<GitRepo> {
    for dir in cwd.ancestors() {
        let dot_git = dir.join(".git");
        if dot_git.is_dir() {
            return Some(GitRepo {
                worktree: dir.to_path_buf(),
                git_dir: dot_git,
            });
        }

        if dot_git.is_file()
            && let Some(git_dir) = read_gitdir_file(dir, &dot_git)
        {
            return Some(GitRepo {
                worktree: dir.to_path_buf(),
                git_dir,
            });
        }
    }

    None
}

fn read_gitdir_file(worktree: &Path, dot_git: &Path) -> Option<PathBuf> {
    let contents = fs::read_to_string(dot_git).ok()?;
    let gitdir = contents.trim().strip_prefix("gitdir:")?.trim();
    let path = PathBuf::from(gitdir);

    let resolved = if path.is_absolute() {
        path
    } else {
        worktree.join(path)
    };

    Some(fs::canonicalize(&resolved).unwrap_or(resolved))
}

fn read_git_branch(git_dir: &Path) -> Option<String> {
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();

    if let Some(reference) = head.strip_prefix("ref: ") {
        return reference
            .strip_prefix("refs/heads/")
            .or_else(|| reference.rsplit_once('/').map(|(_, name)| name))
            .map(str::to_string);
    }

    let short = head.chars().take(7).collect::<String>();
    (!short.is_empty()).then(|| format!("@{short}"))
}

fn git_worktree_is_dirty(worktree: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["status", "--porcelain"])
        .output()
        .map(|output| output.status.success() && !output.stdout.is_empty())
        .unwrap_or(false)
}

fn interrupt_state() -> InterruptState {
    INTERRUPT_STATE
        .get_or_init(|| {
            let jobs = Arc::new(Mutex::new(HashSet::new()));
            let interrupted = Arc::new(AtomicBool::new(false));
            let monitor_jobs = Arc::clone(&jobs);
            let monitor_interrupted = Arc::clone(&interrupted);
            thread::spawn(move || {
                loop {
                    thread::sleep(INTERRUPT_MONITOR_INTERVAL);
                    if !monitor_interrupted.load(Ordering::SeqCst) {
                        continue;
                    }

                    let child_ids = monitor_jobs
                        .lock()
                        .map(|jobs| jobs.iter().copied().collect::<Vec<_>>())
                        .unwrap_or_default();
                    for child_id in child_ids {
                        kill_process_tree(child_id);
                    }
                }
            });

            InterruptState { jobs, interrupted }
        })
        .clone()
}

fn foreground_interrupt_guard() -> ForegroundInterruptGuard {
    ForegroundInterruptGuard {
        installed: install_foreground_interrupt_handler(),
    }
}

fn install_foreground_interrupt_handler() -> bool {
    #[cfg(windows)]
    unsafe {
        SetConsoleCtrlHandler(Some(winbash_console_ctrl_handler), WINBOOL_TRUE) != 0
    }

    #[cfg(not(windows))]
    {
        false
    }
}

fn uninstall_foreground_interrupt_handler() {
    #[cfg(windows)]
    unsafe {
        SetConsoleCtrlHandler(Some(winbash_console_ctrl_handler), WINBOOL_FALSE);
    }
}

#[cfg(windows)]
unsafe extern "system" fn winbash_console_ctrl_handler(ctrl_type: u32) -> BOOL {
    if matches!(ctrl_type, CTRL_C_EVENT | CTRL_BREAK_EVENT) {
        if let Some(interrupts) = INTERRUPT_STATE.get() {
            interrupts.interrupted.store(true, Ordering::SeqCst);
        }
        WINBOOL_TRUE
    } else {
        WINBOOL_FALSE
    }
}

fn kill_process_tree(pid: u32) {
    if cfg!(windows) {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    } else {
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn history_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".winbash_history"))
}

fn token_start(line: &str, pos: usize) -> usize {
    let mut start = 0;
    let mut quote = None;
    let mut chars = line[..pos].char_indices().peekable();

    while let Some((index, ch)) = chars.next() {
        match quote {
            Some('\'') if ch == '\'' => quote = None,
            Some('"') if ch == '"' => quote = None,
            Some('"')
                if ch == '\\'
                    && chars.peek().is_some_and(|(_, next)| {
                        matches!(next, '"' | '$') || next.is_whitespace()
                    }) =>
            {
                chars.next();
            }
            Some(_) => {}
            None if ch == '\'' || ch == '"' => quote = Some(ch),
            None if ch == '\\'
                && chars.peek().is_some_and(|(_, next)| {
                    matches!(next, '\'' | '"' | '$' | ';' | '&' | '|' | '<' | '>')
                        || next.is_whitespace()
                }) =>
            {
                chars.next();
            }
            None if matches!(ch, '|' | '&' | ';' | '<' | '>') => start = index + ch.len_utf8(),
            None if ch.is_whitespace() => start = index + ch.len_utf8(),
            None => {}
        }
    }

    start
}

#[cfg(test)]
fn completion_is_command_position(prefix: &str) -> bool {
    completion_context(prefix).command_position
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompletionContext {
    command_position: bool,
    command_name: Option<String>,
    arg_index: usize,
}

fn completion_context(prefix: &str) -> CompletionContext {
    let vars = ShellVars::new();
    let Ok(tokens) = tokenize_line(prefix, &vars, 0, false) else {
        return CompletionContext {
            command_position: false,
            command_name: None,
            arg_index: 0,
        };
    };

    let mut command_position = true;
    let mut expecting_redirect_target = false;
    let mut command_name = None;
    let mut arg_index = 0;

    for token in tokens {
        match token {
            Token::Pipe | Token::Sequence | Token::AndIf | Token::OrIf => {
                command_position = true;
                expecting_redirect_target = false;
                command_name = None;
                arg_index = 0;
            }
            Token::RedirectIn | Token::RedirectOut { .. } | Token::RedirectErr { .. } => {
                command_position = false;
                expecting_redirect_target = true;
            }
            Token::Word(_) if expecting_redirect_target => {
                expecting_redirect_target = false;
            }
            Token::Word(word) if command_position => {
                command_name = Some(word);
                command_position = false;
            }
            Token::Word(_) => {
                arg_index += 1;
            }
        }
    }

    CompletionContext {
        command_position: command_position && !expecting_redirect_target,
        command_name,
        arg_index,
    }
}

fn is_pathish(token: &str) -> bool {
    token.contains('/') || token.contains('\\') || token.starts_with('.') || token.starts_with('~')
}

fn complete_commands(prefix: &str, aliases: &Aliases) -> Vec<Pair> {
    let mut commands = BTreeSet::new();

    for builtin in BUILTINS {
        commands.insert((*builtin).to_string());
    }
    for command in UUTILS {
        commands.insert((*command).to_string());
    }
    for command in path_commands() {
        commands.insert(command);
    }
    for alias in aliases.read().expect("aliases lock poisoned").keys() {
        commands.insert(alias.clone());
    }

    commands
        .into_iter()
        .filter(|command| starts_with(command, prefix))
        .map(|command| Pair {
            display: command.clone(),
            replacement: command,
        })
        .collect()
}

fn complete_variables(raw_prefix: &str, vars: &ShellVars) -> Vec<Pair> {
    let braced = raw_prefix.starts_with("${");
    let prefix = if braced {
        raw_prefix.trim_start_matches("${")
    } else {
        raw_prefix.trim_start_matches('$')
    };
    let prefix = prefix.trim_end_matches('}');

    let mut names = BTreeSet::new();
    for name in vars.keys() {
        names.insert(name.clone());
    }
    for (name, _) in env::vars_os() {
        names.insert(normalize_env_name(&name.to_string_lossy()));
    }

    names
        .into_iter()
        .filter(|name| starts_with(name, prefix))
        .map(|name| {
            let replacement = if braced {
                format!("${{{name}}}")
            } else {
                format!("${name}")
            };
            Pair {
                display: replacement.clone(),
                replacement,
            }
        })
        .collect()
}

fn path_commands() -> BTreeSet<String> {
    let mut commands = BTreeSet::new();
    let Some(paths) = env::var_os("PATH") else {
        return commands;
    };

    let extensions = executable_extensions();
    for dir in env::split_paths(&paths) {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };

            if cfg!(windows) {
                let extension = path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .map(|extension| format!(".{}", extension.to_ascii_lowercase()));
                if extension
                    .as_ref()
                    .is_some_and(|extension| extensions.contains(extension))
                {
                    if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
                        commands.insert(stem.to_string());
                    }
                    commands.insert(file_name.to_string());
                }
            } else {
                commands.insert(file_name.to_string());
            }
        }
    }

    commands
}

fn executable_extensions() -> HashSet<String> {
    env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
        .split(';')
        .map(|extension| extension.to_ascii_lowercase())
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathCompletionMode {
    All,
    DirectoriesOnly,
}

fn complete_paths(raw_prefix: &str) -> Vec<Pair> {
    complete_paths_with_mode(raw_prefix, PathCompletionMode::All)
}

fn complete_paths_with_mode(raw_prefix: &str, mode: PathCompletionMode) -> Vec<Pair> {
    let prefix = unescape_completion_prefix(raw_prefix.trim_start_matches(['\'', '"']));
    let (dir_part, leaf_prefix) = split_path_prefix(&prefix);
    let search_dir = resolve_completion_dir(dir_part);
    let Ok(entries) = fs::read_dir(&search_dir) else {
        return Vec::new();
    };

    let separator = preferred_separator(&prefix);
    let mut dirs = Vec::new();
    let mut files = Vec::new();

    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };

        if !starts_with(&name, leaf_prefix) {
            continue;
        }

        if mode == PathCompletionMode::DirectoriesOnly && !file_type.is_dir() {
            continue;
        }

        let mut replacement = format!("{dir_part}{name}");
        if file_type.is_dir() {
            replacement.push(separator);
        }

        let pair = Pair {
            display: replacement.clone(),
            replacement: escape_token(&replacement),
        };

        if file_type.is_dir() {
            dirs.push(pair);
        } else {
            files.push(pair);
        }
    }

    dirs.sort_by(|left, right| left.display.cmp(&right.display));
    files.sort_by(|left, right| left.display.cmp(&right.display));
    dirs.extend(files);
    dirs
}

fn split_path_prefix(prefix: &str) -> (&str, &str) {
    let Some(index) = prefix.rfind(['/', '\\']) else {
        return ("", prefix);
    };

    let split = index + 1;
    (&prefix[..split], &prefix[split..])
}

fn unescape_completion_prefix(raw_prefix: &str) -> String {
    let mut unescaped = String::new();
    let mut chars = raw_prefix.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\\'
            && chars
                .peek()
                .is_some_and(|next| matches!(next, '\'' | '"') || next.is_whitespace())
        {
            unescaped.push(chars.next().expect("peeked char exists"));
        } else {
            unescaped.push(ch);
        }
    }

    unescaped
}

fn resolve_completion_dir(dir_part: &str) -> PathBuf {
    if dir_part.is_empty() {
        return env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    }

    let expanded = expand_home_path(dir_part);
    if expanded.is_absolute() {
        expanded
    } else {
        env::current_dir()
            .map(|cwd| cwd.join(&expanded))
            .unwrap_or(expanded)
    }
}

fn preferred_separator(prefix: &str) -> char {
    if prefix.contains('\\') && !prefix.contains('/') {
        '\\'
    } else {
        '/'
    }
}

fn escape_token(value: &str) -> String {
    let mut escaped = String::new();
    for ch in value.chars() {
        if ch.is_whitespace() || matches!(ch, '\'' | '"') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

fn starts_with(value: &str, prefix: &str) -> bool {
    if cfg!(windows) {
        value
            .to_ascii_lowercase()
            .starts_with(&prefix.to_ascii_lowercase())
    } else {
        value.starts_with(prefix)
    }
}

fn write_help(stdout: &mut dyn Write) -> Result<(), String> {
    writeln!(
        stdout,
        "winbash: minimal Windows shell\n\
         \n\
         Builtins:\n\
           alias NAME=VALUE   define command-position aliases\n\
           unalias NAME       remove aliases\n\
           cd [DIR|-]         change directory\n\
           export [NAME[=VALUE] ...]\n\
           pwd                print current directory\n\
           source FILE        execute winbash commands from FILE\n\
           unset NAME ...      remove shell variables\n\
           exit [STATUS]      leave the shell\n\
           help               show this help\n\
         \n\
         Startup files:\n\
           ~/.zshrc           imports simple alias NAME=VALUE lines\n\
           ~/.winbashrc       executes supported winbash commands\n\
         \n\
         Prompt status:\n\
           git branch appears in repositories\n\
           git branch* means the worktree is dirty\n\
           !STATUS appears after a failed command\n\
         \n\
         Linux-style variables:\n\
           $NAME and ${{NAME}} expand outside single quotes\n\
           NAME=value sets a shell variable\n\
           NAME=value command runs command with a temporary environment value\n\
           %NAME% is treated literally\n\
         \n\
         Control operators:\n\
           command; next       run commands sequentially\n\
           command && next     run next only if command succeeds\n\
           command || next     run next only if command fails\n\
         \n\
         uutils discovery:\n\
           WINBASH_UUTILS_DIR directory containing ls.exe, cat.exe, ...\n\
           WINBASH_COREUTILS  path to coreutils.exe multi-call binary\n\
           PATH               fallback for separate tools or coreutils.exe"
    )
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn parser_preserves_windows_backslashes() {
        assert_eq!(
            parse_line(r#"ls C:\Users\maxim\Documents"#).unwrap(),
            vec!["ls", r"C:\Users\maxim\Documents"]
        );
    }

    #[test]
    fn parser_preserves_unc_backslashes() {
        assert_eq!(
            parse_line(r#"ls \\server\share"#).unwrap(),
            vec!["ls", r"\\server\share"]
        );
    }

    #[test]
    fn parser_handles_alias_assignment_quotes() {
        assert_eq!(
            parse_line("alias ll='ls -la'").unwrap(),
            vec!["alias", "ll=ls -la"]
        );
    }

    #[test]
    fn parser_supports_escaped_spaces_without_breaking_paths() {
        assert_eq!(
            parse_line(r#"ls folder\ with\ spaces C:\plain\path"#).unwrap(),
            vec!["ls", "folder with spaces", r"C:\plain\path"]
        );
    }

    #[test]
    fn tokenizer_splits_pipeline_and_redirection_without_spaces() {
        let vars = ShellVars::new();
        assert_eq!(
            tokenize_line("ls src|rg main>out.txt", &vars, 0, true).unwrap(),
            vec![
                Token::Word("ls".to_string()),
                Token::Word("src".to_string()),
                Token::Pipe,
                Token::Word("rg".to_string()),
                Token::Word("main".to_string()),
                Token::RedirectOut { append: false },
                Token::Word("out.txt".to_string()),
            ]
        );
    }

    #[test]
    fn tokenizer_splits_control_operators_without_spaces() {
        let vars = ShellVars::new();
        assert_eq!(
            tokenize_line("false&&echo no||echo yes;pwd", &vars, 0, true).unwrap(),
            vec![
                Token::Word("false".to_string()),
                Token::AndIf,
                Token::Word("echo".to_string()),
                Token::Word("no".to_string()),
                Token::OrIf,
                Token::Word("echo".to_string()),
                Token::Word("yes".to_string()),
                Token::Sequence,
                Token::Word("pwd".to_string()),
            ]
        );
    }

    #[test]
    fn split_command_list_keeps_pipes_and_quoted_control_operators() {
        assert_eq!(
            split_command_list(r#"echo "a;b" | cat && echo ok || echo fallback"#).unwrap(),
            vec![
                CommandListItem {
                    connector: None,
                    source: r#"echo "a;b" | cat"#.to_string(),
                },
                CommandListItem {
                    connector: Some(Connector::And),
                    source: "echo ok".to_string(),
                },
                CommandListItem {
                    connector: Some(Connector::Or),
                    source: "echo fallback".to_string(),
                },
            ]
        );
    }

    #[test]
    fn split_command_list_allows_trailing_semicolon() {
        assert_eq!(
            split_command_list("echo ok;").unwrap(),
            vec![CommandListItem {
                connector: None,
                source: "echo ok".to_string(),
            }]
        );
    }

    #[test]
    fn split_command_list_rejects_trailing_conditionals() {
        assert!(split_command_list("echo ok &&").unwrap_err().contains("&&"));
        assert!(split_command_list("echo ok ||").unwrap_err().contains("||"));
    }

    #[test]
    fn pipeline_parser_builds_commands_and_redirects() {
        let vars = ShellVars::new();
        let pipeline = parse_pipeline("ls src | rg main > out.txt", &vars, 0).unwrap();

        assert_eq!(pipeline.commands.len(), 2);
        assert_eq!(pipeline.commands[0].args, vec!["ls", "src"]);
        assert_eq!(pipeline.commands[1].args, vec!["rg", "main"]);
        assert_eq!(
            pipeline.commands[1].stdout,
            Some(Redirect {
                path: PathBuf::from("out.txt"),
                append: false,
            })
        );
    }

    #[test]
    fn pipeline_parser_supports_input_and_stderr_append_redirection() {
        let vars = ShellVars::new();
        let pipeline = parse_pipeline("rg needle < input.txt 2>> errors.txt", &vars, 0).unwrap();

        assert_eq!(pipeline.commands.len(), 1);
        assert_eq!(pipeline.commands[0].args, vec!["rg", "needle"]);
        assert_eq!(pipeline.commands[0].stdin, Some(PathBuf::from("input.txt")));
        assert_eq!(
            pipeline.commands[0].stderr,
            Some(Redirect {
                path: PathBuf::from("errors.txt"),
                append: true,
            })
        );
    }

    #[test]
    fn pipeline_parser_rejects_missing_redirect_target() {
        let vars = ShellVars::new();
        assert!(
            parse_pipeline("echo hi >", &vars, 0)
                .unwrap_err()
                .contains("missing target")
        );
    }

    #[test]
    fn pipeline_parser_rejects_missing_command_after_pipe() {
        let vars = ShellVars::new();
        assert!(
            parse_pipeline("echo hi |", &vars, 0)
                .unwrap_err()
                .contains("missing command")
        );
    }

    #[test]
    fn escaped_control_operator_stays_in_word() {
        assert_eq!(
            parse_line_expanding(r#"echo a\;b c\&d e\|f"#, &ShellVars::new(), 0).unwrap(),
            vec!["echo", "a;b", "c&d", "e|f"]
        );
    }

    #[test]
    fn parser_expands_linux_style_variables() {
        let mut vars = ShellVars::new();
        vars.insert(
            "FOO".to_string(),
            ShellVar {
                value: "bar".to_string(),
                exported: false,
            },
        );

        assert_eq!(
            parse_line_expanding(r#"echo $FOO ${FOO} "$FOO" '$FOO' \$FOO $?"#, &vars, 7).unwrap(),
            vec!["echo", "bar", "bar", "bar", "$FOO", "$FOO", "7"]
        );
    }

    #[test]
    fn parser_does_not_expand_windows_percent_variables() {
        let mut vars = ShellVars::new();
        vars.insert(
            "FOO".to_string(),
            ShellVar {
                value: "bar".to_string(),
                exported: true,
            },
        );

        assert_eq!(
            parse_line_expanding("echo %FOO%", &vars, 0).unwrap(),
            vec!["echo", "%FOO%"]
        );
    }

    #[test]
    fn pipeline_parser_extracts_leading_assignments() {
        let vars = ShellVars::new();
        let pipeline = parse_pipeline("FOO=bar EMPTY= echo ok", &vars, 0).unwrap();

        assert_eq!(pipeline.commands[0].assignments.len(), 2);
        assert_eq!(
            pipeline.commands[0].assignments[0],
            Assignment {
                name: "FOO".to_string(),
                value: "bar".to_string(),
            }
        );
        assert_eq!(pipeline.commands[0].args, vec!["echo", "ok"]);
    }

    #[test]
    fn bare_assignment_sets_shell_var_without_exporting_new_name() {
        let mut shell = Shell::new();
        shell.vars.remove("WINBASH_TEST_VAR");

        assert!(matches!(
            shell.run_line("WINBASH_TEST_VAR=hello").unwrap(),
            LineResult::Continue(0)
        ));
        assert_eq!(shell.vars["WINBASH_TEST_VAR"].value, "hello");
        assert!(!shell.vars["WINBASH_TEST_VAR"].exported);
    }

    #[test]
    fn export_marks_or_sets_exported_variables() {
        let mut shell = Shell::new();
        shell.vars.remove("WINBASH_TEST_VAR");

        shell.run_line("WINBASH_TEST_VAR=hello").unwrap();
        shell.run_line("export WINBASH_TEST_VAR").unwrap();
        assert!(shell.vars["WINBASH_TEST_VAR"].exported);

        shell.run_line("export WINBASH_TEST_OTHER=world").unwrap();
        assert_eq!(shell.vars["WINBASH_TEST_OTHER"].value, "world");
        assert!(shell.vars["WINBASH_TEST_OTHER"].exported);
    }

    #[test]
    fn unset_removes_shell_vars() {
        let mut shell = Shell::new();

        shell.run_line("export WINBASH_TEST_VAR=hello").unwrap();
        shell.run_line("unset WINBASH_TEST_VAR").unwrap();

        assert!(!shell.vars.contains_key("WINBASH_TEST_VAR"));
    }

    #[test]
    fn winbashrc_executes_supported_shell_lines() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let rc = temp.path().join(".winbashrc");
        fs::write(
            &rc,
            "export WINBASH_RC_EXPORTED=hello\n\
             WINBASH_RC_LOCAL=$WINBASH_RC_EXPORTED\n\
             alias greet='echo hi'\n",
        )
        .expect("rc file should be written");

        let mut shell = Shell::new();
        shell.vars.remove("WINBASH_RC_EXPORTED");
        shell.vars.remove("WINBASH_RC_LOCAL");
        shell.load_shell_file(&rc);

        assert_eq!(shell.vars["WINBASH_RC_EXPORTED"].value, "hello");
        assert!(shell.vars["WINBASH_RC_EXPORTED"].exported);
        assert_eq!(shell.vars["WINBASH_RC_LOCAL"].value, "hello");
        assert!(!shell.vars["WINBASH_RC_LOCAL"].exported);

        let aliases = shell
            .aliases
            .read()
            .expect("aliases lock should not be poisoned");
        assert_eq!(aliases["greet"], "echo hi");
    }

    #[test]
    fn zshrc_import_stays_limited_to_simple_aliases() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let zshrc = temp.path().join(".zshrc");
        fs::write(
            &zshrc,
            "alias ll='ls -la'\n\
             export SHOULD_NOT_IMPORT=yes\n",
        )
        .expect("zshrc file should be written");

        let mut shell = Shell::new();
        shell.vars.remove("SHOULD_NOT_IMPORT");
        shell.load_alias_file(&zshrc);

        let aliases = shell
            .aliases
            .read()
            .expect("aliases lock should not be poisoned");
        assert_eq!(aliases["ll"], "ls -la");
        assert!(!shell.vars.contains_key("SHOULD_NOT_IMPORT"));
    }

    #[test]
    fn command_list_expands_vars_after_previous_assignment() {
        let mut shell = Shell::new();

        shell
            .run_line("WINBASH_FIRST=hello; WINBASH_SECOND=$WINBASH_FIRST")
            .unwrap();

        assert_eq!(shell.vars["WINBASH_SECOND"].value, "hello");
    }

    #[test]
    fn and_or_conditionals_follow_last_status() {
        let mut shell = Shell::new();
        shell.vars.remove("WINBASH_SHOULD_SKIP");
        shell.vars.remove("WINBASH_SHOULD_SET");

        shell
            .run_line("definitely_missing_winbash_command && WINBASH_SHOULD_SKIP=no || WINBASH_SHOULD_SET=yes")
            .unwrap();

        assert!(!shell.vars.contains_key("WINBASH_SHOULD_SKIP"));
        assert_eq!(shell.vars["WINBASH_SHOULD_SET"].value, "yes");
    }

    #[test]
    fn child_env_contains_exported_vars_and_local_assignments() {
        let mut vars = ShellVars::new();
        vars.insert(
            "EXPORTED".to_string(),
            ShellVar {
                value: "yes".to_string(),
                exported: true,
            },
        );
        vars.insert(
            "LOCAL".to_string(),
            ShellVar {
                value: "no".to_string(),
                exported: false,
            },
        );

        let env = child_env_for(
            &vars,
            &[Assignment {
                name: "LOCAL".to_string(),
                value: "temporary".to_string(),
            }],
        );

        assert_eq!(env["EXPORTED"], "yes");
        assert_eq!(env["LOCAL"], "temporary");
    }

    #[test]
    fn git_prompt_reads_branch_from_head() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let git_dir = temp.path().join(".git");
        fs::create_dir(&git_dir).expect("git dir should be created");
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD should be written");

        assert_eq!(
            git_prompt_status(temp.path()),
            Some(GitPromptStatus {
                branch: "main".to_string(),
                dirty: false,
            })
        );
        assert_eq!(git_prompt_segment(temp.path()), Some(" main".to_string()));
    }

    #[test]
    fn git_prompt_finds_repo_from_child_directory() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let git_dir = temp.path().join(".git");
        let child = temp.path().join("a").join("b");
        fs::create_dir(&git_dir).expect("git dir should be created");
        fs::create_dir_all(&child).expect("child dirs should be created");
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/feature/test\n")
            .expect("HEAD should be written");

        assert_eq!(
            git_prompt_status(&child),
            Some(GitPromptStatus {
                branch: "feature/test".to_string(),
                dirty: false,
            })
        );
    }

    #[test]
    fn git_prompt_handles_detached_head() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let git_dir = temp.path().join(".git");
        fs::create_dir(&git_dir).expect("git dir should be created");
        fs::write(
            git_dir.join("HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .expect("HEAD should be written");

        assert_eq!(
            git_prompt_status(temp.path()),
            Some(GitPromptStatus {
                branch: "@0123456".to_string(),
                dirty: false,
            })
        );
    }

    #[test]
    fn git_prompt_supports_gitdir_file() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let worktree = temp.path().join("worktree");
        let git_dir = temp.path().join("actual-git-dir");
        fs::create_dir(&worktree).expect("worktree should be created");
        fs::create_dir(&git_dir).expect("git dir should be created");
        fs::write(worktree.join(".git"), "gitdir: ../actual-git-dir\n")
            .expect(".git file should be written");
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD should be written");

        assert_eq!(
            find_git_repo(&worktree),
            Some(GitRepo {
                worktree: worktree.clone(),
                git_dir: fs::canonicalize(&git_dir).expect("git dir should canonicalize"),
            })
        );
        assert_eq!(git_prompt_segment(&worktree), Some(" main".to_string()));
    }

    #[test]
    fn alias_expansion_only_replaces_command_position() {
        let mut shell = Shell::new();
        shell
            .define_aliases(&["ll=ls -la".to_string()])
            .expect("alias should be accepted");

        assert_eq!(
            shell
                .expand_aliases(vec!["ll".to_string(), "src".to_string()])
                .unwrap(),
            vec!["ls", "-la", "src"]
        );
    }

    #[test]
    fn alias_expansion_applies_to_each_pipeline_command() {
        let mut shell = Shell::new();
        shell
            .define_aliases(&["ll=ls -la".to_string(), "s=sort".to_string()])
            .expect("aliases should be accepted");
        let vars = ShellVars::new();
        let mut pipeline = parse_pipeline("ll src | s", &vars, 0).unwrap();

        shell.expand_pipeline_aliases(&mut pipeline).unwrap();

        assert_eq!(pipeline.commands[0].args, vec!["ls", "-la", "src"]);
        assert_eq!(pipeline.commands[1].args, vec!["sort"]);
    }

    #[test]
    fn token_start_ignores_whitespace_inside_quotes() {
        let line = r#"ls "folder with"#;
        assert_eq!(token_start(line, line.len()), 3);
    }

    #[test]
    fn token_start_ignores_escaped_spaces() {
        let line = r#"ls folder\ with"#;
        assert_eq!(token_start(line, line.len()), 3);
    }

    #[test]
    fn token_start_treats_pipe_as_delimiter() {
        let line = "ls|rg";
        assert_eq!(token_start(line, line.len()), 3);
    }

    #[test]
    fn token_start_treats_control_operators_as_delimiters() {
        let line = "false&&echo";
        assert_eq!(token_start(line, line.len()), 7);
    }

    #[test]
    fn completion_detects_command_position_after_pipe() {
        assert!(completion_is_command_position("ls | "));
        assert!(completion_is_command_position("false && "));
        assert!(completion_is_command_position("false || "));
        assert!(completion_is_command_position("pwd; "));
        assert!(!completion_is_command_position("ls > "));
    }

    #[test]
    fn completion_prefix_unescapes_spaces_but_not_path_slashes() {
        assert_eq!(
            unescape_completion_prefix(r#"folder\ with\ spaces\sub"#),
            r#"folder with spaces\sub"#
        );
    }

    #[test]
    fn completion_for_cd_returns_directories_only() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        fs::create_dir(temp.path().join("docs")).expect("directory fixture should be created");
        fs::write(temp.path().join("done.txt"), "").expect("file fixture should be created");

        let prefix = format!("{}/d", to_shell_path(temp.path()));
        let completions = complete_paths_with_mode(&prefix, PathCompletionMode::DirectoriesOnly);

        assert!(
            completions
                .iter()
                .any(|pair| pair.display.ends_with("docs/"))
        );
        assert!(
            !completions
                .iter()
                .any(|pair| pair.display.ends_with("done.txt"))
        );
    }

    #[test]
    fn completion_expands_shell_variables() {
        let mut vars = ShellVars::new();
        vars.insert(
            "FOO_VALUE".to_string(),
            ShellVar {
                value: "bar".to_string(),
                exported: false,
            },
        );

        let completions = complete_variables("$FO", &vars);

        assert!(
            completions
                .iter()
                .any(|pair| pair.replacement == "$FOO_VALUE")
        );
    }

    #[test]
    fn cached_git_prompt_renders_dirty_status_without_blocking_refresh() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let git_dir = temp.path().join(".git");
        fs::create_dir(&git_dir).expect("git dir should be created");
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD should be written");

        let cache = GitPromptCache::default();
        cache.inner.lock().expect("cache should lock").insert(
            temp.path().to_path_buf(),
            GitPromptCacheEntry {
                status: Some(GitPromptStatus {
                    branch: "main".to_string(),
                    dirty: true,
                }),
                refreshing: false,
                last_refresh: Instant::now(),
            },
        );

        assert_eq!(cache.segment(temp.path()), Some(" main*".to_string()));
    }

    #[test]
    fn command_substitution_finds_nested_expression_end() {
        let line = r#"echo $(echo "$(echo nested)") done"#;
        let start = line
            .find("$(")
            .expect("fixture should contain substitution")
            + 2;

        assert_eq!(
            &line[start..find_command_substitution_end(line, start).unwrap()],
            r#"echo "$(echo nested)""#
        );
    }

    fn plain_word_strategy() -> impl Strategy<Value = String> {
        "[A-Za-z0-9_./:-]{1,24}".prop_map(|value| value)
    }

    fn var_name_strategy() -> impl Strategy<Value = String> {
        "[A-Za-z_][A-Za-z0-9_]{0,20}".prop_map(|value| value)
    }

    fn var_value_strategy() -> impl Strategy<Value = String> {
        "[A-Za-z0-9_./:-]{0,32}".prop_map(|value| value)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        #[test]
        fn prop_plain_words_roundtrip(words in prop::collection::vec(plain_word_strategy(), 0..12)) {
            let line = words.join(" ");
            prop_assert_eq!(parse_line(&line).unwrap(), words);
        }

        #[test]
        fn prop_linux_variable_expansion_matches_shell_vars(
            name in var_name_strategy(),
            value in var_value_strategy(),
            status in 0_i32..256,
        ) {
            let mut vars = ShellVars::new();
            vars.insert(
                name.clone(),
                ShellVar {
                    value: value.clone(),
                    exported: false,
                },
            );

            let braced = format!("${{{name}}}");
            let line = format!("echo ${name} {braced} \"${name}\" '${name}' $?");
            let mut expected = vec!["echo".to_string()];
            if !value.is_empty() {
                expected.push(value.clone());
                expected.push(value.clone());
            }
            expected.push(value.clone());
            expected.push(format!("${name}"));
            expected.push(status.to_string());

            prop_assert_eq!(
                parse_line_expanding(&line, &vars, status).unwrap(),
                expected,
            );
        }

        #[test]
        fn prop_semicolon_command_list_preserves_segments(
            segments in prop::collection::vec(plain_word_strategy(), 1..10),
        ) {
            let line = segments.join(";");
            let parsed = split_command_list(&line).unwrap();
            let expected: Vec<_> = segments
                .into_iter()
                .enumerate()
                .map(|(index, source)| CommandListItem {
                    connector: (index > 0).then_some(Connector::Sequence),
                    source,
                })
                .collect();

            prop_assert_eq!(parsed, expected);
        }

        #[test]
        fn prop_escaped_metacharacters_stay_literal(
            ch in prop::sample::select(vec![';', '&', '|', '<', '>', '$', ' ']),
        ) {
            let line = format!("echo a\\{ch}b");
            prop_assert_eq!(
                parse_line_expanding(&line, &ShellVars::new(), 0).unwrap(),
                vec!["echo".to_string(), format!("a{ch}b")]
            );
        }

        #[test]
        fn prop_plain_command_substitution_end_is_balanced(word in plain_word_strategy()) {
            let line = format!("echo $(printf {word}) tail");
            let start = line.find("$(").expect("fixture should contain substitution") + 2;
            let end = find_command_substitution_end(&line, start).unwrap();

            prop_assert_eq!(&line[start..end], format!("printf {word}"));
        }
    }
}
