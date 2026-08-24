use oj::args::{self, Command};

#[tokio::main]
async fn main() {
    match args::parse_from(std::env::args_os()) {
        Command::Build(a) => {
            if let Err(e) = oj::build_cmd::run(&a).await {
                eprintln!("oj build: {e}");
                std::process::exit(1);
            }
        }
        Command::Server(a) => {
            if let Err(e) = oj::server_cmd::run(a).await {
                eprintln!("oj server: {e}");
                std::process::exit(1);
            }
        }
    }
}
