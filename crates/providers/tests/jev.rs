use panda_core::{
    Cancel,
    native_compaction::{Judge, Kind, Question},
};
use panda_providers::jev::Jev;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    thread,
    time::{Duration, Instant},
};

fn body(stream: &mut TcpStream) -> Value {
    let mut reader = BufReader::new(stream);
    let mut size = 0;
    let mut line = String::new();
    loop {
        line.clear();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        if let Some((_, n)) = line
            .to_lowercase()
            .strip_prefix("content-length")
            .and_then(|s| s.split_once(':'))
        {
            size = n.trim().parse().unwrap();
        }
    }
    let mut data = vec![0; size];
    reader.read_exact(&mut data).unwrap();
    serde_json::from_slice(&data).unwrap()
}
fn request(kind: Kind, tool: bool, text: &str) -> Question {
    let mut q = Question::new(
        kind,
        json!({"segment":{"text":text},"next":"fixture secret"}),
    );
    q.scoring_tool = tool;
    q.probability("needed", "Is it needed?");
    q
}
fn mock(server: TcpListener, successes: Vec<bool>) -> thread::JoinHandle<Vec<Value>> {
    thread::spawn(move || {
        successes.into_iter().map(|success|{
        let (mut stream,_)=server.accept().unwrap();
        let request=body(&mut stream);
        let response=json!({"answers":if success{json!({"needed":{"type":"noul","noul":0.9}})}else{json!({"needed":{"type":"choice","choice":"invalid"}})},"usage":{"input_tokens":11,"output_tokens":2}}).to_string();
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",response.len()).unwrap();
        request
    }).collect()
    })
}

#[test]
fn score_copy_masks_before_trimming_memoizes_exact_successes_and_retries_failures() {
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", server.local_addr().unwrap());
    let worker = mock(server, vec![false, true, true, true, true]);
    let mut judge = Jev::new(
        &endpoint,
        "fixture secret",
        "fixture",
        vec!["another-password".into()],
    )
    .unwrap();
    judge.bind(Some(endpoint.clone()));
    let long = format!(
        "HEAD {} -----BEGIN PRIVATE KEY-----{}-----END PRIVATE KEY----- {} TAIL fixture secret another-password",
        "猫".repeat(710),
        "LEAKED_KEY_LINE".repeat(100),
        "q".repeat(610)
    );
    let timeout = Duration::from_secs(2);
    let c = Cancel::default();
    assert!(
        judge
            .ask(request(Kind::Score, true, &long), timeout, &c)
            .is_err()
    );
    judge
        .ask(request(Kind::Score, true, &long), timeout, &c)
        .unwrap();
    judge
        .ask(request(Kind::Score, true, &long), timeout, &c)
        .unwrap();
    judge
        .ask(request(Kind::Score, false, &long), timeout, &c)
        .unwrap();
    judge
        .ask(request(Kind::Coverage, false, &long), timeout, &c)
        .unwrap();
    let mut other = Jev::new(
        &endpoint,
        "fixture secret",
        "fixture",
        vec!["another-password".into()],
    )
    .unwrap();
    other.bind(Some(format!("{endpoint}/second")));
    other
        .ask(request(Kind::Score, true, &long), timeout, &c)
        .unwrap();
    let requests = worker.join().unwrap();
    for request in &requests {
        let wire = request.to_string();
        for secret in ["another-password", "fixture secret", "LEAKED_KEY_LINE"] {
            assert!(!wire.contains(secret));
        }
    }
    let scored = requests[1]["state"]["segment"]["text"].as_str().unwrap();
    assert!(
        scored.contains("trimmed for scoring")
            && scored.starts_with("HEAD")
            && scored.contains("TAIL")
    );
    for full in &requests[2..4] {
        assert!(
            !full.to_string().contains("trimmed for scoring")
                && full.to_string().contains(&"猫".repeat(600))
        );
    }
    assert_eq!(judge.stats().judged, 3);
    assert_eq!(judge.stats().failed, 1);
    assert_eq!(judge.stats().reused, 1);
    assert_eq!(judge.stats().usage.len(), 4);
}

#[test]
fn unavailable_key_and_cancelled_judge_issue_no_requests_and_stalled_response_cancels() {
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", server.local_addr().unwrap());
    let mut none = Jev::new(&endpoint, "", "fixture", vec![]).unwrap();
    let c = Cancel::default();
    let timeout = Duration::from_secs(3);
    assert!(
        none.ask(request(Kind::Moment, false, "a"), timeout, &c)
            .is_err()
    );
    let mut configured = Jev::new(&endpoint, "secret", "fixture", vec![]).unwrap();
    c.stop();
    assert!(
        configured
            .ask(request(Kind::Moment, false, "a"), timeout, &c)
            .is_err()
    );
    server.set_nonblocking(true).unwrap();
    assert_eq!(
        server.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    server.set_nonblocking(false).unwrap();
    c.reset();
    let signal = c.clone();
    let worker = thread::spawn(move || {
        let (mut stream, _) = server.accept().unwrap();
        body(&mut stream);
        signal.stop();
        thread::sleep(Duration::from_millis(250));
    });
    let start = Instant::now();
    assert!(
        configured
            .ask(request(Kind::Moment, false, "a"), timeout, &c)
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
    assert!(start.elapsed() < Duration::from_secs(2));
    worker.join().unwrap();
}
