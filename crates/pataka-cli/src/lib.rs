//! The `pataka` command: Claude Code with Tūmataiti in the middle, and
//! Pātaka's lists, name model and audit log from the terminal.

#![forbid(unsafe_code)]

mod claude;
mod config;
mod ctx;
mod hooks;
mod mail;
mod service;
mod setup;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "pataka",
    version,
    about = "Pātaka: Claude Code with Tūmataiti redaction in the middle. Only redacted text leaves your computer.",
    after_help = "Start with `pataka setup`, then run `pataka claude` in a project folder."
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// First run: protection, the name model, Claude Code hooks, and a redaction check.
    Setup,
    /// Run Claude Code through Pātaka's proxy. Every argument after `claude` goes to Claude Code.
    #[command(disable_help_flag = true)]
    Claude {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run only the proxy for this folder, and print the address to give Claude Code.
    Proxy,
    /// The always-on proxy for every Claude Code session started from a terminal.
    Service {
        #[command(subcommand)]
        action: ServiceCmd,
    },
    /// Run the service in the foreground (what `pataka service install` starts at login).
    Serve,
    /// Unlock the running service with your passphrase (once after each login).
    Unlock,
    /// Lock the running service: Claude Code cannot reach the model until you unlock it.
    Lock,
    /// Kaitiaki lists: names, places, affiliation, health and tapu terms.
    Lists {
        #[command(subcommand)]
        action: ListsCmd,
    },
    /// The Tūmataiti name model.
    Model {
        #[command(subcommand)]
        action: ModelCmd,
    },
    /// The hash-chained audit log.
    Audit {
        #[command(subcommand)]
        action: AuditCmd,
    },
    /// Claude Code hooks in ~/.claude/settings.json.
    Hooks {
        #[command(subcommand)]
        action: HooksCmd,
    },
    /// Pātaka Mail: let Claude Code read and organise your mailbox over IMAP.
    Mail {
        #[command(subcommand)]
        action: MailCmd,
    },
    /// What is set up.
    Status,
    /// Called by Claude Code hooks.
    #[command(hide = true)]
    Hook { which: String },
    /// Run a command with Pātaka's placeholders in its arguments put back to
    /// the real values, inside a Claude Code session run through Pātaka. The
    /// command is checked as written, with placeholders; only the process
    /// that runs sees the real values.
    #[command(disable_help_flag = true)]
    Run {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// Start the service at login and point every Claude Code session at it (asks first).
    Install {
        /// Local port to listen on (default 47613).
        #[arg(long)]
        port: Option<u16>,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Stop the service and put Claude Code's settings back.
    Uninstall,
    /// Is it running, unlocked, and is Claude Code pointed at it?
    Status,
    Start,
    Stop,
}

#[derive(Subcommand)]
enum ListsCmd {
    /// Show every list.
    Show,
    /// A person's name, e.g. "Hēmi Te Rangi".
    AddPerson { term: String },
    /// A place or address.
    AddPlace { term: String },
    /// An iwi, hapū, marae or whānau name.
    AddAffiliation { term: String },
    /// A health term.
    AddHealth { term: String },
    /// A tapu term: a request containing it is never sent.
    AddTapu { term: String },
    /// A name that may be sent unchanged, e.g. a tupuna discussed in research.
    AddAllow { term: String },
    /// Remove a term from every list.
    Remove { term: String },
}

#[derive(Subcommand)]
enum ModelCmd {
    Status,
    /// Download the model and ONNX Runtime, checking each against a pinned SHA-256.
    Install,
    Remove,
}

#[derive(Subcommand)]
enum AuditCmd {
    /// Check the hash chain.
    Verify,
    /// Show recent entries (counts and destinations only, never text).
    Show {
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },
}

#[derive(Subcommand)]
enum MailCmd {
    /// Connect a mailbox: asks for the address and an IMAP app password, and tests the login.
    Connect {
        /// IMAP server, `host` or `host:port` (default imap.gmail.com:993, TLS).
        #[arg(long)]
        host: Option<String>,
        /// The email address (asked for if not given).
        #[arg(long)]
        email: Option<String>,
    },
    /// Forget the stored account.
    Disconnect,
    /// Is a mailbox connected?
    Status,
    /// Reverse one change Claude made, by its audit entry number.
    Undo { entry: u64 },
    /// Offer pataka-mail to every Claude Code (terminal and the Claude desktop app), through the service.
    Register {
        /// Leave claude.ai connectors (such as Gmail) switched on.
        #[arg(long)]
        keep_connectors: bool,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Stop offering pataka-mail to every Claude Code (`pataka claude` still offers it).
    Unregister,
}

#[derive(Subcommand)]
enum HooksCmd {
    /// Add the hooks (asks first).
    Install {
        /// Do not add the tool guard, only the prompt check.
        #[arg(long)]
        no_tool_guard: bool,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    Uninstall,
    Status,
}

/// Run the command line; returns the exit code.
pub fn run() -> i32 {
    let cli = Cli::parse();
    use config::ListKind::*;
    let result = match cli.command {
        Cmd::Setup => setup::setup(),
        Cmd::Claude { args } => claude::claude(args),
        Cmd::Proxy => claude::proxy(),
        Cmd::Service { action } => match action {
            ServiceCmd::Install { port, yes } => service::install(port, yes),
            ServiceCmd::Uninstall => service::uninstall(),
            ServiceCmd::Status => service::status(),
            ServiceCmd::Start => service::start_stop(true),
            ServiceCmd::Stop => service::start_stop(false),
        },
        Cmd::Serve => service::serve(),
        Cmd::Unlock => service::unlock(),
        Cmd::Lock => service::lock(),
        Cmd::Lists { action } => match action {
            ListsCmd::Show => config::lists_show(),
            ListsCmd::AddPerson { term } => config::lists_add(Person, &term),
            ListsCmd::AddPlace { term } => config::lists_add(Place, &term),
            ListsCmd::AddAffiliation { term } => config::lists_add(Affiliation, &term),
            ListsCmd::AddHealth { term } => config::lists_add(Health, &term),
            ListsCmd::AddTapu { term } => config::lists_add(Tapu, &term),
            ListsCmd::AddAllow { term } => config::lists_add(Allow, &term),
            ListsCmd::Remove { term } => config::lists_remove(&term),
        },
        Cmd::Model { action } => match action {
            ModelCmd::Status => config::model_status(),
            ModelCmd::Install => config::model_install(),
            ModelCmd::Remove => config::model_remove(),
        },
        Cmd::Audit { action } => match action {
            AuditCmd::Verify => config::audit_verify(),
            AuditCmd::Show { limit } => config::audit_show(limit),
        },
        Cmd::Hooks { action } => match action {
            HooksCmd::Install { no_tool_guard, yes } => hooks::install(!no_tool_guard, yes).map(|_| 0),
            HooksCmd::Uninstall => hooks::uninstall().map(|_| 0),
            HooksCmd::Status => hooks::installed().map(|(p, t)| {
                println!(
                    "Prompt check: {}\nTool guard:   {}",
                    if p { "on" } else { "off" },
                    if t { "on" } else { "off" }
                );
                0
            }),
        },
        Cmd::Mail { action } => match action {
            MailCmd::Connect { host, email } => mail::connect(host, email),
            MailCmd::Disconnect => mail::disconnect(),
            MailCmd::Status => mail::status(),
            MailCmd::Undo { entry } => mail::undo(entry),
            MailCmd::Register { keep_connectors, yes } => service::mail_register(true, !keep_connectors, yes),
            MailCmd::Unregister => service::mail_register(false, false, true),
        },
        Cmd::Status => config::status(),
        Cmd::Hook { which } => match which.as_str() {
            "prompt" => hooks::hook_prompt(),
            "pretool" => hooks::hook_pretool(),
            _ => Ok(0),
        },
        Cmd::Run { command } => hooks::run(command),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("pataka: {e}");
            1
        }
    }
}
