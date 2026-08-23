use oj::args::{self, Command};

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match args::parse(&argv) {
        Command::None => {
            eprintln!(
                "usage: oj <server|build> [flags]\n  oj server -c config.yaml -b /v1/api -d src --dev\n  oj build [module] -d src -o dist"
            );
            std::process::exit(2);
        }
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
