MAX_RETRIES = 3

def process(data):
    return data.strip()

class Config:
    def __init__(self, host, port):
        self.host = host
        self.port = port

    def validate(self):
        return len(self.host) > 0

    @property
    def address(self):
        return f"{self.host}:{self.port}"
