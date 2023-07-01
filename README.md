# std-esp32-proxy
An async HTTP proxy for the ESP32 using the ESP-IDF.

# How it works
The proxy works by making use of the `std` library exposed through
the `ESP-IDF`.

It connects to WiFi and listens for TCP clients for whom to proxy HTTP requests. The proxying happens through a makeshift bi-directional data copy
similar to [tokio::io::copy_bidirectional](https://docs.rs/tokio/latest/tokio/io/fn.copy_bidirectional.html) after a `CONNECT` request is received and a connection to the destination is established.

# Setup
Please have a look through [The Rust on ESP Book](https://esp-rs.github.io/book/) for setting up the dev environment.

Depending on the `espflash` version, you might have to tweak `.cargo/config.toml`.

# Configuration
There are a couple of environment variables to configure the proxy server:
 - `PROXY_SSID`: WiFi SSID, **mandatory**
 - `PROXY_PASS`: WiFi password, **mandatory**
 - `PROXY_PORT`: TCP server listening port, **mandatory**
 - `PROXY_TUNNEL_TIMEOUT`: Timeout for read operations in milliseconds, **optional**, defaults to `1000`.
 - `PROXY_TUNNEL_BUF_SIZE`: Buffer size in bytes for data copying, **optional**, defaults to `6144`
 - `PROXY_NUM_SOCKETS`: Maximum number of TCP sockets that can be open at the same time, **optional**, defaults to `16`.