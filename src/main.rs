use std::process::ExitCode;

const HELP: &str = "editac — development scaffold

Usage: editac [--help | --version]

No document commands are implemented. Offline fixture and diagnostic commands
are deferred until the shared document schema and fixture contracts are stable.
Roadmaps: https://github.com/edit-ac/architecture";

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        [] => println!("{HELP}"),
        [flag] if flag == "--help" => println!("{HELP}"),
        [flag] if flag == "--version" => {
            println!(
                "editac {} (development scaffold)",
                env!("CARGO_PKG_VERSION")
            );
        }
        _ => {
            eprintln!("Unsupported argument. Run editac --help; no data commands are implemented.");
            return ExitCode::from(2);
        }
    }
    ExitCode::SUCCESS
}
