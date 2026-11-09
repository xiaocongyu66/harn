#[tokio::main]
async fn main() {
    if let Err(error) = harn_cli::run().await {
        eprintln!("[harn] {error:#}");
        std::process::exit(1);
    }
}
