"""Strava API client with OAuth and token refresh."""

import json
import os
import tempfile
import time
from collections.abc import Callable
from pathlib import Path
from typing import Any
from urllib.parse import urlencode

import httpx

from tileharvester.config import settings

TOKEN_URL = "https://www.strava.com/oauth/token"
API_BASE = "https://www.strava.com/api/v3"
MAX_RETRIES = 3
RETRY_BACKOFF_BASE = 1.0  # seconds, doubles each retry


def _request_with_retry(
    request_fn: Callable[[], httpx.Response],
    description: str = "API request",
) -> httpx.Response:
    """Execute an httpx request with exponential backoff retry for transient failures.

    Retries on: 5xx server errors, network errors, timeouts.
    Does NOT retry on: 4xx client errors (including 429 rate limits).
    """
    last_error: Exception | None = None
    for attempt in range(MAX_RETRIES + 1):
        try:
            response = request_fn()
            if response.status_code >= 500:
                response.read()  # consume body before raising
                raise httpx.HTTPStatusError(
                    f"Server error: {response.status_code}",
                    request=response.request,
                    response=response,
                )
            response.raise_for_status()
            return response
        except (httpx.HTTPStatusError, httpx.TransportError) as e:
            last_error = e
            if isinstance(e, httpx.HTTPStatusError) and e.response.status_code < 500:
                raise
            if attempt < MAX_RETRIES:
                delay = RETRY_BACKOFF_BASE * (2**attempt)
                print(
                    f"Retry {attempt + 1}/{MAX_RETRIES} for {description} after {delay:.0f}s: {e}"
                )
                time.sleep(delay)
    if last_error is not None:
        raise last_error
    raise RuntimeError(f"{description} failed without an error")


class StravaError(Exception):
    """Base error for Strava API issues."""


class StravaAuthError(StravaError):
    """Authentication or authorization error (401, token issues)."""


class StravaRateLimitError(StravaError):
    """Rate limit exceeded (429 or close to limit)."""


class StravaNetworkError(StravaError):
    """Network-level error (connection, timeout)."""


class StravaDataError(StravaError):
    """Data error (4xx not auth, bad request)."""


class StravaServerError(StravaError):
    """Strava server error (5xx)."""


def classify_strava_error(e: Exception) -> StravaError:
    """Classify an exception from a Strava API call into a user-friendly error."""
    if isinstance(e, StravaError):
        return e

    if isinstance(e, httpx.HTTPStatusError):
        status = e.response.status_code
        if status == 401:
            return StravaAuthError(
                "Authentication failed. Your token may be expired or revoked. "
                "Run 'tileharvester auth' to re-authenticate."
            )
        if status == 403:
            return StravaAuthError(
                "Access denied. Your app may not have the required permissions. "
                "Re-authenticate with 'tileharvester auth' and ensure 'activity:read_all' scope."
            )
        if status == 429:
            return StravaRateLimitError(
                "Strava rate limit exceeded. Wait 15 minutes before retrying."
            )
        if 400 <= status < 500:
            return StravaDataError(f"Strava API error ({status}): {e.response.text[:500]}")
        if status >= 500:
            return StravaServerError(
                f"Strava server error ({status}). The service may be temporarily unavailable. "
                "Retry later or check status.strava.com."
            )
        return StravaError(f"HTTP {status}: {e.response.text[:500]}")

    if isinstance(e, httpx.ConnectError | httpx.ConnectTimeout):
        return StravaNetworkError("Cannot connect to Strava API. Check your internet connection.")
    if isinstance(e, httpx.ReadTimeout):
        return StravaNetworkError(
            "Strava API request timed out. The server may be slow. Try again."
        )
    if isinstance(e, httpx.NetworkError):
        return StravaNetworkError(f"Network error communicating with Strava API: {e}")

    if isinstance(e, RuntimeError) and "Not authenticated" in str(e):
        return StravaAuthError("Not authenticated. Run 'tileharvester auth' first.")

    return StravaError(str(e))


def _parse_rate_limit_headers(headers: httpx.Headers) -> dict[str, int | None]:
    """Parse Strava rate-limit headers.

    Strava sends values in ``15-minute,daily`` order, for example
    ``X-RateLimit-Limit: 200,2000`` and ``X-RateLimit-Usage: 12,300``.
    """
    limit = headers.get("X-RateLimit-Limit", "")
    usage = headers.get("X-RateLimit-Usage", "")

    def _split_pair(value: str) -> tuple[int | None, int | None]:
        parts = [part.strip() for part in value.split(",", 1)]
        if len(parts) != 2:
            return None, None
        try:
            return int(parts[0]), int(parts[1])
        except ValueError:
            return None, None

    fifteen_min_limit, daily_limit = _split_pair(limit)
    fifteen_min_used, daily_used = _split_pair(usage)
    return {
        "daily_limit": daily_limit,
        "daily_used": daily_used,
        "fifteen_min_limit": fifteen_min_limit,
        "fifteen_min_used": fifteen_min_used,
    }


def _token_file() -> Path:
    return settings.token_path


def _save_tokens(data: dict[str, Any]) -> None:
    settings.ensure_dirs()
    path = _token_file()
    # mkstemp uses mode 0600; atomic replacement preserves the old refresh token
    # if serialization or writing fails, and never exposes partially written JSON.
    payload = json.dumps(data, indent=2)
    descriptor, temporary = tempfile.mkstemp(prefix=".strava-tokens-", dir=path.parent)
    try:
        with os.fdopen(descriptor, "w") as output:
            output.write(payload)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
    finally:
        Path(temporary).unlink(missing_ok=True)


def _load_tokens() -> dict[str, Any] | None:
    if not _token_file().exists():
        return None
    return json.loads(_token_file().read_text())  # type: ignore[no-any-return]


def build_auth_url() -> str:
    settings.validate_strava_credentials()
    scopes = "activity:read_all,activity:write"
    query = urlencode(
        {
            "client_id": settings.strava_client_id,
            "redirect_uri": settings.strava_redirect_uri,
            "response_type": "code",
            "scope": scopes,
        }
    )
    return f"https://www.strava.com/oauth/authorize?{query}"


def exchange_code(code: str) -> dict[str, Any]:
    settings.validate_strava_credentials()
    resp = _request_with_retry(
        lambda: httpx.post(
            TOKEN_URL,
            data={
                "client_id": settings.strava_client_id,
                "client_secret": settings.strava_client_secret,
                "code": code,
                "grant_type": "authorization_code",
            },
        ),
        description="Token exchange",
    )
    data: dict[str, Any] = resp.json()
    data["expires_at"] = int(time.time()) + data.get("expires_in", 21600)
    _save_tokens(data)
    return data


def _refresh_if_needed() -> dict[str, Any]:
    settings.validate_strava_credentials()
    tokens = _load_tokens()
    if tokens is None:
        raise RuntimeError("Not authenticated. Run 'tileharvester auth' first.")

    if tokens.get("expires_at", 0) < time.time() + 300:
        refresh_token = tokens["refresh_token"]
        resp = _request_with_retry(
            lambda: httpx.post(
                TOKEN_URL,
                data={
                    "client_id": settings.strava_client_id,
                    "client_secret": settings.strava_client_secret,
                    "refresh_token": refresh_token,
                    "grant_type": "refresh_token",
                },
            ),
            description="Token refresh",
        )
        data = resp.json()
        data["expires_at"] = int(time.time()) + data.get("expires_in", 21600)
        _save_tokens(data)
        tokens = data

    return tokens


def _headers() -> dict[str, Any]:
    tokens = _refresh_if_needed()
    return {"Authorization": f"Bearer {tokens['access_token']}"}


def _rate_limit_sleep(response: httpx.Response) -> None:
    """Respect both overall and non-upload limits at their actual UTC resets."""
    now = time.time()
    delay = 0.0
    for prefix in ("X-RateLimit", "X-ReadRateLimit"):
        parsed = _parse_rate_limit_headers(
            httpx.Headers(
                {
                    "X-RateLimit-Limit": response.headers.get(f"{prefix}-Limit", ""),
                    "X-RateLimit-Usage": response.headers.get(f"{prefix}-Usage", ""),
                }
            )
        )
        for window, seconds in (("fifteen_min", 900), ("daily", 86400)):
            limit, used = parsed[f"{window}_limit"], parsed[f"{window}_used"]
            if limit is not None and used is not None and limit > 0:
                buffer = min(max(settings.rate_limit_buffer, 0), limit - 1)
                if used >= limit - buffer:
                    delay = max(delay, seconds - now % seconds + 1)
    if delay:
        print(f"Strava rate limit close; waiting {delay:.0f}s for the reset.")
        time.sleep(delay)


def get_athlete() -> dict[str, Any]:
    resp = _request_with_retry(
        lambda: httpx.get(f"{API_BASE}/athlete", headers=_headers()),
        description="get athlete",
    )
    _rate_limit_sleep(resp)
    return resp.json()  # type: ignore[no-any-return]


def get_activities(
    page: int = 1, per_page: int = 200, after: int | None = None, before: int | None = None
) -> list[dict[str, Any]]:
    params = {"page": page, "per_page": per_page}
    if after is not None:
        params["after"] = after
    if before is not None:
        params["before"] = before
    resp = _request_with_retry(
        lambda: httpx.get(
            f"{API_BASE}/athlete/activities", headers=_headers(), params=params, timeout=30
        ),
        description="get activities",
    )
    _rate_limit_sleep(resp)
    return resp.json()  # type: ignore[no-any-return]


def get_activity(activity_id: int) -> dict[str, Any]:
    resp = _request_with_retry(
        lambda: httpx.get(f"{API_BASE}/activities/{activity_id}", headers=_headers(), timeout=30),
        description=f"get activity {activity_id}",
    )
    _rate_limit_sleep(resp)
    return resp.json()  # type: ignore[no-any-return]


def get_activity_streams(activity_id: int, keys: str = "latlng") -> dict[str, Any]:
    resp = _request_with_retry(
        lambda: httpx.get(
            f"{API_BASE}/activities/{activity_id}/streams",
            headers=_headers(),
            params={"keys": keys, "key_by_type": "true"},
            timeout=60,
        ),
        description=f"get streams for activity {activity_id}",
    )
    _rate_limit_sleep(resp)
    return resp.json()  # type: ignore[no-any-return]


def update_activity_description(activity_id: int, description: str) -> dict[str, Any]:
    resp = _request_with_retry(
        lambda: httpx.put(
            f"{API_BASE}/activities/{activity_id}",
            headers=_headers(),
            json={"description": description},
            timeout=30,
        ),
        description=f"update description for activity {activity_id}",
    )
    _rate_limit_sleep(resp)
    return resp.json()  # type: ignore[no-any-return]


def is_authenticated() -> bool:
    return _load_tokens() is not None


def get_rate_limit_status() -> dict[str, Any]:
    """Check Strava API rate limit status with a lightweight request."""
    try:
        resp = _request_with_retry(
            lambda: httpx.get(
                f"{API_BASE}/athlete",
                headers=_headers(),
                timeout=15,
            ),
            description="rate limit status check",
        )
        rate_limit = _parse_rate_limit_headers(resp.headers)
        return {
            "ok": True,
            "status_code": resp.status_code,
            **rate_limit,
        }
    except Exception as e:
        return {
            "ok": False,
            "error": str(classify_strava_error(e)),
        }
