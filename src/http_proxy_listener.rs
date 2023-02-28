use std::{ffi::c_int, sync::Arc};

use bytes::BytesMut;
use tokio::{
    io::{unix::AsyncFd, AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::RwLock,
};
use url::Url;

use serde::Deserialize;

const BUFFER_SIZE: usize = 1024 * 1024;

#[derive(Deserialize)]
pub struct Config {
    // With scheme included
    https_proxy_addr: String,
    https_proxy_username: String,
    https_proxy_password: String
}

// It is really just the first line.
#[derive(Debug)]
struct HttpHeader {
    pub method: String,
    pub target: String,
    pub version: String,
}

impl HttpHeader {
    fn parse(data: &[u8]) -> (HttpHeader, Vec<u8>) {
        let first_line_break = data.iter().position(|v| *v == b'\n').unwrap();
        let (first_line, remainder) = data.split_at(first_line_break);

        let first_line = String::from_utf8_lossy(first_line);
        println!("{:?}", first_line);
        let mut parts = first_line.split_whitespace();

        (
            HttpHeader {
                method: parts.next().unwrap().to_string(),
                target: parts.next().unwrap().to_string(),
                version: parts.next().unwrap().to_string(),
            },
            Vec::from(remainder),
        )
    }

    fn to_easy_curl(&self, config: &Config) -> Result<CurlEasy, curl::Error> {
        let mut easy = curl::easy::Easy2::new(CurlHandler);
        easy.url(&self.target)?;
        easy.http_proxy_tunnel(true)?;
        easy.proxy(&config.https_proxy_addr)?;
        easy.proxy_username(&config.https_proxy_username)?;
        easy.proxy_password(&config.https_proxy_password)?;
        easy.connect_only(true)?;
        easy.perform()?;

        let fd = easy
            .getopt_long(0x200000 + 29 /* CURLINFO_LASTSOCKET */)
            .unwrap() as c_int;

        Ok(CurlEasy {
            inner: easy,
            fd: Arc::new(RwLock::new(AsyncFd::new(fd).unwrap())),
        })
    }

    fn to_target_header(&self) -> String {
        let url = Url::parse(&self.target).unwrap();
        format!("{} {} {}", self.method, url.path(), self.version)
    }

    fn is_connect(&self) -> bool {
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
        println!("-- Waiting for connection");
        let (stream, _addr) = listener.accept().await.unwrap();
        println!("-- New connection from {}", _addr);

        let config = config.clone();
        tokio::spawn(async move { handle_connection(stream, config).await });
    }
}

async fn handle_connection(mut stream: TcpStream, config: Arc<Config>) {
    let mut bytes = BytesMut::new();

    // Wait until the first line is recved.
    while !bytes.contains(&b'\n') {
        stream.read_buf(&mut bytes).await.unwrap();
    }

    // Extract the target host.
    let (header, remainder) = HttpHeader::parse(&bytes);

    let is_connect = header.is_connect();
    if is_connect {
        while !bytes.ends_with(b"\r\n\r\n") {
            stream.read_buf(&mut bytes).await.unwrap();
        }
    }

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
            bidirectional_transmit(easy, stream).await;
        }
        Err(_) => {
            if is_connect {
                stream.write_all(b"HTTP/1.1 400 NOK\r\n\r\n").await.unwrap();
            } else {
                stream.shutdown().await.unwrap();
            }
        }
    }

    // println!("-- Connection dropped!");
}

async fn curl_send_all(easy: &mut CurlEasy, data: &[u8]) {
    let mut sent = 0;

    while sent < data.len() {
        match easy.inner.send(&data[sent..]) {
            Ok(len) => sent += len,
            Err(_) => (),
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

                let result = fd.read().await
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

async fn bidirectional_transmit(mut easy: CurlEasy, stream: TcpStream) {
    let (mut stream_r, mut stream_w) = stream.into_split();

    let (to_curl_send, mut to_curl_recv) = tokio::sync::mpsc::channel(100);
    let (from_curl_send, mut from_curl_recv) = tokio::sync::mpsc::channel::<Vec<u8>>(100);

    // remote proxy -> client
    let remote_to_client = tokio::spawn(async move {
        loop {
            match from_curl_recv.recv().await {
                Some(buffer) => {stream_w.write_all(&buffer).await.unwrap(); ()},
                None => return
            }
        }
    });

    // client -> remote proxy
    let client_to_remote = tokio::spawn(async move {
        loop {
            let mut buffer = vec![0; BUFFER_SIZE];
            match stream_r.read(&mut buffer).await {
                Ok(num_bytes) if num_bytes > 0 => to_curl_send
                    .send(buffer[..num_bytes].to_vec())
                    .await
                    .unwrap(),
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
                        Some(data) => {
                            // println!("Sending to curl!");
                            curl_send_all(&mut easy, &data).await;
                        }
                        None => return
                    }
                }

                num_bytes = curl_read(&mut easy, &mut buffer) => {
                    if num_bytes == 0 {
                        return
                    }

                    // println!("Reading from curl!");
                    from_curl_send.send(buffer[..num_bytes].to_vec()).await.unwrap();
                }
            }
        }
    });

    let _ = client_to_remote.await;
    remote_to_client.abort();
    curl_handler.abort();
}
