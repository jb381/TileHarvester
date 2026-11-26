//! In-memory and loopback HTTP implementations of the same Strava fixtures.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use chrono::DateTime;
use serde_json::{json, Value};
use tileharvester::error::{classify_http_status, Result, StravaError};
use tileharvester::strava_client::{RateLimitInfo, RateLimitStatus, StravaApi};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivityRequest {
    pub page: u32,
    pub per_page: u32,
    pub after: Option<i64>,
    pub before: Option<i64>,
}

#[derive(Debug, Default)]
struct Calls {
    activity_requests: Vec<ActivityRequest>,
    stream_calls: Vec<(i64, String)>,
    activity_details_requests: Vec<i64>,
    annotation_writes: Vec<(i64, String)>,
    descriptions: HashMap<i64, String>,
}

pub struct MockStravaApi {
    authenticated: bool,
    activities: Vec<Value>,
    short_pages: HashMap<u32, usize>,
    streams: HashMap<i64, Value>,
    metadata: HashMap<i64, Value>,
    athlete: Value,
    oauth_tokens: Value,
    rate_limit: RateLimitStatus,
    fail_fetch: bool,
    fail_streams: HashSet<i64>,
    fail_annotate: bool,
    fail_update_description: bool,
    calls: Mutex<Calls>,
}

impl Default for MockStravaApi {
    fn default() -> Self {
        Self {
            authenticated: true,
            activities: Vec::new(),
            short_pages: HashMap::new(),
            streams: HashMap::new(),
            metadata: HashMap::new(),
            athlete: json!({"id": 1, "firstname": "Test", "lastname": "Athlete"}),
            oauth_tokens: json!({
                "access_token": "mock-access", "refresh_token": "mock-refresh", "expires_in": 21600
            }),
            rate_limit: RateLimitStatus {
                ok: true,
                status_code: Some(200),
                rate_limit: RateLimitInfo::default(),
                error: None,
            },
            fail_fetch: false,
            fail_streams: HashSet::new(),
            fail_annotate: false,
            fail_update_description: false,
            calls: Mutex::new(Calls::default()),
        }
    }
}

impl MockStravaApi {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_authenticated(mut self, authenticated: bool) -> Self {
        self.authenticated = authenticated;
        self
    }

    /// Activities are paginated in the supplied order, typically newest first.
    pub fn with_activities(mut self, activities: Vec<Value>) -> Self {
        self.activities = activities;
        self
    }

    /// Limit the returned items without changing subsequent page offsets.
    pub fn with_short_page(mut self, page: u32, len: usize) -> Self {
        self.short_pages.insert(page, len);
        self
    }

    pub fn with_streams(mut self, id: i64, streams: Value) -> Self {
        self.streams.insert(id, streams);
        self
    }

    pub fn with_activity(mut self, id: i64, metadata: Value) -> Self {
        self.metadata.insert(id, metadata);
        self
    }

    pub fn with_description(mut self, id: i64, description: impl Into<String>) -> Self {
        self.calls
            .get_mut()
            .unwrap()
            .descriptions
            .insert(id, description.into());
        self
    }

    pub fn with_athlete(mut self, athlete: Value) -> Self {
        self.athlete = athlete;
        self
    }

    pub fn with_oauth_tokens(mut self, tokens: Value) -> Self {
        self.oauth_tokens = tokens;
        self
    }

    pub fn with_rate_limit_status(mut self, status: RateLimitStatus) -> Self {
        self.rate_limit = status;
        self
    }

    pub fn fail_fetch(mut self) -> Self {
        self.fail_fetch = true;
        self
    }

    pub fn fail_streams_for(mut self, id: i64) -> Self {
        self.fail_streams.insert(id);
        self
    }

    /// Fail the detail lookup performed before annotation.
    pub fn fail_annotate(mut self) -> Self {
        self.fail_annotate = true;
        self
    }

    pub fn fail_update_description(mut self) -> Self {
        self.fail_update_description = true;
        self
    }

    pub fn activity_requests(&self) -> Vec<ActivityRequest> {
        self.calls.lock().unwrap().activity_requests.clone()
    }

    pub fn stream_calls(&self) -> Vec<(i64, String)> {
        self.calls.lock().unwrap().stream_calls.clone()
    }

    pub fn activity_details_requests(&self) -> Vec<i64> {
        self.calls.lock().unwrap().activity_details_requests.clone()
    }

    /// Includes attempted writes that fail.
    pub fn annotation_writes(&self) -> Vec<(i64, String)> {
        self.calls.lock().unwrap().annotation_writes.clone()
    }

    pub fn current_descriptions(&self) -> HashMap<i64, String> {
        let mut descriptions: HashMap<_, _> = self
            .activities
            .iter()
            .filter_map(|activity| {
                Some((
                    activity["id"].as_i64()?,
                    activity["description"].as_str()?.to_owned(),
                ))
            })
            .collect();
        descriptions.extend(self.metadata.iter().filter_map(|(&id, activity)| {
            Some((id, activity["description"].as_str()?.to_owned()))
        }));
        descriptions.extend(self.calls.lock().unwrap().descriptions.clone());
        descriptions
    }

    fn activity_details(&self, id: i64) -> Value {
        let mut activity = self
            .metadata
            .get(&id)
            .or_else(|| {
                self.activities
                    .iter()
                    .find(|entry| entry["id"].as_i64() == Some(id))
            })
            .cloned()
            .unwrap_or_else(|| json!({"id": id}));
        if let Some(description) = self.calls.lock().unwrap().descriptions.get(&id) {
            activity["description"] = json!(description);
        }
        activity
    }
}

fn failure(message: &str) -> tileharvester::error::Error {
    classify_http_status(500, message).into()
}

impl StravaApi for MockStravaApi {
    fn is_authenticated(&self) -> bool {
        self.authenticated
    }

    fn build_auth_url(&self) -> Result<String> {
        Ok("https://www.strava.com/oauth/authorize?client_id=mock".into())
    }

    fn exchange_code(&self, code: &str) -> Result<Value> {
        if code.is_empty() {
            return Err(StravaError::Auth("missing mock authorization code".into()).into());
        }
        Ok(self.oauth_tokens.clone())
    }

    fn get_athlete(&self) -> Result<Value> {
        Ok(self.athlete.clone())
    }

    fn get_activities(
        &self,
        page: u32,
        per_page: u32,
        after: Option<i64>,
        before: Option<i64>,
    ) -> Result<Vec<Value>> {
        self.calls
            .lock()
            .unwrap()
            .activity_requests
            .push(ActivityRequest {
                page,
                per_page,
                after,
                before,
            });
        if self.fail_fetch {
            return Err(failure("mock activity fetch failed"));
        }
        let limit = per_page as usize;
        Ok(self
            .activities
            .iter()
            .filter(|activity| {
                if after.is_none() && before.is_none() {
                    return true;
                }
                activity["start_date"]
                    .as_str()
                    .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                    .is_some_and(|time| {
                        let time = time.timestamp();
                        after.is_none_or(|bound| time > bound)
                            && before.is_none_or(|bound| time < bound)
                    })
            })
            .skip((page.saturating_sub(1) as usize).saturating_mul(limit))
            .take(
                self.short_pages
                    .get(&page)
                    .copied()
                    .unwrap_or(limit)
                    .min(limit),
            )
            .cloned()
            .collect())
    }

    fn get_activity(&self, id: i64) -> Result<Value> {
        self.calls
            .lock()
            .unwrap()
            .activity_details_requests
            .push(id);
        if self.fail_annotate {
            return Err(failure("mock activity detail fetch failed"));
        }
        Ok(self.activity_details(id))
    }

    fn get_activity_streams(&self, id: i64, keys: &str) -> Result<Value> {
        self.calls
            .lock()
            .unwrap()
            .stream_calls
            .push((id, keys.to_owned()));
        if self.fail_streams.contains(&id) {
            return Err(failure("mock stream fetch failed"));
        }
        let Some(streams) = self.streams.get(&id).and_then(Value::as_object) else {
            return Ok(json!({}));
        };
        Ok(Value::Object(
            keys.split(',')
                .map(str::trim)
                .filter_map(|key| Some((key.to_owned(), streams.get(key)?.clone())))
                .collect(),
        ))
    }

    fn update_activity_description(&self, id: i64, description: &str) -> Result<Value> {
        let mut calls = self.calls.lock().unwrap();
        calls.annotation_writes.push((id, description.to_owned()));
        if self.fail_update_description {
            return Err(failure("mock description update failed"));
        }
        calls.descriptions.insert(id, description.to_owned());
        drop(calls);
        Ok(self.activity_details(id))
    }

    fn get_rate_limit_status(&self) -> RateLimitStatus {
        self.rate_limit.clone()
    }
}

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub body: String,
}

impl HttpRequest {
    pub fn form(&self) -> HashMap<String, String> {
        decode_form(&self.body)
    }
}

fn decode_form(value: &str) -> HashMap<String, String> {
    url::form_urlencoded::parse(value.as_bytes())
        .into_owned()
        .collect()
}

#[derive(Default)]
pub struct MockStravaServerBuilder {
    api: Arc<MockStravaApi>,
    headers: Vec<(String, String)>,
    transient: HashMap<(String, String), (usize, u16)>,
    statuses: HashMap<(String, String), u16>,
    refresh: Option<(String, Value)>,
    authorization_code: Option<String>,
    bearer: Option<String>,
}

impl MockStravaServerBuilder {
    pub fn with_api(mut self, api: MockStravaApi) -> Self {
        self.api = Arc::new(api);
        self
    }

    pub fn with_rate_limit_headers(mut self, limits: &str, usage: &str) -> Self {
        self.headers.extend([
            ("X-RateLimit-Limit".into(), limits.into()),
            ("X-RateLimit-Usage".into(), usage.into()),
        ]);
        self
    }

    pub fn with_read_rate_limit_headers(mut self, limits: &str, usage: &str) -> Self {
        self.headers.extend([
            ("X-ReadRateLimit-Limit".into(), limits.into()),
            ("X-ReadRateLimit-Usage".into(), usage.into()),
        ]);
        self
    }

    pub fn with_transient_failures(
        mut self,
        method: &str,
        path: &str,
        count: usize,
        status: u16,
    ) -> Self {
        assert!((500..600).contains(&status));
        self.transient
            .insert((method.into(), path.into()), (count, status));
        self
    }

    pub fn with_status(mut self, method: &str, path: &str, status: u16) -> Self {
        self.statuses.insert((method.into(), path.into()), status);
        self
    }

    pub fn with_refresh(
        mut self,
        expected_refresh_token: impl Into<String>,
        response: Value,
    ) -> Self {
        self.refresh = Some((expected_refresh_token.into(), response));
        self
    }

    pub fn with_authorization_code(mut self, code: impl Into<String>) -> Self {
        self.authorization_code = Some(code.into());
        self
    }

    pub fn with_bearer_token(mut self, access_token: impl Into<String>) -> Self {
        self.bearer = Some(format!("Bearer {}", access_token.into()));
        self
    }

    pub fn start(mut self) -> io::Result<MockStravaServer> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let address = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let worker_stop = Arc::clone(&stop);
        let worker_requests = Arc::clone(&requests);
        let api = Arc::clone(&self.api);
        let worker = thread::Builder::new()
            .name("mock-strava-http".into())
            .spawn(move || {
                for connection in listener.incoming() {
                    if worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(mut stream) = connection else {
                        continue;
                    };
                    let timeout = Some(Duration::from_secs(2));
                    let result = stream
                        .set_read_timeout(timeout)
                        .and_then(|()| stream.set_write_timeout(timeout))
                        .and_then(|()| read_request(&mut stream));
                    if let Ok(request) = result {
                        let (status, body) = self.respond(&request);
                        worker_requests.lock().unwrap().push(request);
                        let _ = write_response(&mut stream, status, &self.headers, &body);
                    }
                }
            })?;
        Ok(MockStravaServer {
            address,
            stop,
            requests,
            api,
            worker: Some(worker),
        })
    }

    fn respond(&mut self, request: &HttpRequest) -> (u16, Value) {
        let key = (request.method.clone(), request.path.clone());
        if let Some((remaining, status)) = self.transient.get_mut(&key) {
            if *remaining > 0 {
                *remaining -= 1;
                return http_error(*status, "mock transient failure");
            }
        }
        if let Some(&status) = self.statuses.get(&key) {
            return http_error(status, "configured mock status");
        }
        if request.path == "/oauth/token" && request.method == "POST" {
            return self.token_response(&request.form());
        }
        if self
            .bearer
            .as_ref()
            .is_some_and(|token| request.headers.get("authorization") != Some(token))
        {
            return http_error(401, "invalid mock bearer token");
        }
        let result = match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/api/v3/athlete") => self.api.get_athlete(),
            ("GET", "/api/v3/athlete/activities") => {
                let query = &request.query;
                let number = |name: &str| query.get(name).and_then(|value| value.parse().ok());
                let bound = |name: &str| query.get(name).and_then(|value| value.parse().ok());
                self.api
                    .get_activities(
                        number("page").unwrap_or(1),
                        number("per_page").unwrap_or(30),
                        bound("after"),
                        bound("before"),
                    )
                    .map(Value::Array)
            }
            (_, path) => {
                let Some(activity) = path.strip_prefix("/api/v3/activities/") else {
                    return http_error(404, "unknown mock route");
                };
                let (id, suffix) = activity.split_once('/').unwrap_or((activity, ""));
                let Ok(id) = id.parse() else {
                    return http_error(404, "invalid mock activity id");
                };
                match (request.method.as_str(), suffix) {
                    ("GET", "") => self.api.get_activity(id),
                    ("GET", "streams") => self.api.get_activity_streams(
                        id,
                        request.query.get("keys").map_or("", String::as_str),
                    ),
                    ("PUT", "") => {
                        let body = serde_json::from_str::<Value>(&request.body).unwrap_or_default();
                        let Some(description) = body["description"].as_str() else {
                            return http_error(400, "missing mock description");
                        };
                        self.api.update_activity_description(id, description)
                    }
                    _ => return http_error(404, "unknown mock activity route"),
                }
            }
        };
        match result {
            Ok(body) => (200, body),
            Err(error) => http_error(500, &error.to_string()),
        }
    }

    fn token_response(&self, form: &HashMap<String, String>) -> (u16, Value) {
        match form.get("grant_type").map(String::as_str) {
            Some("authorization_code") => {
                let code = form.get("code");
                if self
                    .authorization_code
                    .as_ref()
                    .is_some_and(|expected| code != Some(expected))
                {
                    return http_error(401, "invalid mock authorization code");
                }
                match self.api.exchange_code(code.map_or("", String::as_str)) {
                    Ok(tokens) => (200, tokens),
                    Err(error) => http_error(400, &error.to_string()),
                }
            }
            Some("refresh_token") => {
                let presented = form.get("refresh_token");
                match &self.refresh {
                    Some((expected, response)) if presented == Some(expected) => {
                        (200, response.clone())
                    }
                    None if presented.is_some_and(|value| !value.is_empty()) => {
                        (200, self.api.oauth_tokens.clone())
                    }
                    _ => http_error(401, "invalid mock refresh token"),
                }
            }
            _ => http_error(400, "unknown mock grant type"),
        }
    }
}

pub struct MockStravaServer {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<HttpRequest>>>,
    api: Arc<MockStravaApi>,
    worker: Option<JoinHandle<()>>,
}

impl MockStravaServer {
    pub fn builder() -> MockStravaServerBuilder {
        MockStravaServerBuilder::default()
    }

    pub fn token_url(&self) -> String {
        format!("http://{}/oauth/token", self.address)
    }

    pub fn api_base(&self) -> String {
        format!("http://{}/api/v3", self.address)
    }

    pub fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub fn api(&self) -> &MockStravaApi {
        &self.api
    }
}

impl Drop for MockStravaServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_secs(1));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> io::Result<HttpRequest> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid mock HTTP request");
    let mut fields = line.split_whitespace();
    let method = fields.next().ok_or_else(invalid)?.to_owned();
    let target = fields.next().ok_or_else(invalid)?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let path = path.to_owned();
    let query = decode_form(query);
    let mut headers = HashMap::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err(invalid());
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        let (key, value) = line.split_once(':').ok_or_else(invalid)?;
        headers.insert(key.to_ascii_lowercase(), value.trim().to_owned());
    }
    let length = match headers.get("content-length") {
        Some(value) => value.parse().map_err(|_| invalid())?,
        None => 0,
    };
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let body = String::from_utf8(body).map_err(|_| invalid())?;
    Ok(HttpRequest {
        method,
        path,
        query,
        headers,
        body,
    })
}

fn http_error(status: u16, message: &str) -> (u16, Value) {
    (status, json!({"message": message}))
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    headers: &[(String, String)],
    body: &Value,
) -> io::Result<()> {
    let body = body.to_string();
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Mock Response",
    };
    write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n", body.len())?;
    for (key, value) in headers {
        write!(stream, "{key}: {value}\r\n")?;
    }
    write!(stream, "\r\n{body}")
}
