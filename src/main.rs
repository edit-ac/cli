use std::process::ExitCode;

const HELP: &str = "editac — local-first editor client scaffold

Usage: editac [--help | --version]

Options:
  --help       Show this help
  --version    Show the development version

No document commands are implemented.";

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        [] => println!("{HELP}"),
        [flag] if flag == "--help" => println!("{HELP}"),
        [flag] if flag == "--version" => {
            println!(
                "editac {} (development scaffold; engine {})",
                env!("CARGO_PKG_VERSION"),
                editac_cli::EDITOR_REVISION
            );
        }
        _ => {
            eprintln!("Unsupported argument. Run editac --help; no data commands are implemented.");
            return ExitCode::from(2);
        }
    }
    ExitCode::SUCCESS
}
