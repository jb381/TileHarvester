//! Strava API client with OAuth token storage and automatic refresh.

use std::fs::{self, File};
use std::io::ErrorKind;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::blocking::{Client, RequestBuilder};
use reqwest::header::HeaderMap;
use reqwest::Method;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use crate::config::Settings;
use crate::error::{classify_http_status, classify_reqwest_error, Error, Result, StravaError};

pub const TOKEN_URL: &str = "https://www.strava.com/oauth/token";
pub const API_BASE: &str = "https://www.strava.com/api/v3";
pub const MAX_RETRIES: usize = 3;
pub const RETRY_BACKOFF_BASE_SECS: f64 = 1.0;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RateLimitInfo {
    pub daily_limit: Option<i64>,
    pub daily_used: Option<i64>,
    pub fifteen_min_limit: Option<i64>,
    pub fifteen_min_used: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitStatus {
    pub ok: bool,
    pub status_code: Option<u16>,
    pub rate_limit: RateLimitInfo,
    pub error: Option<String>,
}

pub trait StravaApi {
    fn is_authenticated(&self) -> bool;
    fn build_auth_url(&self) -> Result<String>;
    fn exchange_code(&self, code: &str) -> Result<Value>;
    fn get_athlete(&self) -> Result<Value>;
    fn get_activities(
        &self,
        page: u32,
        per_page: u32,
        after: Option<i64>,
        before: Option<i64>,
    ) -> Result<Vec<Value>>;
    fn get_activity(&self, activity_id: i64) -> Result<Value>;
    fn get_activity_streams(&self, activity_id: i64, keys: &str) -> Result<Value>;
    fn update_activity_description(&self, activity_id: i64, description: &str) -> Result<Value>;
    fn get_rate_limit_status(&self) -> RateLimitStatus;
}

pub fn parse_rate_limit_headers(
    limit_header: Option<&str>,
    usage_header: Option<&str>,
) -> RateLimitInfo {
    fn pair(header: Option<&str>) -> Option<(i64, i64)> {
        let (quarter, daily) = header?.split_once(',')?;
        Some((quarter.trim().parse().ok()?, daily.trim().parse().ok()?))
    }

    let limit = pair(limit_header);
    let usage = pair(usage_header);
    RateLimitInfo {
        daily_limit: limit.map(|(_, daily)| daily),
        daily_used: usage.map(|(_, daily)| daily),
        fifteen_min_limit: limit.map(|(quarter, _)| quarter),
        fifteen_min_used: usage.map(|(quarter, _)| quarter),
    }
}

fn rate_limits(headers: &HeaderMap, limit: &str, usage: &str) -> RateLimitInfo {
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    parse_rate_limit_headers(header(limit), header(usage))
}

pub fn compute_rate_limit_delay(
    headers: &HeaderMap,
    rate_limit_buffer: i64,
    now_unix_secs: f64,
) -> f64 {
    let mut delay: f64 = 0.0;
    for (limit, usage) in [
        ("x-ratelimit-limit", "x-ratelimit-usage"),
        ("x-readratelimit-limit", "x-readratelimit-usage"),
    ] {
        let limits = rate_limits(headers, limit, usage);
        for (limit, used, seconds) in [
            (limits.fifteen_min_limit, limits.fifteen_min_used, 900.0),
            (limits.daily_limit, limits.daily_used, 86400.0),
        ] {
            if let (Some(limit), Some(used)) = (limit, used) {
                if limit > 0 && used >= limit - rate_limit_buffer.clamp(0, limit - 1) {
                    delay = delay.max(seconds - now_unix_secs.rem_euclid(seconds) + 1.0);
                }
            }
        }
    }
    delay
}

pub fn save_tokens(settings: &Settings, data: &Value) -> Result<()> {
    settings.ensure_dirs()?;
    let path = settings.token_path();
    let mut temporary = tempfile::Builder::new()
        .prefix(".strava-tokens-")
        .tempfile_in(&settings.data_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    serde_json::to_writer_pretty(temporary.as_file_mut(), data)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|err| err.error)?;
    Ok(())
}

pub fn load_tokens(settings: &Settings) -> Result<Option<Value>> {
    match File::open(settings.token_path()) {
        Ok(file) => Ok(Some(serde_json::from_reader(std::io::BufReader::new(
            file,
        ))?)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

pub fn build_auth_url(settings: &Settings) -> Result<String> {
    settings.validate_strava_credentials()?;
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", &settings.strava_client_id)
        .append_pair("redirect_uri", &settings.strava_redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", "activity:read_all,activity:write")
        .finish();
    Ok(format!("https://www.strava.com/oauth/authorize?{query}"))
}

pub fn is_authenticated(settings: &Settings) -> bool {
    load_tokens(settings).is_ok_and(|tokens| tokens.is_some())
}

pub fn classify_strava_error(err: &Error) -> StravaError {
    err.classify_strava()
}

pub struct HttpStravaClient {
    settings: Settings,
    client: Client,
    token_url: String,
    api_base: String,
    clock: Box<dyn Fn() -> f64 + Send + Sync>,
    sleeper: Box<dyn Fn(Duration) + Send + Sync>,
    token_lock: Mutex<()>,
}

struct Response {
    status: u16,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl HttpStravaClient {
    pub fn new(settings: Settings) -> Result<Self> {
        Self::with_endpoints(settings, TOKEN_URL, API_BASE)
    }

    pub fn with_endpoints(
        settings: Settings,
        token_url: impl Into<String>,
        api_base: impl Into<String>,
    ) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|err| classify_reqwest_error(&err))?;
        Ok(Self {
            settings,
            client,
            token_url: token_url.into(),
            api_base: api_base.into().trim_end_matches('/').to_owned(),
            clock: Box::new(|| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs_f64()
            }),
            sleeper: Box::new(std::thread::sleep),
            token_lock: Mutex::new(()),
        })
    }

    /// Replace the UNIX clock and blocking sleep function, for example in tests.
    pub fn with_clock_and_sleeper(
        mut self,
        clock: impl Fn() -> f64 + Send + Sync + 'static,
        sleeper: impl Fn(Duration) + Send + Sync + 'static,
    ) -> Self {
        self.clock = Box::new(clock);
        self.sleeper = Box::new(sleeper);
        self
    }

    fn request(&self, mut request: impl FnMut() -> Result<RequestBuilder>) -> Result<Response> {
        for attempt in 0.. {
            let err = match request()?.send() {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        let headers = response.headers().clone();
                        match response.bytes() {
                            Ok(body) => {
                                return Ok(Response {
                                    status: status.as_u16(),
                                    headers,
                                    body: body.to_vec(),
                                })
                            }
                            Err(err) => classify_reqwest_error(&err),
                        }
                    } else {
                        let body = response.text().unwrap_or_default();
                        let err = classify_http_status(status.as_u16(), &body);
                        if !status.is_server_error() {
                            return Err(err.into());
                        }
                        err
                    }
                }
                Err(err) if err.is_builder() => return Err(Error::Config(err.to_string())),
                Err(err) => classify_reqwest_error(&err),
            };
            if attempt == MAX_RETRIES {
                return Err(err.into());
            }
            let delay = RETRY_BACKOFF_BASE_SECS * 2_f64.powi(attempt as i32);
            eprintln!(
                "Retry {}/{MAX_RETRIES} for Strava API request after {delay:.0}s: {err}",
                attempt + 1
            );
            (self.sleeper)(Duration::from_secs_f64(delay));
        }
        unreachable!("request returns after the final attempt")
    }

    fn request_tokens(&self, grant_type: &str, field: &str, secret: &str) -> Result<Value> {
        let response = self.request(|| {
            Ok(self.client.post(&self.token_url).form(&[
                ("client_id", self.settings.strava_client_id.as_str()),
                ("client_secret", self.settings.strava_client_secret.as_str()),
                ("grant_type", grant_type),
                (field, secret),
            ]))
        })?;
        let mut tokens: Value = serde_json::from_slice(&response.body)?;
        let data = tokens
            .as_object_mut()
            .ok_or_else(|| StravaError::Data("Expected a Strava token object".into()))?;
        let expires_in = match data.get("expires_in") {
            Some(value) => value
                .as_i64()
                .ok_or_else(|| StravaError::Data("Invalid Strava expires_in value".into()))?,
            None => 21600,
        };
        let expires_at = ((self.clock)() as i64)
            .checked_add(expires_in)
            .ok_or_else(|| StravaError::Data("Invalid Strava token expiration".into()))?;
        data.insert("expires_at".into(), json!(expires_at));
        save_tokens(&self.settings, &tokens)?;
        Ok(tokens)
    }

    fn access_token(&self) -> Result<String> {
        self.settings.validate_strava_credentials()?;
        let _guard = self
            .token_lock
            .lock()
            .map_err(|err| Error::Other(err.to_string()))?;
        let mut tokens = load_tokens(&self.settings)?.ok_or(Error::NotAuthenticated)?;
        if tokens["expires_at"].as_f64().unwrap_or(0.0) < (self.clock)() + 300.0 {
            tokens = self.request_tokens(
                "refresh_token",
                "refresh_token",
                token_field(&tokens, "refresh_token")?,
            )?;
        }
        Ok(token_field(&tokens, "access_token")?.to_owned())
    }

    fn api_request(
        &self,
        method: Method,
        path: &str,
        timeout_secs: u64,
        configure: impl Fn(RequestBuilder) -> RequestBuilder,
    ) -> Result<Response> {
        let url = format!("{}{path}", self.api_base);
        self.request(|| {
            let token = self.access_token()?;
            Ok(configure(
                self.client
                    .request(method.clone(), &url)
                    .bearer_auth(token)
                    .timeout(Duration::from_secs(timeout_secs)),
            ))
        })
    }

    fn api_json<T: DeserializeOwned>(&self, response: Response) -> Result<T> {
        let delay = compute_rate_limit_delay(
            &response.headers,
            self.settings.rate_limit_buffer,
            (self.clock)(),
        );
        if delay > 0.0 {
            eprintln!("Strava rate limit close; waiting {delay:.0}s for the reset.");
            (self.sleeper)(Duration::from_secs_f64(delay));
        }
        Ok(serde_json::from_slice(&response.body)?)
    }
}

fn token_field<'a>(tokens: &'a Value, field: &str) -> Result<&'a str> {
    tokens[field].as_str().ok_or_else(|| {
        StravaError::Auth(format!(
            "Missing or invalid {field}. Run 'tileharvester auth' to re-authenticate."
        ))
        .into()
    })
}

impl StravaApi for HttpStravaClient {
    fn is_authenticated(&self) -> bool {
        is_authenticated(&self.settings)
    }

    fn build_auth_url(&self) -> Result<String> {
        build_auth_url(&self.settings)
    }

    fn exchange_code(&self, code: &str) -> Result<Value> {
        self.settings.validate_strava_credentials()?;
        let _guard = self
            .token_lock
            .lock()
            .map_err(|err| Error::Other(err.to_string()))?;
        self.request_tokens("authorization_code", "code", code)
    }

    fn get_athlete(&self) -> Result<Value> {
        self.api_json(self.api_request(Method::GET, "/athlete", 5, |request| request)?)
    }

    fn get_activities(
        &self,
        page: u32,
        per_page: u32,
        after: Option<i64>,
        before: Option<i64>,
    ) -> Result<Vec<Value>> {
        let mut params = vec![("page", i64::from(page)), ("per_page", i64::from(per_page))];
        params.extend(after.map(|value| ("after", value)));
        params.extend(before.map(|value| ("before", value)));
        self.api_json(
            self.api_request(Method::GET, "/athlete/activities", 30, |request| {
                request.query(&params)
            })?,
        )
    }

    fn get_activity(&self, activity_id: i64) -> Result<Value> {
        self.api_json(self.api_request(
            Method::GET,
            &format!("/activities/{activity_id}"),
            30,
            |request| request,
        )?)
    }

    fn get_activity_streams(&self, activity_id: i64, keys: &str) -> Result<Value> {
        self.api_json(self.api_request(
            Method::GET,
            &format!("/activities/{activity_id}/streams"),
            60,
            |request| request.query(&[("keys", keys), ("key_by_type", "true")]),
        )?)
    }

    fn update_activity_description(&self, activity_id: i64, description: &str) -> Result<Value> {
        self.api_json(self.api_request(
            Method::PUT,
            &format!("/activities/{activity_id}"),
            30,
            |request| request.json(&json!({"description": description})),
        )?)
    }

    fn get_rate_limit_status(&self) -> RateLimitStatus {
        match self.api_request(Method::GET, "/athlete", 15, |request| request) {
            Ok(response) => RateLimitStatus {
                ok: true,
                status_code: Some(response.status),
                rate_limit: rate_limits(
                    &response.headers,
                    "x-ratelimit-limit",
                    "x-ratelimit-usage",
                ),
                error: None,
            },
            Err(err) => RateLimitStatus {
                ok: false,
                status_code: None,
                rate_limit: RateLimitInfo::default(),
                error: Some(classify_strava_error(&err).to_string()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::thread::{self, JoinHandle};

    fn credentials(dir: &tempfile::TempDir) -> Settings {
        Settings {
            data_dir: dir.path().join("data"),
            strava_client_id: "client + id".into(),
            strava_client_secret: "secret".into(),
            strava_redirect_uri: "http://localhost:8000/callback?next=/a&b=c".into(),
            ..Settings::default()
        }
    }

    #[test]
    fn headers_parse_pairs_independently() {
        assert_eq!(
            parse_rate_limit_headers(Some(" 200, 2000 "), Some("12,300")),
            RateLimitInfo {
                daily_limit: Some(2000),
                daily_used: Some(300),
                fifteen_min_limit: Some(200),
                fifteen_min_used: Some(12),
            }
        );
        assert_eq!(
            parse_rate_limit_headers(Some("200,bad"), Some("12")),
            RateLimitInfo::default()
        );
    }

    #[test]
    fn respects_both_limits_and_the_later_utc_reset() {
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-limit", "200,2000".parse().unwrap());
        headers.insert("x-ratelimit-usage", "195,300".parse().unwrap());
        assert_eq!(compute_rate_limit_delay(&headers, 10, 100.0), 801.0);
        assert_eq!(compute_rate_limit_delay(&headers, -1, 100.0), 0.0);
        headers.insert("x-readratelimit-limit", "100,1000".parse().unwrap());
        headers.insert("x-readratelimit-usage", "95,300".parse().unwrap());
        headers.remove("x-ratelimit-usage");
        assert_eq!(compute_rate_limit_delay(&headers, 10, 100.0), 801.0);
        headers.insert("x-ratelimit-usage", "195,1995".parse().unwrap());
        assert_eq!(compute_rate_limit_delay(&headers, 10, 100.0), 86301.0);
        headers.remove("x-ratelimit-usage");
        headers.insert("x-readratelimit-usage", "95,995".parse().unwrap());
        assert_eq!(compute_rate_limit_delay(&headers, 10, 100.0), 86301.0);
        headers.insert("x-readratelimit-usage", "0,0".parse().unwrap());
        assert_eq!(compute_rate_limit_delay(&headers, i64::MAX, 100.0), 0.0);
    }

    #[cfg(unix)]
    #[test]
    fn token_save_uses_private_atomic_replacement_and_cleans_up_failure() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let settings = credentials(&dir);
        let path = settings.token_path();
        let old = json!({"refresh_token": "old"});
        let new = json!({"refresh_token": "new"});
        assert!(!is_authenticated(&settings));
        save_tokens(&settings, &old).unwrap();
        assert!(is_authenticated(&settings));
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        let old_file = File::open(&path).unwrap();
        save_tokens(&settings, &new).unwrap();
        assert_eq!(serde_json::from_reader::<_, Value>(old_file).unwrap(), old);
        assert_eq!(load_tokens(&settings).unwrap(), Some(new));

        let backup = path.with_extension("backup");
        fs::rename(&path, &backup).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(save_tokens(&settings, &old).is_err());
        assert!(path.is_dir());
        assert_eq!(fs::read_dir(&settings.data_dir).unwrap().count(), 2);
        assert_eq!(
            serde_json::from_reader::<_, Value>(File::open(backup).unwrap()).unwrap(),
            json!({"refresh_token": "new"})
        );
    }

    #[test]
    fn authorization_url_encodes_parameters_and_requires_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let settings = credentials(&dir);
        let url = url::Url::parse(&build_auth_url(&settings).unwrap()).unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("www.strava.com"));
        assert_eq!(url.path(), "/oauth/authorize");
        let pairs: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(pairs.len(), 4);
        assert_eq!(pairs["client_id"], settings.strava_client_id);
        assert_eq!(pairs["redirect_uri"], settings.strava_redirect_uri);
        assert_eq!(pairs["response_type"], "code");
        assert_eq!(pairs["scope"], "activity:read_all,activity:write");
        assert!(matches!(
            build_auth_url(&Settings::default()),
            Err(Error::Config(_))
        ));
    }

    #[test]
    fn classifies_application_errors() {
        let auth = Error::NotAuthenticated;
        assert!(matches!(classify_strava_error(&auth), StravaError::Auth(_)));
        let rate_limit = StravaError::RateLimit("custom".into());
        assert_eq!(
            classify_strava_error(&rate_limit.clone().into()),
            rate_limit
        );
        assert!(matches!(
            classify_strava_error(&Error::Validation("invalid".into())),
            StravaError::Other(_)
        ));
    }

    fn serve(responses: Vec<String>) -> (String, JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            responses
                .into_iter()
                .map(|response| {
                    let (mut socket, _) = listener.accept().unwrap();
                    socket
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    let mut reader = BufReader::new(&mut socket);
                    let mut request = String::new();
                    let mut content_length = 0;
                    loop {
                        let mut line = String::new();
                        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                        request.push_str(&line);
                        if let Some(value) =
                            line.to_ascii_lowercase().strip_prefix("content-length:")
                        {
                            content_length = value.trim().parse().unwrap();
                        }
                        if line == "\r\n" {
                            break;
                        }
                    }
                    let mut body = vec![0; content_length];
                    reader.read_exact(&mut body).unwrap();
                    request.push_str(std::str::from_utf8(&body).unwrap());
                    socket.write_all(response.as_bytes()).unwrap();
                    request
                })
                .collect()
        });
        (base, server)
    }

    fn response(status: u16, headers: &str, body: &str) -> String {
        format!("HTTP/1.1 {status} Test\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n{body}", body.len())
    }

    fn mock_client(settings: Settings, base: &str) -> (HttpStravaClient, Arc<Mutex<Vec<u64>>>) {
        let sleeps = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&sleeps);
        let client = HttpStravaClient::with_endpoints(settings, format!("{base}/token"), base)
            .unwrap()
            .with_clock_and_sleeper(
                || 100.0,
                move |delay| recorded.lock().unwrap().push(delay.as_secs()),
            );
        (client, sleeps)
    }

    #[test]
    fn retries_server_errors_and_interrupted_responses() {
        let dir = tempfile::tempdir().unwrap();
        let settings = credentials(&dir);
        save_tokens(&settings, &json!({"access_token": "ok", "expires_at": 400})).unwrap();
        let (base, server) = serve(vec![
            response(500, "", "{}"),
            "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 100\r\n\r\n{}".into(),
            response(503, "", "{}"),
            response(
                200,
                "X-ReadRateLimit-Limit: 100,1000\r\nX-ReadRateLimit-Usage: 95,300\r\n",
                "{\"id\":42}",
            ),
        ]);
        let (client, sleeps) = mock_client(settings, &base);
        assert_eq!(client.get_athlete().unwrap(), json!({"id": 42}));
        assert_eq!(*sleeps.lock().unwrap(), vec![1, 2, 4, 801]);
        let requests = server.join().unwrap();
        assert!(requests
            .iter()
            .all(|request| request.starts_with("GET /athlete ")
                && request.contains("authorization: Bearer ok")));
    }

    #[test]
    fn client_errors_are_classified_without_retry() {
        let dir = tempfile::tempdir().unwrap();
        let settings = credentials(&dir);
        save_tokens(&settings, &json!({"access_token": "ok", "expires_at": 400})).unwrap();
        for status in [400, 401, 403, 429] {
            let (base, server) = serve(vec![response(status, "", "failure")]);
            let (client, sleeps) = mock_client(settings.clone(), &base);
            let error = classify_strava_error(&client.get_activity(42).unwrap_err());
            assert_eq!(error, classify_http_status(status, "failure"));
            assert!(sleeps.lock().unwrap().is_empty());
            assert!(server.join().unwrap()[0].starts_with("GET /activities/42 "));
        }
    }

    #[test]
    fn exhausts_retries_and_returns_server_error() {
        let dir = tempfile::tempdir().unwrap();
        let settings = credentials(&dir);
        save_tokens(&settings, &json!({"access_token": "ok", "expires_at": 400})).unwrap();
        let (base, server) = serve(vec![response(503, "", "{}"); 4]);
        let (client, sleeps) = mock_client(settings, &base);
        assert!(matches!(
            client.get_athlete(),
            Err(Error::Strava(StravaError::Server(_)))
        ));
        assert_eq!(*sleeps.lock().unwrap(), vec![1, 2, 4]);
        assert_eq!(server.join().unwrap().len(), 4);
    }

    #[test]
    fn refreshes_and_persists_before_requesting_rate_limit_status() {
        let dir = tempfile::tempdir().unwrap();
        let settings = credentials(&dir);
        save_tokens(
            &settings,
            &json!({"access_token": "old", "refresh_token": "old refresh", "expires_at": 399}),
        )
        .unwrap();
        let (base, server) = serve(vec![
            response(
                200,
                "",
                r#"{"access_token":"new","refresh_token":"new refresh"}"#,
            ),
            response(
                200,
                "X-RateLimit-Limit: 200,2000\r\nX-RateLimit-Usage: 200,100\r\n",
                "{}",
            ),
        ]);
        let (client, sleeps) = mock_client(settings.clone(), &base);
        let status = client.get_rate_limit_status();
        assert!(status.ok);
        assert_eq!(status.status_code, Some(200));
        assert_eq!(status.rate_limit.daily_limit, Some(2000));
        assert_eq!(status.rate_limit.fifteen_min_used, Some(200));
        assert!(sleeps.lock().unwrap().is_empty());
        assert_eq!(
            load_tokens(&settings).unwrap().unwrap(),
            json!({"access_token": "new", "refresh_token": "new refresh", "expires_at": 21700})
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /token "));
        assert!(requests[0].contains("grant_type=refresh_token&refresh_token=old+refresh"));
        assert!(requests[1].contains("authorization: Bearer new"));
    }

    #[test]
    fn exchanges_code_and_persists_calculated_expiration() {
        let dir = tempfile::tempdir().unwrap();
        let settings = credentials(&dir);
        let (base, server) = serve(vec![response(
            200,
            "",
            r#"{"access_token":"new","refresh_token":"refresh","expires_in":1234,"expires_at":0}"#,
        )]);
        let (client, sleeps) = mock_client(settings.clone(), &base);
        let tokens = client.exchange_code("a+b").unwrap();
        assert_eq!(tokens["expires_at"], 1334);
        assert_eq!(load_tokens(&settings).unwrap(), Some(tokens));
        assert!(sleeps.lock().unwrap().is_empty());
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /token "));
        assert!(requests[0].ends_with(
            "client_id=client+%2B+id&client_secret=secret&grant_type=authorization_code&code=a%2Bb"
        ));
    }
}
