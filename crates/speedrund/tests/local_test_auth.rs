use serde_json::{Value, json};
use std::{
    fs,
    net::TcpListener,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[test]
fn local_test_endpoint_requires_full_token_and_rejects_forged_results() {
    let home = tempfile::tempdir().unwrap();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_speedrund"))
        .env("SPEEDRUN_HOME", home.path())
        .env("SPEEDRUN_ADDR", format!("127.0.0.1:{port}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    struct Kill<'a>(&'a mut std::process::Child);
    impl Drop for Kill<'_> {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _cleanup = Kill(&mut daemon);
    let base = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while ureq::get(&format!("{base}/v1/health")).call().is_err() {
        assert!(Instant::now() < deadline, "daemon did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    let full = fs::read_to_string(home.path().join("token")).unwrap();
    let agent = fs::read_to_string(home.path().join("agent-token")).unwrap();
    let post = |token: &str, body: Value| {
        let result = ureq::post(&format!("{base}/v1/runs/nonexistent/local-tests"))
            .set("Authorization", &format!("Bearer {}", token.trim()))
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());
        match result {
            Ok(r) | Err(ureq::Error::Status(_, r)) => r.status(),
            Err(e) => panic!("{e}"),
        }
    };
    assert_eq!(post(&agent, json!({"argv": ["true"]})), 401);
    assert_eq!(post(&full, json!({"argv": ["true"]})), 404);
    assert_eq!(
        post(
            &full,
            json!({"argv": ["true"], "success": true, "head": "forged"})
        ),
        422
    );
}
