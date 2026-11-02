//! A stand-in for the GitHub endpoints the commons uses to link a person (COMMONS.md §20):
//! `POST /login/device/code`, `POST /login/oauth/access_token` and `GET /user`, on one
//! loopback port that serves as both `SPEEDRUN_GITHUB_URL` and `SPEEDRUN_GITHUB_API`.
//!
//! Shared by the commons and CLI tests through `#[path]`.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

pub const USER_CODE: &str = "WDJB-MJHT";
pub const VERIFICATION_URI: &str = "https://github.com/login/device";
pub const DEVICE_CODE: &str = "3584d83530557fdd1f46af8289938c8ef79f9dc5";
pub const ACCESS_TOKEN: &str = "gho_16C7e42F292c6912E7710c838347Ae178B4a";
pub const CLIENT_ID: &str = "Iv1.speedruntestclient";

/// How the stand-in answers.
#[derive(Clone, Copy)]
pub struct Behaviour {
    /// Token requests answered `authorization_pending` before the token is granted.
    pub pending_polls: usize,
    /// Answer every token request with `access_denied` instead.
    pub deny: bool,
    /// The `interval` the device-code answer names.
    pub interval: u64,
    pub user_id: u64,
    pub login: &'static str,
}

impl Default for Behaviour {
    fn default() -> Self {
        Self {
            pending_polls: 1,
            deny: false,
            interval: 5,
            user_id: 4242,
            login: "Octo-Person",
        }
    }
}

/// One request the stand-in received.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    /// Header names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

pub struct FakeGitHub {
    pub base: String,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl FakeGitHub {
    pub fn start(behaviour: Behaviour) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&requests);
        let token_requests = Arc::new(Mutex::new(0usize));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (record, token_requests) = (Arc::clone(&record), Arc::clone(&token_requests));
                std::thread::spawn(move || {
                    let _ = answer(stream, &record, &token_requests, behaviour);
                });
            }
        });
        Self { base, requests }
    }

    /// The environment that points a commons at this stand-in with a client id.
    pub fn env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("SPEEDRUN_GITHUB_URL", self.base.clone()),
            ("SPEEDRUN_GITHUB_API", self.base.clone()),
            ("SPEEDRUN_GITHUB_CLIENT_ID", CLIENT_ID.to_string()),
        ]
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    pub fn requests_to(&self, path: &str) -> Vec<Request> {
        self.requests().into_iter().filter(|r| r.path == path).collect()
    }
}

fn answer(
    stream: TcpStream,
    record: &Mutex<Vec<Request>>,
    token_requests: &Mutex<usize>,
    behaviour: Behaviour,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    let mut length = 0usize;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            let (name, value) = (name.trim().to_ascii_lowercase(), value.trim().to_string());
            if name == "content-length" {
                length = value.parse().unwrap_or(0);
            }
            headers.push((name, value));
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let request = Request {
        method,
        path: path.split('?').next().unwrap_or("").to_string(),
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    };
    record.lock().unwrap().push(request.clone());

    let (status, json) = match request.path.as_str() {
        "/login/device/code" => (
            "200 OK",
            serde_json::json!({
                "device_code": DEVICE_CODE,
                "user_code": USER_CODE,
                "verification_uri": VERIFICATION_URI,
                "expires_in": 900,
                "interval": behaviour.interval,
            }),
        ),
        "/login/oauth/access_token" => {
            let mut seen = token_requests.lock().unwrap();
            *seen += 1;
            if behaviour.deny {
                ("200 OK", serde_json::json!({"error": "access_denied"}))
            } else if *seen <= behaviour.pending_polls {
                ("200 OK", serde_json::json!({"error": "authorization_pending"}))
            } else {
                (
                    "200 OK",
                    serde_json::json!({"access_token": ACCESS_TOKEN, "token_type": "bearer", "scope": ""}),
                )
            }
        }
        "/user" if request.header("authorization") == Some(&format!("Bearer {ACCESS_TOKEN}")) => (
            "200 OK",
            serde_json::json!({"id": behaviour.user_id, "login": behaviour.login}),
        ),
        "/user" => (
            "401 Unauthorized",
            serde_json::json!({"message": "Bad credentials"}),
        ),
        _ => ("404 Not Found", serde_json::json!({"message": "Not Found"})),
    };
    let text = json.to_string();
    let mut stream = stream;
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    )?;
    stream.flush()
}
