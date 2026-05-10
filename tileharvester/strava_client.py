"""Strava API client with OAuth and token refresh."""
import json
import time
from pathlib import Path
from typing import Any, Optional

import httpx

from tileharvester.config import settings


TOKEN_URL = "https://www.strava.com/oauth/token"
API_BASE = "https://www.strava.com/api/v3"


def _token_file() -> Path:
    return settings.token_path


def _save_tokens(data: dict) -> None:
    settings.ensure_dirs()
    _token_file().write_text(json.dumps(data, indent=2))


def _load_tokens() -> Optional[dict]:
    if not _token_file().exists():
        return None
    return json.loads(_token_file().read_text())


def build_auth_url() -> str:
    scopes = "activity:read_all,activity:write"
    return (
        f"https://www.strava.com/oauth/authorize"
        f"?client_id={settings.strava_client_id}"
        f"&redirect_uri={settings.strava_redirect_uri}"
        f"&response_type=code"
        f"&scope={scopes}"
    )


def exchange_code(code: str) -> dict:
    resp = httpx.post(
        TOKEN_URL,
        data={
            "client_id": settings.strava_client_id,
            "client_secret": settings.strava_client_secret,
            "code": code,
            "grant_type": "authorization_code",
        },
    )
    resp.raise_for_status()
    data = resp.json()
    data["expires_at"] = int(time.time()) + data.get("expires_in", 21600)
    _save_tokens(data)
    return data


def _refresh_if_needed() -> dict:
    tokens = _load_tokens()
    if tokens is None:
        raise RuntimeError("Not authenticated. Run 'tileharvester auth' first.")

    if tokens.get("expires_at", 0) < time.time() + 300:
        resp = httpx.post(
            TOKEN_URL,
            data={
                "client_id": settings.strava_client_id,
                "client_secret": settings.strava_client_secret,
                "refresh_token": tokens["refresh_token"],
                "grant_type": "refresh_token",
            },
        )
        resp.raise_for_status()
        data = resp.json()
        data["expires_at"] = int(time.time()) + data.get("expires_in", 21600)
        _save_tokens(data)
        tokens = data

    return tokens


def _headers() -> dict:
    tokens = _refresh_if_needed()
    return {"Authorization": f"Bearer {tokens['access_token']}"}


def _rate_limit_sleep(response: httpx.Response) -> None:
    """Respect Strava rate limits."""
    limit = response.headers.get("X-RateLimit-Limit", "")
    usage = response.headers.get("X-RateLimit-Usage", "")
    if limit and usage:
        try:
            daily_limit, _ = map(int, limit.split(","))
            daily_used, _ = map(int, usage.split(","))
            if daily_used >= daily_limit - settings.rate_limit_buffer:
                print(f"Rate limit close: {daily_used}/{daily_limit}. Sleeping 15 minutes.")
                time.sleep(900)
        except ValueError:
            pass


def get_athlete() -> dict:
    resp = httpx.get(f"{API_BASE}/athlete", headers=_headers())
    resp.raise_for_status()
    _rate_limit_sleep(resp)
    return resp.json()


def get_activities(page: int = 1, per_page: int = 200, after: Optional[int] = None, before: Optional[int] = None) -> list[dict]:
    params = {"page": page, "per_page": per_page}
    if after is not None:
        params["after"] = after
    if before is not None:
        params["before"] = before
    resp = httpx.get(f"{API_BASE}/athlete/activities", headers=_headers(), params=params, timeout=30)
    resp.raise_for_status()
    _rate_limit_sleep(resp)
    return resp.json()


def get_activity(activity_id: int) -> dict:
    resp = httpx.get(f"{API_BASE}/activities/{activity_id}", headers=_headers(), timeout=30)
    resp.raise_for_status()
    _rate_limit_sleep(resp)
    return resp.json()


def get_activity_streams(activity_id: int, keys: str = "latlng") -> dict:
    resp = httpx.get(
        f"{API_BASE}/activities/{activity_id}/streams",
        headers=_headers(),
        params={"keys": keys, "key_by_type": "true"},
        timeout=60,
    )
    resp.raise_for_status()
    _rate_limit_sleep(resp)
    return resp.json()


def update_activity_description(activity_id: int, description: str) -> dict:
    resp = httpx.put(
        f"{API_BASE}/activities/{activity_id}",
        headers=_headers(),
        json={"description": description},
        timeout=30,
    )
    resp.raise_for_status()
    _rate_limit_sleep(resp)
    return resp.json()


def is_authenticated() -> bool:
    return _load_tokens() is not None
