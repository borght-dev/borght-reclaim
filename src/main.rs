mod activity;
mod clean;
mod cli;
mod format;
mod git;
mod gui;
mod index;
mod journal;
mod plan;
mod scan;
mod snapshot;
mod territory;
mod treemap;
mod winfs;

fn main() -> std::process::ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match cli::run(&arguments) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("reclaim: {message}");
            std::process::ExitCode::FAILURE
        }
    }
}
