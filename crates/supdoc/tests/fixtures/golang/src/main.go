package main

const MaxRetries = 3

var globalState string

func process(data string) string { return data }

type Config struct {
	Host string
	Port int
}

func (c *Config) Validate() bool { return len(c.Host) > 0 }

type Handler interface {
	Handle(msg string)
	Name() string
}
