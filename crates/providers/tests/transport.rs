use panda_core::{Cancel, Provider};
use panda_providers::Api;
use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
    time::{Duration, Instant},
};
#[test]
fn auth_failure_and_cancellation() {
    for stall in [false, true] {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", server.local_addr().unwrap());
        let cancel = Cancel::default();
        let signal = cancel.clone();
        let worker = thread::spawn(move || {
            let (mut stream, _) = server.accept().unwrap();
            let mut buf = [0; 2048];
            assert!(stream.read(&mut buf).unwrap() > 0);
            if stall {
                signal.stop();
                thread::sleep(Duration::from_millis(250));
            } else {
                stream
                    .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n")
                    .unwrap();
            }
        });
        let start = Instant::now();
        let err = Api {
            model: "test".into(),
            key: "fixture".into(),
            endpoint,
        }
        .reply("", &[], &[], &cancel, &mut |_| {})
        .unwrap_err();
        assert!(
            err.to_string()
                .contains(if stall { "cancelled" } else { "401" }),
            "{err}"
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        worker.join().unwrap();
    }
}
