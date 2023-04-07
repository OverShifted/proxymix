use std::{
    ffi::{c_int, c_uint},
    sync::Arc,
};

use bytes::BytesMut;
use tokio::{
    io::{unix::AsyncFd, AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::RwLock,
};
use url::Url;

use serde::Deserialize;

const BUFFER_SIZE: usize = 1024 * 1024;
const CURLINFO_LASTSOCKET: c_uint = 0x200000 + 29;

#[derive(Deserialize)]
pub struct Config {
    // With scheme included
    https_proxy_addr: String,
    https_proxy_username: String,
    https_proxy_password: String,
    pub(crate) port: u16,
}

// It is really just the first line.
#[derive(Debug)]
struct HttpHeader {
    pub method: String,
    pub target: String,
    pub version: String,

    pub headers: Vec<(String, String)>,
}

// https://stackoverflow.com/a/35907071/11814750
fn find_subsequence<T>(haystack: &[T], needle: &[T]) -> Option<usize>
where
    for<'a> &'a [T]: PartialEq,
{
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

impl HttpHeader {
    pub fn parse(data: &[u8]) -> (HttpHeader, &[u8]) {
        let first_line_break = data.iter().position(|v| *v == b'\r').unwrap();
        let (first_line, remainder) = data.split_at(first_line_break);

        let first_line = String::from_utf8_lossy(first_line);
        let mut parts = first_line.split_whitespace();

        let end_of_header = find_subsequence(remainder, b"\r\n\r\n").unwrap();

        (
            HttpHeader {
                method: parts.next().unwrap().to_string(),
                target: parts.next().unwrap().to_string(),
                version: parts.next().unwrap().to_string(),
                headers: Self::parse_headers(&String::from_utf8_lossy(
                    &remainder[2..end_of_header + 1 /* Include a `\r` at the end */],
                )),
            },
            &remainder[end_of_header + 4..],
        )
    }

    fn parse_headers(headers: &str) -> Vec<(String, String)> {
        headers
            .split('\n')
            .filter_map(|item| {
                if item.len() == 0 {
                    None
                } else {
                    let colon_index = item.find(": ").unwrap();
                    let (k, v) = item.split_at(colon_index);
                    Some((k.into(), v[2..v.len() - 1].into()))
                }
            })
            .collect::<Vec<(String, String)>>()
    }

    pub fn get_header(&self, key: &str) -> Option<&str> {
        let key = key.to_lowercase();

        for (k, v) in &self.headers {
            if k.to_lowercase() == key {
                return Some(&v);
            }
        }

        None
    }

    pub fn to_easy_curl(&self, config: &Config) -> Result<CurlEasy, curl::Error> {
        let mut easy = curl::easy::Easy2::new(CurlHandler);
        easy.url(&self.target)?;
        easy.http_proxy_tunnel(true)?;
        easy.proxy(&config.https_proxy_addr)?;
        easy.proxy_username(&config.https_proxy_username)?;
        easy.proxy_password(&config.https_proxy_password)?;
        easy.connect_only(true)?;
        easy.perform()?;

        let fd = easy.getopt_long(CURLINFO_LASTSOCKET).unwrap() as c_int;

        Ok(CurlEasy {
            inner: easy,
            fd: Arc::new(RwLock::new(AsyncFd::new(fd).unwrap())),
        })
    }

    pub fn to_target_header(&self) -> String {
        let url = Url::parse(&self.target).unwrap();
        format!(
            "{} {} {}\r\n{}\r\n\r\n",
            self.method,
            url.path(),
            self.version,
            self.headers
                .iter()
                .filter(|(k, _)| !k.to_lowercase().starts_with("proxy")) // Remove headers like `Proxy-Connection`
                .map(|(k, v)| { format!("{}: {}", k, v) }) // Encode the header
                // FIXME: Unnecessary heap allocation here
                // See: https://stackoverflow.com/questions/56033289/join-iterator-of-str
                .collect::<Vec<String>>()
                .join("\r\n")
        )
    }

    pub fn is_connect(&self) -> bool {
        self.method.to_uppercase() == "CONNECT"
    }
}

struct CurlEasy {
    pub inner: curl::easy::Easy2<CurlHandler>,
    pub fd: Arc<RwLock<AsyncFd<c_int>>>,
}

unsafe impl Send for CurlEasy {}
unsafe impl Sync for CurlEasy {}

struct CurlHandler;
impl curl::easy::Handler for CurlHandler {}

pub async fn listen(port: u16, config: Config) {
    let listener = TcpListener::bind(("0.0.0.0", port)).await.unwrap();
    let config = Arc::new(config);

    loop {
        let (stream, _addr) = listener.accept().await.unwrap();
        println!("-- New connection from {}", _addr);

        let config = config.clone();
        tokio::spawn(async move {
            handle_connection(stream, config).await;
        });
    }
}

async fn handle_connection(mut stream: TcpStream, config: Arc<Config>) {
    let mut bytes = BytesMut::new();

    // Read the entire header
    loop {
        let out = stream.read_buf(&mut bytes).await.unwrap();

        // IDK why, but sometimes nothing can be read from the stream.
        // (This happened to me with firefox)
        // Without this if statement, We will consume alot of CPU time.
        if out == 0 {
            stream.shutdown().await.unwrap();
            return;
        }

        if find_subsequence(&bytes, b"\r\n\r\n").is_some() {
            break;
        }
    }

    // Extract the target host.
    let (header, remainder) = HttpHeader::parse(&bytes);
    let is_connect = header.is_connect();

    // Create a connection to the remote proxy
    match header.to_easy_curl(&config) {
        Ok(mut easy) => {
            if is_connect {
                stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await.unwrap();
            } else {
                // Curl has already sent the CONNECT request. We are directly
                // communicating with the target. So we send the "normal" HTTP header.
                let target_header = header.to_target_header();
                curl_send_all(&mut easy, target_header.as_bytes()).await;

                // And the rest of the data
                curl_send_all(&mut easy, &remainder).await;
            }

            // Start transmitting data back and forth
            bidirectional_transmit(easy, stream, is_connect).await;
        }
        Err(_) => {
            if is_connect {
                stream.write_all(b"HTTP/1.1 400 NOK\r\n\r\n").await.unwrap();
            } else {
                stream.shutdown().await.unwrap();
            }
        }
    }
}

async fn curl_send_all(easy: &mut CurlEasy, data: &[u8]) {
    let mut sent = 0;

    while sent < data.len() {
        if let Ok(len) = easy.inner.send(&data[sent..]) {
            sent += len
        }
    }
}

fn poll_curl(easy: &mut CurlEasy, buffer: &mut [u8]) -> Result<usize, ()> {
    match easy.inner.recv(buffer) {
        Ok(num_bytes) => Ok(num_bytes),
        Err(err) if err.is_again() => Err(()),
        Err(err) => panic!("Got error from curl: {:?}", err),
    }
}

async fn curl_read(easy: &mut CurlEasy, buffer: &mut [u8]) -> usize {
    loop {
        match poll_curl(easy, buffer) {
            Ok(num_bytes) => return num_bytes,
            Err(_) => {
                let fd = easy.fd.clone();

                let result = fd
                    .read()
                    .await
                    .readable()
                    .await
                    .unwrap()
                    .try_io(|_| poll_curl(easy, buffer).map_err(|_| std_io_would_block()));

                if let Ok(Ok(num_bytes)) = result {
                    return num_bytes;
                }
            }
        }
    }
}

fn std_io_would_block() -> std::io::Error {
    std::io::Error::from(std::io::ErrorKind::WouldBlock)
}

async fn bidirectional_transmit(mut easy: CurlEasy, stream: TcpStream, is_connect: bool) {
    let (mut stream_r, mut stream_w) = stream.into_split();

    let (to_curl_send, mut to_curl_recv) = tokio::sync::mpsc::channel(1000);
    let (from_curl_send, mut from_curl_recv) = tokio::sync::mpsc::channel::<Vec<u8>>(1000);

    // remote proxy -> client
    let remote_to_client = tokio::spawn(async move {
        let mut bytes = BytesMut::new();
        let mut bytes_remaining: Option<usize> = None;

        loop {
            match from_curl_recv.recv().await {
                Some(buffer) => {
                    stream_w.write_all(&buffer).await.unwrap();

                    if !is_connect {
                        if let Some(bytes_remaining) = bytes_remaining.as_mut() {
                            *bytes_remaining -= buffer.len();
                        } else {
                            bytes.extend_from_slice(&buffer);

                            if let Some(pos) = find_subsequence(&bytes, b"\r\n\r\n") {
                                bytes_remaining = Some(
                                    HttpHeader::parse(&bytes)
                                        .0
                                        .get_header("content-length")
                                        .unwrap()
                                        .parse()
                                        .unwrap(),
                                );
                                *bytes_remaining.as_mut().unwrap() -= buffer.len() - pos - 4;
                                bytes.clear();
                            }
                        }

                        if let Some(v) = bytes_remaining {
                            if v == 0 {
                                // End of http transaction
                                return true;
                            }
                        }
                    }
                }
                None => return false,
            }
        }
    });

    // client -> remote proxy
    let client_to_remote = tokio::spawn(async move {
        loop {
            let mut buffer = vec![0; BUFFER_SIZE];
            match stream_r.read(&mut buffer).await {
                Ok(num_bytes) if num_bytes > 0 => {
                    to_curl_send
                        .send(buffer[..num_bytes].to_vec())
                        .await
                        .unwrap();
                }
                _ => return,
            };
        }
    });

    // Curl handler
    let curl_handler = tokio::spawn(async move {
        let mut buffer = vec![0; BUFFER_SIZE];

        loop {
            tokio::select! {
                data = to_curl_recv.recv() => {
                    match data {
                        Some(data) => curl_send_all(&mut easy, &data).await,
                        None => return
                    }
                }

                num_bytes = curl_read(&mut easy, &mut buffer) => {
                    if num_bytes == 0 {
                        return
                    }

                    from_curl_send.send(buffer[..num_bytes].to_vec()).await.unwrap();
                }
            }
        }
    });

    let _ = client_to_remote.await;
    remote_to_client.abort();
    curl_handler.abort();
}
