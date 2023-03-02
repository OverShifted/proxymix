# Proxymix
Converts a https proxy (potentially with digest auth) to a local http proxy

## Usage
1. Clone and compile it yourself:
```sh
git clone https://github.com/OverShifted/proxymix
cd proxymix
cargo build
```

2.Create a file called `proxymix.toml` with the following contents:
```toml
port = 4444
https_proxy_addr = "https://example.com:1234"
https_proxy_username = "<your username here>"
https_proxy_password = "<your password here>"
```
3. Run proxy mix (In the working directory which `proxymix.toml` is located at)
```sh
cargo run
```

4. Test it
```sh
# With http CONNECT method
curl --proxy http://127.0.0.1:4444 https://google.com/
# Without http CONNECT method
curl --proxy http://127.0.0.1:4444 http://google.com/
```
It has some problems (aka bugs) and won't work under all circumstances.
