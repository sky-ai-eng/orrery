//! The egress a client is given is the only one it uses: proxy variables in
//! the process's environment, which a supervisor's or an operator's shell
//! may carry, change nothing. Its own binary, so that setting them touches
//! no other test.

use std::io::{Read, Write};
use std::net::TcpListener;

use orrery_outbound::{Client, Egress, Request};

#[test]
fn proxy_variables_in_the_environment_are_ignored() {
    // SAFETY: nothing else runs in this process yet: the receiver thread and
    // the runtime are started below.
    unsafe {
        for name in [
            "ALL_PROXY",
            "all_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
        ] {
            std::env::set_var(name, "http://127.0.0.1:9");
        }
        std::env::remove_var("NO_PROXY");
        std::env::remove_var("no_proxy");
    }

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut read = Vec::new();
        while !read.windows(4).any(|w| w == b"\r\n\r\n") {
            let mut chunk = [0; 1024];
            let n = conn.read(&mut chunk).unwrap();
            read.extend_from_slice(&chunk[..n]);
        }
        conn.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
    });

    let client = Client::new(Egress::Direct, None).unwrap();
    let attempt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(client.post(Request {
            url: format!("http://localhost:{port}/hook"),
            headers: Default::default(),
            body: Vec::new(),
        }));
    assert_eq!(attempt.outcome.unwrap().status, 204);
}
