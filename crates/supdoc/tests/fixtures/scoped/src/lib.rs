struct Config {
    host: String,
}

impl Config {
    fn validate(&self) -> bool {
        !self.host.is_empty()
    }

    fn default_host() -> String {
        "localhost".to_string()
    }
}

struct Server {
    port: u16,
}

impl Server {
    fn validate(&self) -> bool {
        self.port != 0
    }
}
