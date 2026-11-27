//! A tiny HTTP client for the container healthcheck: the image has no shell, curl or
//! wget, so the binary checks itself.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::Error;
use crate::server::CORE;

const TIMEOUT: Duration = Duration::from_secs(3);

/// Sends `GET /solr/openlibrary/admin/ping` to `addr` and succeeds on a 200 response.
///
/// # Errors
/// Fails if the address does not resolve, the connection or the exchange fails or times
/// out, or the status is not 200.
pub fn ping(addr: &str) -> Result<(), Error> {
    let target = addr
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| Error::Other(format!("cannot resolve {addr}")))?;
    let mut stream = TcpStream::connect_timeout(&target, TIMEOUT)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    write!(
        stream,
        "GET /solr/{CORE}/admin/ping HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )?;
    let mut head = Vec::new();
    stream.take(64).read_to_end(&mut head)?;
    let status = String::from_utf8_lossy(&head);
    let status = status.lines().next().unwrap_or_default();
    if status.split_whitespace().nth(1) == Some("200") {
        Ok(())
    } else {
        Err(Error::Other(format!("ping {addr}: {status:?}")))
    }
}

#[cfg(test)]
mod tests {
    use super::ping;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    /// Serves one connection with `reply` and returns the request line it read.
    fn one_shot(reply: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let handle = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(conn.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            conn.write_all(reply.as_bytes()).unwrap();
            line
        });
        (addr, handle)
    }

    #[test]
    fn ok_on_200() {
        let (addr, server) = one_shot("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}");
        assert!(ping(&addr).is_ok());
        assert_eq!(
            server.join().unwrap(),
            "GET /solr/openlibrary/admin/ping HTTP/1.0\r\n"
        );
    }

    #[test]
    fn fails_on_other_status_or_no_server() {
        let (addr, server) = one_shot("HTTP/1.1 404 Not Found\r\n\r\n");
        assert!(ping(&addr).is_err());
        server.join().unwrap();
        let free = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = free.local_addr().unwrap().to_string();
        drop(free);
        assert!(ping(&addr).is_err());
    }
}
