mod http_proxy_listener;

#[tokio::main]
async fn main() {
    let config = match tokio::fs::read_to_string("proxymix.toml").await {
        Err(error) => {
            println!(
                "** Error: Could not open config file (proxymix.toml): {}",
                error
            );
            return;
        }
        Ok(config) => match toml::from_str::<http_proxy_listener::Config>(&config) {
            Err(error) => {
                println!(
                    "** Error: Could parse open config file (proxymix.toml): {}",
                    error
                );
                return;
            }
            Ok(config) => config,
        },
    };

    http_proxy_listener::listen(config.port, config).await;
}
