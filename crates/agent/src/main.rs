//! Binary entry point; all logic lives in the `remote_agent` library crate.

fn main() {
    let code = match remote_agent::cli::run() {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("error: {err:#}");
            tracing::error!("fatal: {err:#}");
            1
        }
    };
    std::process::exit(code);
}
