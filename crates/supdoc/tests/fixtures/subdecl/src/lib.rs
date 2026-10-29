struct Config {
    host: String,
    port: u16,
}

enum Message {
    Quit,
    Send { payload: Vec<u8> },
    Echo(String),
}

trait Handler {
    fn handle(&self, msg: &Message);
    fn name(&self) -> &str { "default" }
}
