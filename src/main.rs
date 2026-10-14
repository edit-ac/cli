use editac_cli::diagnostics::{self, Format};
use std::{ffi::OsString, path::PathBuf, process::ExitCode};

const HELP: &str = "editac — local-first editor client

Usage: editac [--help | --version]
       editac tui <path> --store <new-absolute-db> [--create] [--format <markdown|text>]
       editac tui <ac-reference|configured-app-url> --catalog <host-granted-json> [--offline]
       editac edit <path> [--create] [--format <markdown|text>]
         [--editor <configuration>] [--recovery-dir <private-directory>]
       editac edit <hosted-url> --service-config <json> --cache-dir <private-parent>
         [--offline | --adapter <absolute-host-executable>] [--editor <configuration>]
       editac status <path>
       editac status <hosted-url> --service-config <json> --cache-dir <private-parent>
       editac sync <hosted-url> --service-config <json> --cache-dir <private-parent>
         --adapter <absolute-host-executable> [--attempts <1..5>]
       editac recover <session-directory> [--output <new-path>]
       editac validate <path> --input-format <markdown|json|update> [--base <path>]
       editac inspect <path> --input-format <markdown|json|update> [--base <path>]
       editac convert <path> --input-format <markdown|json|update>
         --output-format <markdown|json|update> --output <path> [--allow-lossy] [--overwrite]

Options:
  --help       Show this help
  --version    Show the development version

Diagnostics are secondary interchange operations. Update inputs are identified
JSON envelopes; incremental updates require an initialized identified --base.
Inspection emits safe counts/codes. Remote durability is not assessed.
Conversion requires explicit loss/overwrite consent and preserves the source.
Hosted online editing requires an explicit configured host adapter.
Offline hosted editing and status use an existing private cache without credentials.";

fn data_command(args: &[OsString]) -> Result<(), String> {
    let operation = args[0].to_str().ok_or("invalid-command")?;
    let source = PathBuf::from(args.get(1).ok_or("input-path-required")?);
    let (mut input, mut output, mut destination, mut base) = (None, None, None, None);
    let (mut allow_lossy, mut overwrite) = (false, false);
    let mut i = 2;
    while i < args.len() {
        let flag = args[i].to_str().ok_or("invalid-option")?;
        match flag {
            "--allow-lossy" if operation == "convert" && !allow_lossy => allow_lossy = true,
            "--overwrite" if operation == "convert" && !overwrite => overwrite = true,
            "--input-format" | "--output-format" | "--output" | "--base" => {
                i += 1;
                let value = args.get(i).ok_or("option-value-required")?;
                match flag {
                    "--input-format" if input.is_none() => {
                        input = Some(
                            Format::parse(value.to_str().ok_or("invalid-format")?)
                                .map_err(|e| e.to_string())?,
                        )
                    }
                    "--output-format" if operation == "convert" && output.is_none() => {
                        output = Some(
                            Format::parse(value.to_str().ok_or("invalid-format")?)
                                .map_err(|e| e.to_string())?,
                        )
                    }
                    "--output" if operation == "convert" && destination.is_none() => {
                        destination = Some(PathBuf::from(value))
                    }
                    "--base" if operation != "convert" && base.is_none() => {
                        base = Some(PathBuf::from(value))
                    }
                    _ => return Err("unexpected-or-duplicate-option".into()),
                }
            }
            _ => return Err("unexpected-or-duplicate-option".into()),
        }
        i += 1;
    }
    let input = input.ok_or("explicit-input-format-required")?;
    if operation == "convert" {
        let (_, conversion) = diagnostics::convert_file(
            &source,
            &destination.ok_or("output-path-required")?,
            input,
            output.ok_or("explicit-output-format-required")?,
            allow_lossy,
            overwrite,
        )
        .map_err(|e| e.to_string())?;
        println!(
            "{}",
            serde_json::json!({"outputBytes":conversion.bytes.len(),"lossy":conversion.lossy,"diagnostics":conversion.diagnostics,"durability":"locally-saved"})
        );
    } else {
        let bytes = diagnostics::read(&source).map_err(|e| e.to_string())?;
        let base = base
            .as_deref()
            .map(diagnostics::read)
            .transpose()
            .map_err(|e| e.to_string())?;
        let report =
            diagnostics::validate(&bytes, input, base.as_deref()).map_err(|e| e.to_string())?;
        println!(
            "{}",
            serde_json::to_string(&report).map_err(|_| "report-serialization-failed")?
        );
    }
    Ok(())
}
fn local_command(args: &[OsString]) -> Result<(), String> {
    let path = PathBuf::from(args.get(1).ok_or("path-required")?);
    let mut options = editac_cli::target::ResolveOptions::default();
    let (mut editor, mut recovery, mut output) = (None, None, None);
    let mut seen = std::collections::HashSet::new();
    let mut i = 2;
    while i < args.len() {
        let flag = args[i].to_str().ok_or("invalid-option")?;
        if !seen.insert(flag) {
            return Err("duplicate-option".into());
        }
        if flag == "--create" && args[0] == "edit" {
            options.create = true;
            i += 1;
            continue;
        }
        i += 1;
        let value = args.get(i).ok_or("option-value-required")?;
        match flag {
            "--format" if args[0] == "edit" => {
                options.format = Some(
                    editac_cli::target::Format::parse(value.to_str().ok_or("invalid-format")?)
                        .map_err(|e| e.to_string())?,
                )
            }
            "--editor" if args[0] == "edit" => {
                editor = Some(value.to_str().ok_or("invalid-editor")?)
            }
            "--recovery-dir" if args[0] == "edit" => recovery = Some(PathBuf::from(value)),
            "--output" if args[0] == "recover" => output = Some(PathBuf::from(value)),
            _ => return Err("unexpected-option".into()),
        }
        i += 1;
    }
    let result = if args[0] == "edit" {
        editac_cli::local::edit(&path, options, editor, recovery.as_deref())?
    } else {
        editac_cli::local::recover(&path, output.as_deref())?
    };
    println!("{result}");
    Ok(())
}
fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        [] => println!("{HELP}"),
        [flag] if flag == "--help" => println!("{HELP}"),
        [flag] if flag == "--version" => println!(
            "editac {} (development scaffold; engine {})",
            env!("CARGO_PKG_VERSION"),
            editac_cli::EDITOR_REVISION
        ),
        [command, ..] if command == "tui" => {
            let result =
                editac_cli::tui::session::Request::parse(&args).and_then(|request| request.open());
            match result {
                Ok(session) => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(u64::MAX, |d| d.as_secs());
                    let state = session.service.session().state(now);
                    let capabilities = session.service.capabilities(now);
                    println!(
                        "{}",
                        serde_json::json!({"phase":"session-acquired", "presentation":"terminal loop pending", "sourceBytes": session.source.len(), "representation":format!("{:?}",session.representation), "identity":editac_cli::storage::StoredIdentity::from(session.service.session().document().identity()), "canPrepare":capabilities.can_prepare, "canApply":capabilities.can_apply, "canHistory":capabilities.can_history, "permission":format!("{:?}",state.permission)})
                    );
                }
                Err(error) => {
                    eprintln!("{error}. Run editac --help.");
                    return ExitCode::from(2);
                }
            }
        }
        [command, ..] if matches!(command.to_str(), Some("sync" | "status")) => {
            match editac_cli::client_commands::execute(&args) {
                Ok(report) => println!("{report}"),
                Err(error) => {
                    eprintln!("{error}. Run editac --help.");
                    return ExitCode::from(2);
                }
            }
        }
        [command, target, ..]
            if command == "edit" && target.to_str().is_some_and(|s| s.starts_with("https://")) =>
        {
            match editac_cli::client_commands::execute(&args) {
                Ok(report) => println!("{report}"),
                Err(error) => {
                    eprintln!("{error}. Run editac --help.");
                    return ExitCode::from(2);
                }
            }
        }
        [command, ..] if matches!(command.to_str(), Some("edit" | "recover")) => {
            if let Err(error) = local_command(&args) {
                eprintln!("{error}. Run editac --help.");
                return ExitCode::from(2);
            }
        }
        [command, ..] if matches!(command.to_str(), Some("validate" | "inspect" | "convert")) => {
            if let Err(error) = data_command(&args) {
                eprintln!("{error}. Run editac --help.");
                return ExitCode::from(2);
            }
        }
        _ => {
            eprintln!("Unsupported argument. Run editac --help.");
            return ExitCode::from(2);
        }
    }
    ExitCode::SUCCESS
}
