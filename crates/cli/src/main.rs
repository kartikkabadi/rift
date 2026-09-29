use clap::{Parser, Subcommand, ValueEnum};
use rift::{
    Backend, CopyMode, CowMode, Create, CreateOptions, HookMode, InitProgress, Manager,
    RemoveOptions,
};
use std::io::Read;
use std::path::PathBuf;
use thiserror::Error;

type Result<T> = std::result::Result<T, CliError>;

#[derive(Debug, Error)]
enum CliError {
    #[error(transparent)]
    Rift(#[from] rift::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(
        "This is the root workspace.\n\nUnregistering it removes Rift metadata and trashes all child rifts.\nRun `rift remove -f` to continue."
    )]
    ForceRequired,
}

#[derive(Parser)]
#[command(name = "rift")]
struct Cli {
    #[arg(long, hide = true)]
    database: Option<PathBuf>,
    #[arg(long, hide = true, global = true)]
    shell_cwd: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum Shell {
    Bash,
    Zsh,
    Nushell,
    Fish,
    Powershell,
}

impl Shell {
    fn init_script(self, executable: &str) -> String {
        match self {
            Shell::Bash | Shell::Zsh => {
                let executable = posix_shell_quote(executable);
                format!(
                    r#"rift() {{
  case "${{1-}}" in
    init|create|remove)
      local __rift_cwd __rift_status
      __rift_cwd="$({executable} --shell-cwd "$@")"
      __rift_status=$?
      if [ -n "$__rift_cwd" ]; then
        builtin cd -- "$__rift_cwd" || return $?
      fi
      return $__rift_status
      ;;
    *)
      {executable} "$@"
      ;;
  esac
}}"#,
                )
            }
            Shell::Nushell => {
                let executable = nushell_shell_quote(executable);
                format!(
                    r#"def --env --wrapped rift [...rest] {{
  match ($rest | get 0? | default "" | into string) {{
    "init" | "create" | "remove" => {{
      let cwd = (^{executable} --shell-cwd ...$rest | str trim)
      if ($cwd | is-not-empty) {{
        cd $cwd
      }}
    }}
    _ => {{
      ^{executable} ...$rest
    }}
  }}
}}"#,
                )
            }
            Shell::Fish => {
                let executable = fish_shell_quote(executable);
                format!(
                    r#"function rift
  if test (count $argv) -gt 0; and contains -- $argv[1] init create remove
    set -l __rift_cwd ({executable} --shell-cwd $argv | string collect)
    set -l __rift_status $status
    if test -n "$__rift_cwd"
      builtin cd -- "$__rift_cwd"; or return $status
    end
    return $__rift_status
  end
  {executable} $argv
end"#,
                )
            }
            Shell::Powershell => {
                let executable = powershell_shell_quote(executable);
                format!(
                    r#"function rift {{
  $command = if ($args.Count -gt 0) {{ [string]$args[0] }} else {{ "" }}
  if ($command -in 'init', 'create', 'remove') {{
    $output = & {executable} --shell-cwd @args | Out-String
    $status = $LASTEXITCODE
    $cwd = $output.Trim()
    if ($cwd) {{ Set-Location -LiteralPath $cwd }}
    $global:LASTEXITCODE = $status
    return
  }}
  & {executable} @args
}}"#,
                )
            }
        }
    }
}

#[derive(Subcommand)]
enum Command {
    #[command(hide = true)]
    Rpc,
    ShellInit {
        #[arg(value_enum)]
        shell: Shell,
    },
    Init {
        at: Option<PathBuf>,
        #[arg(long)]
        here: bool,
        /// Fail instead of falling back to a regular copy when the filesystem
        /// cannot copy-on-write.
        #[arg(long)]
        cow_only: bool,
    },
    Create {
        from: Option<PathBuf>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        into: Option<PathBuf>,
        #[arg(long)]
        copy_all: bool,
        #[arg(long)]
        no_hooks: bool,
        /// Fail instead of falling back to a regular copy when the filesystem
        /// cannot copy-on-write.
        #[arg(long)]
        cow_only: bool,
    },
    /// Report what this machine supports: filesystem, copy method, and
    /// whether new rifts will be instant or regular copies.
    Doctor {
        of: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    Remove {
        at: Option<PathBuf>,
        #[arg(long)]
        children: bool,
        #[arg(short = 'f', long)]
        force: bool,
        #[arg(long)]
        no_hooks: bool,
    },
    List {
        of: Option<PathBuf>,
    },
    Ancestors {
        of: Option<PathBuf>,
    },
    Gc,
}

fn main() {
    if let Err(error) = run() {
        let message = match &error {
            CliError::Rift(error) => error_message(error),
            _ => error.to_string(),
        };
        eprintln!("{message}");
        std::process::exit(1);
    }
}

fn error_message(error: &rift::Error) -> String {
    match error {
        rift::Error::InitializationRequired(_) => {
            "this workspace must be initialized first; run `rift init` from its root folder".into()
        }
        rift::Error::WorkspaceNotInitialized(_) => {
            "no initialized workspace found; run `rift init` from the root folder".into()
        }
        rift::Error::MissingMarker(_) => {
            "this workspace is missing its `.rift` marker; run `rift init` to restore it".into()
        }
        _ => error.to_string(),
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let command = match cli.command {
        Command::Rpc => {
            const LIMIT: u64 = 1024 * 1024;
            let mut input = Vec::new();
            std::io::stdin().take(LIMIT + 1).read_to_end(&mut input)?;
            let output = if input.len() as u64 > LIMIT {
                rift::rpc::error("invalid_request", "RPC request exceeds 1 MiB")
            } else {
                match String::from_utf8(input) {
                    Ok(input) => rift::rpc::call(&input),
                    Err(error) => rift::rpc::error("invalid_request", error.to_string()),
                }
            };
            print!("{output}");
            return Ok(());
        }
        Command::ShellInit { shell } => {
            print_shell_init(shell);
            return Ok(());
        }
        command => command,
    };
    let mut manager = match cli.database {
        Some(path) => Manager::open(path)?,
        None => Manager::open_default()?,
    };
    match command {
        Command::Rpc => unreachable!(),
        Command::ShellInit { shell } => {
            print_shell_init(shell);
            Ok(())
        }
        Command::Init { at, here, cow_only } => {
            let requested = std::fs::canonicalize(at.unwrap_or(std::env::current_dir()?))?;
            let (at, existing, missing_marker) = init_target(&manager, &requested, here)?;
            let initialized_from_inside = std::env::current_dir()?.starts_with(&at);
            let mut converting = false;
            let outcome =
                manager.init_with_cow_mode(&at, cow_mode(cow_only), |progress| match progress {
                    InitProgress::CreatingSubvolume => {
                        converting = true;
                        eprintln!("Initializing  {}\n", at.display());
                        eprintln!("First-time setup can take a moment.");
                        eprintln!("New rifts will be instant.\n");
                        eprintln!("Creating BTRFS subvolume...");
                    }
                    InitProgress::ImportingWorkspace => eprintln!("Importing workspace..."),
                    InitProgress::ImportedEntries { .. } => {}
                    InitProgress::ActivatingWorkspace
                    | InitProgress::RegisteringWorkspace
                    | InitProgress::RestoringMarker
                    | InitProgress::RemovingOriginal => {}
                })?;
            if outcome.is_converted() {
                if converting {
                    eprintln!("\nReady  {}", at.display());
                } else {
                    eprintln!("Ready  {}", at.display());
                }
                if initialized_from_inside {
                    if cli.shell_cwd {
                        println!("{}", at.display());
                    } else {
                        eprintln!(
                            "run `cd {}` to enter the initialized workspace",
                            at.display()
                        );
                    }
                }
            } else if let Some(root) = missing_marker {
                eprintln!("Restored marker  {}", root.display());
            } else if let Some(existing) = existing {
                eprintln!("Already initialized  {}", existing.display());
            } else {
                eprintln!("Ready  {}", at.display());
            }
            if outcome.is_degraded() {
                eprintln!(
                    "note: this filesystem cannot make instant copies; new rifts will be regular copies (slower, same result)"
                );
            }
            Ok(())
        }
        Command::Create {
            from,
            name,
            into,
            copy_all,
            no_hooks,
            cow_only,
        } => {
            let destination = manager.create_with_options(
                Create::new(from.unwrap_or(std::env::current_dir()?))
                    .with_name(name)
                    .with_storage(into),
                CreateOptions::default()
                    .copy_mode(if copy_all {
                        CopyMode::All
                    } else {
                        CopyMode::Filtered
                    })
                    .hook_mode(if no_hooks {
                        HookMode::Skip
                    } else {
                        HookMode::Run
                    })
                    .cow_mode(cow_mode(cow_only)),
            )?;
            if cli.shell_cwd {
                eprintln!("created {}", destination.display());
            }
            println!("{}", destination.display());
            Ok(())
        }
        Command::Remove {
            at,
            children,
            force,
            no_hooks,
        } => {
            let at = manager.workspace(at.unwrap_or(std::env::current_dir()?))?;
            let cwd = std::fs::canonicalize(std::env::current_dir()?)?;
            if children {
                let result = manager.remove_all_with_options(
                    &at,
                    RemoveOptions::default().hook_mode(if no_hooks {
                        HookMode::Skip
                    } else {
                        HookMode::Run
                    }),
                );
                if let Ok(removed) = &result {
                    for path in removed {
                        if cli.shell_cwd {
                            eprintln!("removed {}", path.display());
                        } else {
                            println!("{}", path.display());
                        }
                    }
                }
                // Children are trashed before postremove runs, so the shell
                // must leave a removed child even when the hook fails.
                if cli.shell_cwd && !cwd.exists() {
                    println!("{}", at.display());
                }
                result?;
            } else {
                let ancestors = manager.ancestors(&at)?;
                let unregistering_root = ancestors.is_empty();
                require_force_for_root(unregistering_root, force)?;
                let destination = if cli.shell_cwd && cwd.starts_with(&at) {
                    if unregistering_root {
                        Some(at.clone())
                    } else {
                        ancestors.into_iter().next()
                    }
                } else {
                    None
                };
                let result = manager.remove_with_options(
                    &at,
                    RemoveOptions::default().hook_mode(if no_hooks {
                        HookMode::Skip
                    } else {
                        HookMode::Run
                    }),
                );
                if result.is_ok() {
                    if unregistering_root {
                        eprintln!("Unregistered  {}", at.display());
                    } else if cli.shell_cwd {
                        eprintln!("removed {}", at.display());
                    }
                }
                // A child is trashed before postremove runs, so the shell must
                // leave it even when the hook fails. A root stays on disk after
                // unregistering, hence the success check.
                if let Some(destination) = destination
                    && (result.is_ok() || !at.exists())
                {
                    println!("{}", destination.display());
                }
                result?;
            }
            Ok(())
        }
        Command::List { of } => {
            for path in manager.list(of.unwrap_or(std::env::current_dir()?))? {
                println!("{}", path.display());
            }
            Ok(())
        }
        Command::Ancestors { of } => {
            for path in manager.ancestors(of.unwrap_or(std::env::current_dir()?))? {
                println!("{}", path.display());
            }
            Ok(())
        }
        Command::Doctor { of, json } => {
            let probe = manager.probe(of.unwrap_or(std::env::current_dir()?))?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&probe).unwrap_or_else(|_| "{}".into())
                );
            } else {
                let filesystem = probe.filesystem.as_deref().unwrap_or("unknown");
                let method = match probe.backend {
                    Backend::Btrfs => "instant copies (btrfs snapshots)",
                    Backend::Reflink => "instant copies (Linux reflinks)",
                    Backend::Apfs => "instant copies (APFS clonefile)",
                    Backend::ReFs => "instant copies (ReFS block cloning)",
                    Backend::Portable => {
                        "regular copies only (no instant-copy support on this filesystem)"
                    }
                };
                println!("{}", probe.path.display());
                println!("filesystem: {filesystem}");
                println!("copy method: {method}");
                if matches!(probe.backend, Backend::Portable) {
                    println!("tip: `rift init` still works; new rifts will just take longer");
                }
            }
            Ok(())
        }
        Command::Gc => {
            for path in manager.gc()? {
                println!("{}", path.display());
            }
            Ok(())
        }
    }
}

fn cow_mode(cow_only: bool) -> CowMode {
    if cow_only {
        CowMode::Require
    } else {
        CowMode::Auto
    }
}

fn init_target(
    manager: &Manager,
    requested: &std::path::Path,
    here: bool,
) -> Result<(PathBuf, Option<PathBuf>, Option<PathBuf>)> {
    if here {
        return Ok((requested.to_path_buf(), None, None));
    }
    match manager.workspace(requested) {
        Ok(root) => Ok((root.clone(), Some(root), None)),
        Err(rift::Error::MissingMarker(root)) => Ok((root.clone(), None, Some(root))),
        Err(rift::Error::WorkspaceNotInitialized(_)) => Ok((git_root(requested), None, None)),
        Err(error) => Err(error.into()),
    }
}

fn git_root(path: &std::path::Path) -> PathBuf {
    path.ancestors()
        .find(|directory| directory.join(".git").exists())
        .unwrap_or(path)
        .to_path_buf()
}

fn require_force_for_root(unregistering_root: bool, force: bool) -> Result<()> {
    if unregistering_root && !force {
        return Err(CliError::ForceRequired);
    }
    Ok(())
}

fn print_shell_init(shell: Shell) {
    let executable = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rift"));
    println!("{}", shell.init_script(&executable.to_string_lossy()));
}

fn posix_shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn nushell_shell_quote(value: &str) -> String {
    let mut hashes = String::from("#");
    while value.contains(&format!("'{}", hashes)) {
        hashes.push('#');
    }
    format!("r{}'{}'{}", hashes, value, hashes)
}

fn fish_shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn powershell_shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn initialization_guidance_is_rendered_by_the_cli() {
        let path = PathBuf::from("/tmp/app");

        assert_eq!(
            error_message(&rift::Error::WorkspaceNotInitialized(path.clone())),
            "no initialized workspace found; run `rift init` from the root folder"
        );
        assert_eq!(
            error_message(&rift::Error::MissingMarker(path)),
            "this workspace is missing its `.rift` marker; run `rift init` to restore it"
        );
    }

    #[test]
    fn init_target_selects_git_root_unless_here_is_requested() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("app");
        let nested = root.join("nested");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir(&nested).unwrap();
        let manager = Manager::open(temp.path().join("rift.sqlite")).unwrap();

        assert_eq!(init_target(&manager, &nested, false).unwrap().0, root);
        assert_eq!(init_target(&manager, &nested, true).unwrap().0, nested);
    }

    #[test]
    fn root_unregistration_requires_force() {
        assert!(matches!(
            require_force_for_root(true, false),
            Err(CliError::ForceRequired)
        ));
        assert!(require_force_for_root(true, true).is_ok());
        assert!(require_force_for_root(false, false).is_ok());
        assert_eq!(
            CliError::ForceRequired.to_string(),
            "This is the root workspace.\n\nUnregistering it removes Rift metadata and trashes all child rifts.\nRun `rift remove -f` to continue."
        );
    }

    #[test]
    fn create_command_accepts_copy_and_hook_flags() {
        let cli = Cli::try_parse_from([
            "rift",
            "create",
            "--name",
            "child",
            "--copy-all",
            "--no-hooks",
            "--cow-only",
        ])
        .unwrap();

        assert!(matches!(
            cli.command,
            Command::Create {
                copy_all: true,
                no_hooks: true,
                cow_only: true,
                ..
            }
        ));
    }

    #[test]
    fn init_and_doctor_accept_strict_and_json_flags() {
        let init = Cli::try_parse_from(["rift", "init", "--here", "--cow-only"]).unwrap();
        let doctor = Cli::try_parse_from(["rift", "doctor", "--json"]).unwrap();

        assert!(matches!(
            init.command,
            Command::Init {
                here: true,
                cow_only: true,
                ..
            }
        ));
        assert!(matches!(doctor.command, Command::Doctor { json: true, .. }));
    }

    #[test]
    fn remove_command_accepts_no_hooks() {
        let cli = Cli::try_parse_from(["rift", "remove", "--no-hooks"]).unwrap();

        assert!(matches!(
            cli.command,
            Command::Remove { no_hooks: true, .. }
        ));
    }

    #[test]
    fn shell_init_renders_posix_wrapper_for_bash_and_zsh() {
        let wrapper = r#"rift() {
  case "${1-}" in
    init|create|remove)
      local __rift_cwd __rift_status
      __rift_cwd="$('/tmp/rift' --shell-cwd "$@")"
      __rift_status=$?
      if [ -n "$__rift_cwd" ]; then
        builtin cd -- "$__rift_cwd" || return $?
      fi
      return $__rift_status
      ;;
    *)
      '/tmp/rift' "$@"
      ;;
  esac
}"#;

        assert_eq!(Shell::Bash.init_script("/tmp/rift"), wrapper);
        assert_eq!(Shell::Zsh.init_script("/tmp/rift"), wrapper);
    }

    #[test]
    fn shell_init_renders_nushell_wrapper() {
        let wrapper = r#"def --env --wrapped rift [...rest] {
  match ($rest | get 0? | default "" | into string) {
    "init" | "create" | "remove" => {
      let cwd = (^r#'/tmp/rift'# --shell-cwd ...$rest | str trim)
      if ($cwd | is-not-empty) {
        cd $cwd
      }
    }
    _ => {
      ^r#'/tmp/rift'# ...$rest
    }
  }
}"#;

        assert_eq!(Shell::Nushell.init_script("/tmp/rift"), wrapper);
    }

    #[test]
    fn nushell_shell_quote_uses_enough_raw_string_hashes() {
        assert_eq!(nushell_shell_quote("/tmp/rift"), "r#'/tmp/rift'#");
        assert_eq!(
            nushell_shell_quote("/tmp/it's'#rift"),
            "r##'/tmp/it's'#rift'##"
        );
    }

    #[test]
    fn shell_init_renders_fish_and_powershell_wrappers() {
        let fish = Shell::Fish.init_script("/tmp/rift");
        let powershell = Shell::Powershell.init_script("/tmp/rift");

        assert!(fish.contains("function rift"));
        assert!(fish.contains("--shell-cwd $argv"));
        assert!(fish.contains("builtin cd"));
        assert!(powershell.contains("function rift"));
        assert!(powershell.contains("--shell-cwd @args"));
        assert!(powershell.contains("Set-Location"));
    }

    #[test]
    fn fish_and_powershell_quotes_escape_quotes() {
        assert_eq!(fish_shell_quote("/tmp/it's rift"), "'/tmp/it\\'s rift'");
        assert_eq!(
            powershell_shell_quote("/tmp/it's rift"),
            "'/tmp/it''s rift'"
        );
    }
}
