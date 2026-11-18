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

#[test]
fn classifies_http_retries_and_server_delay() {
    use panda_core::TransientFailure;
    for (status, code, header, expected) in [
        (503, "", "2", Some(2)),
        (429, "rate_limit", "Thu, 01 Jan 1970 00:00:00 GMT", Some(0)),
        (401, "", "0", None),
        (429, "insufficient_quota", "0", None),
        (403, "", "0", None),
    ] {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", server.local_addr().unwrap());
        let worker = thread::spawn(move || {
            let (mut stream, _) = server.accept().unwrap();
            let mut buf = [0; 4096];
            assert!(stream.read(&mut buf).unwrap() > 0);
            let body = serde_json::json!({"error":{"code":code}}).to_string();
            write!(stream,"HTTP/1.1 {status} Error\r\nRetry-After: {header}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        });
        let error = Api {
            endpoint,
            model: "fixture".into(),
            key: "fixture".into(),
        }
        .reply("", &[], &[], &Cancel::default(), &mut |_| {})
        .unwrap_err();
        assert_eq!(error.is::<TransientFailure>(), expected.is_some());
        assert_eq!(
            error
                .downcast_ref::<TransientFailure>()
                .and_then(|e| e.retry_after)
                .map(|d| d.as_secs()),
            expected
        );
        worker.join().unwrap();
    }
}
