/// Benchmark configuration parsed from CLI arguments.
pub struct Config {
    pub url: String,
    pub host: String,
    pub port: u16,
    pub path: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub threads: usize,
    pub connections: usize,
    pub duration: std::time::Duration,
    pub timeout: std::time::Duration,
    pub print_latency: bool,
    pub json_output: bool,
    pub numa_node: Option<usize>,
    pub cpu_affinity: Option<Vec<usize>>,
}

impl Config {
    pub fn from_args() -> Self {
        let args: Vec<String> = std::env::args().collect();
        let mut config = Config {
            url: String::new(),
            host: String::new(),
            port: 80,
            path: "/".to_string(),
            method: "GET".to_string(),
            headers: Vec::new(),
            body: None,
            threads: num_cpus(),
            connections: 100,
            duration: std::time::Duration::from_secs(10),
            timeout: std::time::Duration::from_secs(2),
            print_latency: false,
            json_output: false,
            numa_node: None,
            cpu_affinity: None,
        };

        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "-t" | "--threads" => { i += 1; config.threads = args[i].parse().unwrap_or(config.threads); }
                "-c" | "--connections" => { i += 1; config.connections = args[i].parse().unwrap_or(config.connections); }
                "-d" | "--duration" => { i += 1; config.duration = parse_duration(&args[i]); }
                "-H" | "--header" => { i += 1; if let Some((k, v)) = args[i].split_once(": ") { config.headers.push((k.to_string(), v.to_string())); } }
                "--body" => { i += 1; config.body = Some(args[i].clone()); config.method = "POST".to_string(); }
                "--method" => { i += 1; config.method = args[i].clone(); }
                "--timeout" => { i += 1; config.timeout = parse_duration(&args[i]); }
                "--latency" => { config.print_latency = true; }
                "--json" => { config.json_output = true; }
                "--numa" => { i += 1; config.numa_node = args[i].parse().ok(); }
                "--cpu" => { i += 1; config.cpu_affinity = Some(parse_cpu_range(&args[i])); }
                arg if !arg.starts_with('-') => {
                    config.url = arg.to_string();
                    let url_clone = config.url.clone();
                    parse_url(&url_clone, &mut config);
                }
                _ => {}
            }
            i += 1;
        }

        if config.url.is_empty() {
            eprintln!("Usage: zerobench [OPTIONS] <URL>");
            std::process::exit(1);
        }

        config
    }

    /// Connections per thread (evenly distributed).
    pub fn connections_per_thread(&self) -> usize {
        (self.connections + self.threads - 1) / self.threads
    }
}

fn parse_url(url: &str, config: &mut Config) {
    let without_scheme = url.strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);
    let (host_port, path) = without_scheme.split_once('/').unwrap_or((without_scheme, ""));
    config.path = format!("/{path}");
    if let Some((h, p)) = host_port.split_once(':') {
        config.host = h.to_string();
        config.port = p.parse().unwrap_or(80);
    } else {
        config.host = host_port.to_string();
        config.port = if url.starts_with("https") { 443 } else { 80 };
    }
}

fn parse_duration(s: &str) -> std::time::Duration {
    if let Some(secs) = s.strip_suffix('s') {
        std::time::Duration::from_secs(secs.parse().unwrap_or(10))
    } else if let Some(mins) = s.strip_suffix('m') {
        std::time::Duration::from_secs(mins.parse::<u64>().unwrap_or(1) * 60)
    } else {
        std::time::Duration::from_secs(s.parse().unwrap_or(10))
    }
}

fn parse_cpu_range(s: &str) -> Vec<usize> {
    let mut cpus = Vec::new();
    for part in s.split(',') {
        if let Some((start, end)) = part.split_once('-') {
            let s: usize = start.parse().unwrap_or(0);
            let e: usize = end.parse().unwrap_or(s);
            cpus.extend(s..=e);
        } else if let Ok(n) = part.parse() {
            cpus.push(n);
        }
    }
    cpus
}

fn num_cpus() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}
