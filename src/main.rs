use clap::Parser;
use jj_vine::cli::Cli;

#[tokio::main]
async fn main() {
    if let Err(error) = Cli::parse().run_stdout().await {
        eprintln!("error: {error}");
        if std::env::var_os("RUST_BACKTRACE").is_some() {
            eprintln!("{error:?}");
        }
        std::process::exit(1);
    }
}
