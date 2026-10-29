interface ServerConfig {
    host: string;
    port: number;
}

class App {
    name: string;
    render(): void {}
}

enum Status {
    Active = "active",
    Inactive = "inactive",
}

enum Direction {
    Up,
    Down,
}
