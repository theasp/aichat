mod completer;
mod highlighter;
mod prompt;

use self::completer::ReplCompleter;
use self::highlighter::ReplHighlighter;
use self::prompt::ReplPrompt;

use crate::client::init_client;
use crate::config::GlobalConfig;
use crate::render::{render_error, render_stream};
use crate::utils::{create_abort_signal, set_text, AbortSignal};

use anyhow::{bail, Context, Result};
use crossbeam::sync::WaitGroup;
use fancy_regex::Regex;
use lazy_static::lazy_static;
use reedline::Signal;
use reedline::{
    default_emacs_keybindings, default_vi_insert_keybindings, default_vi_normal_keybindings,
    ColumnarMenu, EditMode, Emacs, KeyCode, KeyModifiers, Keybindings, Reedline, ReedlineEvent,
    ReedlineMenu, ValidationResult, Validator, Vi,
};
use std::io::Read;

const MENU_NAME: &str = "completion_menu";

const REPL_COMMANDS: [(&str, &str); 13] = [
    (".help", "Print this help message"),
    (".info", "Print system info"),
    (".model", "Switch LLM model"),
    (".role", "Use a role"),
    (".info role", "Show role info"),
    (".exit role", "Leave current role"),
    (".session", "Start a context-aware chat session"),
    (".info session", "Show session info"),
    (".exit session", "End the current session"),
    (".set", "Modify the configuration parameters"),
    (".copy", "Copy the last reply to the clipboard"),
    (".read", "Import from file and submit"),
    (".exit", "Exit the REPL"),
];

lazy_static! {
    static ref COMMAND_RE: Regex = Regex::new(r"^\s*(\.\S*)\s*").unwrap();
    static ref MULTILINE_RE: Regex = Regex::new(r#"(?s)^\s*"""\s*(.*)\s*"""\s*$"#).unwrap();
}

pub struct Repl {
    config: GlobalConfig,
    editor: Reedline,
    prompt: ReplPrompt,
    abort: AbortSignal,
}

impl Repl {
    pub fn init(config: &GlobalConfig) -> Result<Self> {
        let editor = Self::create_editor(config)?;

        let prompt = ReplPrompt::new(config);

        let abort = create_abort_signal();

        Ok(Self {
            config: config.clone(),
            editor,
            prompt,
            abort,
        })
    }

    pub fn run(&mut self) -> Result<()> {
        self.banner();

        let mut already_ctrlc = false;

        loop {
            if self.abort.aborted_ctrld() {
                break;
            }
            if self.abort.aborted_ctrlc() && !already_ctrlc {
                already_ctrlc = true;
            }
            let sig = self.editor.read_line(&self.prompt);
            match sig {
                Ok(Signal::Success(line)) => {
                    already_ctrlc = false;
                    self.abort.reset();
                    match self.handle(&line) {
                        Ok(quit) => {
                            if quit {
                                break;
                            }
                        }
                        Err(err) => {
                            render_error(err, self.config.read().highlight);
                            println!()
                        }
                    }
                }
                Ok(Signal::CtrlC) => {
                    self.abort.set_ctrlc();
                    if already_ctrlc {
                        break;
                    }
                    already_ctrlc = true;
                    println!("(To exit, press Ctrl+C again or Ctrl+D or type .exit)\n");
                }
                Ok(Signal::CtrlD) => {
                    self.abort.set_ctrld();
                    break;
                }
                _ => {}
            }
        }
        self.handle(".exit session")?;
        Ok(())
    }

    fn handle(&self, mut line: &str) -> Result<bool> {
        if let Ok(Some(captures)) = MULTILINE_RE.captures(line) {
            if let Some(text_match) = captures.get(1) {
                line = text_match.as_str();
            }
        }
        match parse_command(line) {
            Some((cmd, args)) => match cmd {
                ".help" => {
                    dump_repl_help();
                }
                ".info" => match args {
                    Some("role") => {
                        let info = self.config.read().role_info()?;
                        println!("{}", info);
                    }
                    Some("session") => {
                        let info = self.config.read().session_info()?;
                        println!("{}", info);
                    }
                    Some(_) => unknown_command()?,
                    None => {
                        let output = self.config.read().sys_info()?;
                        println!("{}", output);
                    }
                },
                ".edit" => {
                    println!(r#"Deprecated. Use """ instead."#);
                }
                ".model" => match args {
                    Some(name) => {
                        self.config.write().set_model(name)?;
                    }
                    None => println!("Usage: .model <name>"),
                },
                ".role" => match args {
                    Some(args) => match args.split_once(|c| c == '\n' || c == ' ') {
                        Some((name, text)) => {
                            let name = name.trim();
                            let text = text.trim();
                            let old_role =
                                self.config.read().role.as_ref().map(|v| v.name.to_string());
                            self.config.write().set_role(name)?;
                            self.ask(text)?;
                            match old_role {
                                Some(old_role) => self.config.write().set_role(&old_role)?,
                                None => self.config.write().clear_role()?,
                            }
                        }
                        None => {
                            self.config.write().set_role(args)?;
                        }
                    },
                    None => println!(r#"Usage: .role <name> [text...]"#),
                },
                ".session" => {
                    self.config.write().start_session(args)?;
                }
                ".set" => {
                    if let Some(args) = args {
                        self.config.write().update(args)?;
                    }
                }
                ".copy" => {
                    let config = self.config.read();
                    self.copy(config.last_reply())
                        .with_context(|| "Failed to copy the last output")?;
                }
                ".read" => match args {
                    Some(file) => {
                        let mut content = String::new();
                        let mut file =
                            std::fs::File::open(file).with_context(|| "Unable to open file")?;
                        file.read_to_string(&mut content)
                            .with_context(|| "Unable to read file")?;
                        self.ask(&content)?;
                    }
                    None => println!("Usage: .read <textfile>"),
                },
                ".exit" => match args {
                    Some("role") => {
                        self.config.write().clear_role()?;
                    }
                    Some("session") => {
                        self.config.write().end_session()?;
                    }
                    Some(_) => unknown_command()?,
                    None => {
                        return Ok(true);
                    }
                },
                // deprecated this command
                ".clear" => match args {
                    Some("role") => {
                        println!(r#"Deprecated. Use ".exit role" instead."#);
                    }
                    Some("conversation") => {
                        println!(r#"Deprecated. Use ".exit session" instead."#);
                    }
                    _ => unknown_command()?,
                },
                _ => unknown_command()?,
            },
            None => {
                self.ask(line)?;
            }
        }

        println!();

        Ok(false)
    }

    fn ask(&self, input: &str) -> Result<()> {
        if input.is_empty() {
            return Ok(());
        }
        self.config.read().maybe_print_send_tokens(input);
        let wg = WaitGroup::new();
        let client = init_client(&self.config)?;
        let ret = render_stream(
            input,
            client.as_ref(),
            &self.config,
            true,
            self.abort.clone(),
            wg.clone(),
        );
        wg.wait();
        let buffer = ret?;
        self.config.write().save_message(input, &buffer)?;
        if self.config.read().auto_copy {
            let _ = self.copy(&buffer);
        }
        Ok(())
    }

    fn banner(&self) {
        let version = env!("CARGO_PKG_VERSION");
        print!(
            r#"Welcome to aichat {version}
Type ".help" for more information.
"#
        )
    }

    fn create_editor(config: &GlobalConfig) -> Result<Reedline> {
        let completer = ReplCompleter::new(config);
        let highlighter = ReplHighlighter::new(config);
        let menu = Self::create_menu();
        let edit_mode = Self::create_edit_mode(config);
        let mut editor = Reedline::create()
            .with_completer(Box::new(completer))
            .with_highlighter(Box::new(highlighter))
            .with_menu(menu)
            .with_edit_mode(edit_mode)
            .with_quick_completions(true)
            .with_partial_completions(true)
            .with_validator(Box::new(ReplValidator))
            .with_ansi_colors(true);

        editor.enable_bracketed_paste()?;

        Ok(editor)
    }

    fn extra_keybindings(keybindings: &mut Keybindings) {
        keybindings.add_binding(
            KeyModifiers::NONE,
            KeyCode::Tab,
            ReedlineEvent::UntilFound(vec![
                ReedlineEvent::Menu(MENU_NAME.to_string()),
                ReedlineEvent::MenuNext,
            ]),
        );
    }

    fn create_edit_mode(config: &GlobalConfig) -> Box<dyn EditMode> {
        let edit_mode: Box<dyn EditMode> = if config.read().keybindings.is_vi() {
            let mut normal_keybindings = default_vi_normal_keybindings();
            let mut insert_keybindings = default_vi_insert_keybindings();
            Self::extra_keybindings(&mut normal_keybindings);
            Self::extra_keybindings(&mut insert_keybindings);
            Box::new(Vi::new(insert_keybindings, normal_keybindings))
        } else {
            let mut keybindings = default_emacs_keybindings();
            Self::extra_keybindings(&mut keybindings);
            Box::new(Emacs::new(keybindings))
        };
        edit_mode
    }

    fn create_menu() -> ReedlineMenu {
        let completion_menu = ColumnarMenu::default().with_name(MENU_NAME);
        ReedlineMenu::EngineCompleter(Box::new(completion_menu))
    }

    fn copy(&self, text: &str) -> Result<()> {
        if text.is_empty() {
            bail!("No text")
        }
        set_text(text)?;
        Ok(())
    }
}

/// A default validator which checks for mismatched quotes and brackets
struct ReplValidator;

impl Validator for ReplValidator {
    fn validate(&self, line: &str) -> ValidationResult {
        let line = line.trim();
        if line.starts_with(r#"""""#) && !line[3..].ends_with(r#"""""#) {
            ValidationResult::Incomplete
        } else {
            ValidationResult::Complete
        }
    }
}

fn unknown_command() -> Result<()> {
    bail!(r#"Unknown command. Type ".help" for more information."#);
}

fn dump_repl_help() {
    let head = REPL_COMMANDS
        .iter()
        .map(|(name, desc)| format!("{name:<24} {desc}"))
        .collect::<Vec<String>>()
        .join("\n");
    println!(
        r###"{head}

Type """ to begin multi-line editing, type """ to end it.
Press Ctrl+C to abort aichat, Ctrl+D to exit the REPL"###,
    );
}

fn parse_command(line: &str) -> Option<(&str, Option<&str>)> {
    match COMMAND_RE.captures(line) {
        Ok(Some(captures)) => {
            let cmd = captures.get(1)?.as_str();
            let args = line[captures[0].len()..].trim();
            let args = if args.is_empty() { None } else { Some(args) };
            Some((cmd, args))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_process_command_line() {
        assert_eq!(parse_command(" ."), Some((".", None)));
        assert_eq!(parse_command(" .role"), Some((".role", None)));
        assert_eq!(parse_command(" .role  "), Some((".role", None)));
        assert_eq!(
            parse_command(" .set dry_run true"),
            Some((".set", Some("dry_run true")))
        );
        assert_eq!(
            parse_command(" .set dry_run true  "),
            Some((".set", Some("dry_run true")))
        );
        assert_eq!(
            parse_command(".prompt \nabc\n"),
            Some((".prompt", Some("abc")))
        );
    }
}
