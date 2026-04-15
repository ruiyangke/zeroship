mod config;
mod http;
mod numa;
mod stats;

use config::Config;

fn main() {
    let config = Config::from_args();
    println!("zerobench v0.1.0");
    println!("  URL:         {}", config.url);
    println!("  Threads:     {}", config.threads);
    println!("  Connections: {}", config.connections);
    println!("  Duration:    {:?}", config.duration);
}
