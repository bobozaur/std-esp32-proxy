#![feature(const_option_ext)]

use std::{
    net::{SocketAddr, TcpListener, TcpStream},
    panic::catch_unwind,
    time::Duration,
};

use anyhow::{bail, Result};
use async_io::block_on;
use embassy_futures::select::{select3, Either3};
use embedded_svc::wifi::{AuthMethod, ClientConfiguration, Configuration};
use esp_idf_hal::prelude::Peripherals;
use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    nvs::EspDefaultNvsPartition,
    timer::EspTaskTimerService,
    wifi::{AsyncWifi, EspWifi},
};
use esp_idf_sys as _;

use log::*;
use once_cell::sync::Lazy;
use smol::{
    future::pending,
    io::{AsyncReadExt, AsyncWriteExt},
    Executor,
};

const SSID: &str = env!("PROXY_SSID");
const PASS: &str = env!("PROXY_PASS");
const PORT: u16 = str_to_usize(env!("PROXY_PORT")) as u16;
const TUNNEL_TIMEOUT: usize = option_env!("PROXY_TUNNEL_TIMEOUT")
    .map(str_to_usize)
    .unwrap_or(1000);
const TUNNEL_BUF_SIZE: usize = option_env!("PROXY_TUNNEL_BUF_SIZE")
    .map(str_to_usize)
    .unwrap_or(6114);

/// The number of sockets (which use file descriptors) open at the same time.
/// Too many would consume too many resources and too few would result in lower performance.
/// From limited testing, the default value seems to be the sweet spot.
const NUM_SOCKETS: usize = option_env!("PROXY_NUM_SOCKETS")
    .map(str_to_usize)
    .unwrap_or(16);

const PROXY_OK_RESP: &[u8] = b"HTTP/1.1 200 Connection Established\r\n\r\n";
const HTTP_EOF: &[u8] = b"\r\n\r\n";

static EXECUTOR: Lazy<Executor<'_>> = Lazy::new(|| {
    let num_threads = NUM_SOCKETS / 8;
    for n in 0..num_threads {
        std::thread::Builder::new()
            .name(format!("smol-{n}"))
            .stack_size(8192)
            .spawn(|| catch_unwind(|| block_on(EXECUTOR.run(pending::<()>()))).unwrap())
            .unwrap();
    }
    Executor::new()
});

fn main() {
    // It is necessary to call this function once. Otherwise some patches to the runtime
    // implemented by esp-idf-sys might not link properly. See https://github.com/esp-rs/esp-idf-template/issues/71
    esp_idf_sys::link_patches();
    // Bind the log crate to the ESP Logging facilities
    esp_idf_svc::log::EspLogger::initialize_default();

    // Set the limit for file descriptors
    #[allow(clippy::needless_update)]
    {
        esp_idf_sys::esp!(unsafe {
            esp_idf_sys::esp_vfs_eventfd_register(&esp_idf_sys::esp_vfs_eventfd_config_t {
                max_fds: NUM_SOCKETS,
                ..Default::default()
            })
        })
        .unwrap();
    }

    loop {
        if let Err(e) = block_on(EXECUTOR.run(run())) {
            error!("Error encountered: {e}");
        }
    }
}

/// Connects to WiFi and starts proxying requests.
/// This function only returns if an error is encountered,
/// otherwise proxying requests forever.
///
/// # Errors
///
/// Errors can arise from either the WiFi setup or
/// the proxy server setup.
async fn run() -> Result<()> {
    let peripherals = Peripherals::take().unwrap();
    let sys_loop = EspSystemEventLoop::take()?;
    let timer_service = EspTaskTimerService::new()?;
    let nvs = EspDefaultNvsPartition::take()?;

    let mut wifi = AsyncWifi::wrap(
        EspWifi::new(peripherals.modem, sys_loop.clone(), Some(nvs))?,
        sys_loop,
        timer_service,
    )?;

    let wifi_configuration: Configuration = Configuration::Client(ClientConfiguration {
        ssid: SSID.into(),
        bssid: None,
        auth_method: AuthMethod::WPA2Personal,
        password: PASS.into(),
        channel: None,
    });

    wifi.set_configuration(&wifi_configuration)?;

    wifi.start().await?;
    info!("Wifi started");

    wifi.connect().await?;
    info!("Wifi connected");

    wifi.wait_netif_up().await?;
    info!("Wifi netif up");

    let ip_info = wifi.wifi().sta_netif().get_ip_info()?;

    info!("Wifi DHCP info: {:?}", ip_info);

    start_proxy().await?;

    Ok(())
}

/// Starts proxying requests by creating a [`TcpListener`]
/// and forever looping over accepted clients.
///
/// This function only accepts a client, then spawns
/// another async task to take handle it, being ready to
/// accept another client after that.
///
/// # Errors
///
/// An error can really only arise from the listener setup.
async fn start_proxy() -> Result<()> {
    let listener = smol::Async::<TcpListener>::bind(([0, 0, 0, 0], PORT))?;

    loop {
        let (src_stream, peer_addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                error!("error {e}");
                continue;
            }
        };

        info!("Client accepted: {}", peer_addr);

        EXECUTOR.spawn(proxy_wrapper(src_stream)).detach();
    }
}

/// A wrapper with the sole purpose of logging errors
/// encountered in the proxy tunnel.
async fn proxy_wrapper(src_stream: smol::Async<TcpStream>) {
    if let Err(e) = proxy(src_stream).await {
        error!("Proxy error: {e:?}");
    }
}

/// One of the two core components of the proxy server.
///
/// This function takes accepts a [`TcpStream`] and then:
/// - handles the initial (and expected) `CONNECT` request
/// - creates another [`TcpStream`] to the destination server
/// - proxies the request from the source client and the response from the destination server.
///
/// # Errors
///
/// Apart from typical I/O errors, proxying will fail if:
/// - the actual request to be proxied is not preceded by an `HTTP CONNECT` request
/// - parsing the destination server host fails
/// - connecting a [`TcpStream`] to the destination server fails
async fn proxy(mut src_stream: smol::Async<TcpStream>) -> Result<()> {
    // Buffer that will be used for read/write operations.
    let mut buf = vec![0; TUNNEL_BUF_SIZE].into_boxed_slice();

    // Read and parse the initial request.
    // It will generally always fit inside a single packet.
    let n = sock_read(&mut src_stream, &mut buf).await?;
    let req_str = std::str::from_utf8(&buf[..n])?;
    let host = parse_initial_req(req_str)?;

    // Ensure we read the entire CONNECT request,
    // even if we most probably already did.
    while &buf[n - 4..n] != HTTP_EOF {
        src_stream.read(&mut buf).await?;
    }

    let dest_stream = smol::Async::<TcpStream>::connect(host).await?;

    info!("Connected to destination {host}!");

    // Respond to the client
    src_stream.write_all(PROXY_OK_RESP).await?;
    src_stream.flush().await?;

    info!("Proxying request...");

    tunnel(src_stream, dest_stream, &mut buf).await?;

    info!("Response sent back to client!");

    Ok(())
}

/// The other core component of the proxy server.
///
/// This function takes the two [`TcpStream`] instances
/// and copies data back and forth between them.
///
/// It does not parse the request/response at all,
/// so it doesn't implicitly know when either of them
/// is over.
///
/// However, we know for sure that:
/// - we first get a request from the source
/// - as we pipe the request to the destination, when it's over,
///   the destination will start sending a response
///
/// Hence, we read concurrently from both, initially expecting
/// the request from the source. When that is over,
/// the response will begin being read from the destination.
///
/// Since without parsing the response we don't know
/// for sure when it's over and no more data will be sent,
/// we instead have a one second timeout after which the tunnel is closed.
async fn tunnel(
    mut src_stream: smol::Async<TcpStream>,
    mut dest_stream: smol::Async<TcpStream>,
    buf: &mut [u8],
) -> Result<()> {
    // Reuse the same buffer for both read operations, because RAM be limited.
    //
    // SAFETY:
    //    We're only ever expecting one of the reads to populate the buffer.
    //
    //    Both of them populating the buffer means that the destination
    //    responded to an incomplete request or the request is malformed.
    //
    //    Regardless of which of the above scenarios happen, it is not
    //    our problem and it is up to the source/destination to deal with it.
    let buf1 = unsafe { core::slice::from_raw_parts_mut(buf.as_mut_ptr(), buf.len()) };
    let buf2 = unsafe { core::slice::from_raw_parts_mut(buf.as_mut_ptr(), buf.len()) };

    loop {
        // Conncurrently read from both streams until some
        // data is certainly received, or until reaching the timeout.
        //
        // We need an external timeout and not a read timeout on the sockets
        // because the [`TcpStream`] instances are in non-blocking mode.
        let res = select3(
            sock_read(&mut src_stream, buf1),
            sock_read(&mut dest_stream, buf2),
            smol::Timer::after(Duration::from_millis(TUNNEL_TIMEOUT as u64)),
        )
        .await;

        // Decide whether we keep the tunnel alive or close the sockets.
        //
        // If the read didn't timeout, then some data was read, so we
        // pipe it to the appropriate socket.
        //
        // However, if a timeout occurred, we break from the loop and end the tunnel.
        match res {
            Either3::First(r) => sock_write(&mut dest_stream, &buf[..r?]).await,
            Either3::Second(r) => sock_write(&mut src_stream, &buf[..r?]).await,
            Either3::Third(_) => break,
        }?;
    }

    Ok(())
}

/// Helper to ensure we flush all the written data.
async fn sock_write(socket: &mut smol::Async<TcpStream>, buf: &[u8]) -> Result<()> {
    debug!("Writing {} bytes", buf.len());

    socket.write_all(buf).await?;
    socket.flush().await?;

    Ok(())
}

/// Helper to ensure we actually read something from the socket.
async fn sock_read(socket: &mut smol::Async<TcpStream>, buf: &mut [u8]) -> Result<usize> {
    let mut n = 0;

    while n == 0 {
        n = socket.read(buf).await?;
        debug!("Read {n} bytes")
    }

    Ok(n)
}

/// Parses the initial request received (or rather the first part of it) to:
/// - ensure it is a `CONNECT` request
/// - extract the destination host
///
/// `CONNECT` requests are fairly small and can fit inside a single TCP packet
/// (assuming the standard Ethernet payload is the TCP window size).
///
/// Even if the entire request does not fit, we really only care about the first line.
/// Even less than that, as we really just want the first two "words" (as in whitespace separated).
///
/// The only way for the request data to be abruptly cut off in the middle of the
/// destination host (or before it, even) is for the TCP window size to be too small
/// to expect this server to work in a meaningful manner anyway.
///
/// # Errors
///
/// Will error out if:
/// - the request data is insufficient/malformed.
/// - the request is not a `CONNECT` request
/// - parsing the host to a [`SocketAddr`] fails
fn parse_initial_req(req_str: &str) -> Result<SocketAddr> {
    let mut req_iter = req_str.split_whitespace();
    let Some(method) = req_iter.next() else {
        bail!("can't determine the HTTP method from request data: {req_str}");
    };

    if method != "CONNECT" {
        bail!("expected CONNECT request");
    }

    info!("CONNECT request received");

    let Some(host) = req_iter.next() else {
        bail!("missing host from request");
    };

    let sock_addr = host.into()?;

    Ok(sock_addr)
}

/// Helper to allow converting a string slice (essentially from env variables)
/// to usize at compile time.
const fn str_to_usize(env: &str) -> usize {
    let mut bytes = env.as_bytes();
    let mut val = 0;

    while let [byte, rest @ ..] = bytes {
        assert!(b'0' <= *byte && *byte <= b'9', "invalid digit");
        val = val * 10 + (*byte - b'0') as usize;
        bytes = rest;
    }

    val
}
