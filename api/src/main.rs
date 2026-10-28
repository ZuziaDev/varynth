use varynth_api::Config;

#[tokio::main]
async fn main() {
    let _ = dotenvy::from_filename(concat!(env!("CARGO_MANIFEST_DIR"), "/.env"));
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "varynth_api=info".into()),
        )
        .init();
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("!! {error}");
            std::process::exit(1);
        }
    };
    if let Err(error) = varynth_api::serve(config).await {
        eprintln!("!! {error}");
        std::process::exit(1);
    }
}
