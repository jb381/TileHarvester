"""Tests for strava_client.py."""

import time
from unittest.mock import MagicMock, patch

import httpx
import pytest

from tileharvester.strava_client import (
    StravaAuthError,
    StravaDataError,
    StravaNetworkError,
    StravaRateLimitError,
    StravaServerError,
    _headers,
    _load_tokens,
    _parse_rate_limit_headers,
    _rate_limit_sleep,
    _refresh_if_needed,
    _request_with_retry,
    _save_tokens,
    _token_file,
    build_auth_url,
    classify_strava_error,
    exchange_code,
    get_rate_limit_status,
    is_authenticated,
)


def _make_response(status_code=200, json_data=None, headers=None):
    """Build a real httpx.Response with httpx-compatible behavior."""
    if json_data is None:
        json_data = {}
    if headers is None:
        headers = {}
    request = httpx.Request("GET", "https://www.strava.com/api/v3/test")
    return httpx.Response(status_code, request=request, headers=headers, json=json_data)


class TestRequestWithRetry:
    def test_successful_request(self):
        response = _make_response(200, {"ok": True})
        result = _request_with_retry(lambda: response, description="test")
        assert result.status_code == 200
        assert result.json() == {"ok": True}

    def test_retries_on_5xx(self):
        fail_response = _make_response(500)
        ok_response = _make_response(200, {"ok": True})
        call_count = [0]

        def request_fn():
            call_count[0] += 1
            if call_count[0] < 3:
                return fail_response
            return ok_response

        with patch("time.sleep"):
            result = _request_with_retry(request_fn, description="test")
        assert result.status_code == 200
        assert call_count[0] == 3

    def test_no_retry_on_4xx(self):
        fail_response = _make_response(400)

        with pytest.raises(httpx.HTTPStatusError):
            _request_with_retry(lambda: fail_response, description="test")

    def test_exhausts_retries(self):
        fail_response = _make_response(503)

        with patch("time.sleep"), pytest.raises(httpx.HTTPStatusError):
            _request_with_retry(lambda: fail_response, description="test")


class TestParseRateLimitHeaders:
    def test_parses_valid_headers(self):
        headers = httpx.Headers(
            {
                "X-RateLimit-Limit": "200,2000",
                "X-RateLimit-Usage": "12,300",
            }
        )
        result = _parse_rate_limit_headers(headers)
        assert result["fifteen_min_limit"] == 200
        assert result["fifteen_min_used"] == 12
        assert result["daily_limit"] == 2000
        assert result["daily_used"] == 300

    def test_parses_single_value(self):
        headers = httpx.Headers(
            {
                "X-RateLimit-Limit": "200",
                "X-RateLimit-Usage": "12",
            }
        )
        result = _parse_rate_limit_headers(headers)
        assert result["daily_limit"] is None
        assert result["daily_used"] is None

    def test_parses_missing_headers(self):
        result = _parse_rate_limit_headers(httpx.Headers({}))
        assert result["daily_limit"] is None
        assert result["daily_used"] is None
        assert result["fifteen_min_limit"] is None
        assert result["fifteen_min_used"] is None

    def test_parses_invalid_numbers(self):
        headers = httpx.Headers(
            {
                "X-RateLimit-Limit": "abc,xyz",
                "X-RateLimit-Usage": "12,300",
            }
        )
        result = _parse_rate_limit_headers(headers)
        assert result["fifteen_min_limit"] is None
        assert result["fifteen_min_used"] == 12


class TestTokenStorage:
    def test_save_and_load_tokens(self, temp_settings, monkeypatch):
        import tileharvester.strava_client as strava_client_mod

        monkeypatch.setattr(strava_client_mod, "settings", temp_settings)

        data = {
            "access_token": "abc123",
            "refresh_token": "ref456",
            "expires_at": int(time.time()) + 21600,
        }
        _save_tokens(data)
        loaded = _load_tokens()
        assert loaded is not None
        assert loaded["access_token"] == "abc123"
        assert loaded["refresh_token"] == "ref456"

    def test_load_tokens_returns_none_when_file_missing(self, temp_settings, monkeypatch):
        import tileharvester.strava_client as strava_client_mod

        monkeypatch.setattr(strava_client_mod, "settings", temp_settings)
        # Ensure no token file exists
        if temp_settings.token_path.exists():
            temp_settings.token_path.unlink()
        assert _load_tokens() is None

    def test_token_file_path(self, temp_settings, monkeypatch):
        import tileharvester.strava_client as strava_client_mod

        monkeypatch.setattr(strava_client_mod, "settings", temp_settings)
        assert _token_file() == temp_settings.token_path


class TestBuildAuthUrl:
    def test_builds_url_with_correct_params(self, temp_settings, monkeypatch):
        import tileharvester.strava_client as strava_client_mod

        monkeypatch.setattr(strava_client_mod, "settings", temp_settings)

        url = build_auth_url()
        assert "strava.com/oauth/authorize" in url
        assert "client_id=test_client_id" in url
        assert "response_type=code" in url
        # URL-encoded: %3A is :, %2C is ,
        assert "activity%3Aread_all" in url
        assert "activity%3Awrite" in url

    def test_raises_when_credentials_missing(self, temp_settings, monkeypatch):
        import tileharvester.strava_client as strava_client_mod

        settings_bad = temp_settings.model_copy()
        settings_bad.strava_client_id = ""
        monkeypatch.setattr(strava_client_mod, "settings", settings_bad)

        with pytest.raises(RuntimeError, match="not configured"):
            build_auth_url()


class TestExchangeCode:
    def test_exchanges_code_successfully(self, temp_settings, mock_strava_api, monkeypatch):
        import tileharvester.config as config_mod

        monkeypatch.setattr(config_mod, "settings", temp_settings)

        mock_response = _make_response(
            200,
            {
                "access_token": "tok123",
                "refresh_token": "ref456",
                "expires_in": 21600,
                "athlete": {"id": 42},
            },
        )
        mock_strava_api["post"].return_value = mock_response

        result = exchange_code("test_code")
        assert result["access_token"] == "tok123"
        assert "expires_at" in result


class TestRefreshIfNeeded:
    def test_raises_when_not_authenticated(self, temp_settings, monkeypatch):
        import tileharvester.strava_client as strava_client_mod

        monkeypatch.setattr(strava_client_mod, "settings", temp_settings)
        # Ensure no token file
        if temp_settings.token_path.exists():
            temp_settings.token_path.unlink()

        with pytest.raises(RuntimeError, match="Not authenticated"):
            _refresh_if_needed()

    def test_returns_tokens_when_valid(self, temp_settings, monkeypatch):
        import tileharvester.strava_client as strava_client_mod

        monkeypatch.setattr(strava_client_mod, "settings", temp_settings)

        data = {
            "access_token": "abc",
            "refresh_token": "ref",
            "expires_at": int(time.time()) + 3600,
        }
        _save_tokens(data)
        tokens = _refresh_if_needed()
        assert tokens["access_token"] == "abc"

    def test_refreshes_expired_token(self, temp_settings, mock_strava_api, monkeypatch):
        import tileharvester.strava_client as strava_client_mod

        monkeypatch.setattr(strava_client_mod, "settings", temp_settings)

        data = {
            "access_token": "old",
            "refresh_token": "ref",
            "expires_at": int(time.time()) - 100,
        }
        _save_tokens(data)

        mock_response = _make_response(
            200,
            {
                "access_token": "new_tok",
                "refresh_token": "new_ref",
                "expires_in": 21600,
            },
        )
        mock_strava_api["post"].return_value = mock_response

        tokens = _refresh_if_needed()
        assert tokens["access_token"] == "new_tok"


class TestHeaders:
    def test_returns_bearer_headers(self, temp_settings, monkeypatch):
        import tileharvester.strava_client as strava_client_mod

        monkeypatch.setattr(strava_client_mod, "settings", temp_settings)

        data = {
            "access_token": "bearer_tok",
            "refresh_token": "ref",
            "expires_at": int(time.time()) + 3600,
        }
        _save_tokens(data)
        headers = _headers()
        assert headers["Authorization"] == "Bearer bearer_tok"


class TestRateLimitSleep:
    def test_no_sleep_when_under_limit(self, temp_settings):
        response = _make_response(
            200,
            headers={
                "X-RateLimit-Limit": "200,2000",
                "X-RateLimit-Usage": "10,100",
            },
        )
        # Should not raise or block
        _rate_limit_sleep(response)

    def test_sleeps_when_near_daily_limit(self, temp_settings, monkeypatch):
        import tileharvester.strava_client as strava_client_mod

        monkeypatch.setattr(strava_client_mod, "settings", temp_settings)

        response = _make_response(
            200,
            headers={
                "X-RateLimit-Limit": "200,2000",
                "X-RateLimit-Usage": "10,1995",
            },
        )
        with patch("time.sleep") as mock_sleep, patch("time.time", return_value=86400 - 899):
            _rate_limit_sleep(response)
            mock_sleep.assert_called_once_with(900.0)


class TestIsAuthenticated:
    def test_returns_false_when_no_token_file(self, temp_settings, monkeypatch):
        import tileharvester.strava_client as strava_client_mod

        monkeypatch.setattr(strava_client_mod, "settings", temp_settings)
        if temp_settings.token_path.exists():
            temp_settings.token_path.unlink()
        assert is_authenticated() is False

    def test_returns_true_when_token_file_exists(self, temp_settings, monkeypatch):
        import tileharvester.strava_client as strava_client_mod

        monkeypatch.setattr(strava_client_mod, "settings", temp_settings)
        _save_tokens({"access_token": "x"})
        assert is_authenticated() is True


class TestGetRateLimitStatus:
    def test_returns_ok_on_success(self, temp_settings, mock_strava_api, monkeypatch):
        import tileharvester.strava_client as strava_client_mod

        monkeypatch.setattr(strava_client_mod, "settings", temp_settings)

        data = {
            "access_token": "tok",
            "refresh_token": "ref",
            "expires_at": int(time.time()) + 3600,
        }
        _save_tokens(data)

        mock_response = _make_response(
            200,
            {"id": 42},
            headers={
                "X-RateLimit-Limit": "200,2000",
                "X-RateLimit-Usage": "50,500",
            },
        )
        mock_strava_api["get"].return_value = mock_response

        result = get_rate_limit_status()
        assert result["ok"] is True
        assert result["daily_limit"] == 2000
        assert result["daily_used"] == 500

    def test_returns_error_on_failure(self, temp_settings, mock_strava_api, monkeypatch):
        import tileharvester.strava_client as strava_client_mod

        monkeypatch.setattr(strava_client_mod, "settings", temp_settings)

        data = {
            "access_token": "tok",
            "refresh_token": "ref",
            "expires_at": int(time.time()) + 3600,
        }
        _save_tokens(data)

        mock_response = _make_response(401)
        mock_strava_api["get"].return_value = mock_response

        result = get_rate_limit_status()
        assert result["ok"] is False
        assert "error" in result


class TestClassifyStravaError:
    def test_classifies_401_as_auth_error(self):
        request = MagicMock()
        response = MagicMock()
        response.status_code = 401
        response.text = "Unauthorized"
        response.request = request
        exc = httpx.HTTPStatusError("401", request=request, response=response)
        result = classify_strava_error(exc)
        assert isinstance(result, StravaAuthError)

    def test_classifies_429_as_rate_limit_error(self):
        request = MagicMock()
        response = MagicMock()
        response.status_code = 429
        response.text = "Rate limited"
        response.request = request
        exc = httpx.HTTPStatusError("429", request=request, response=response)
        result = classify_strava_error(exc)
        assert isinstance(result, StravaRateLimitError)

    def test_classifies_500_as_server_error(self):
        request = MagicMock()
        response = MagicMock()
        response.status_code = 500
        response.text = "Server error"
        response.request = request
        exc = httpx.HTTPStatusError("500", request=request, response=response)
        result = classify_strava_error(exc)
        assert isinstance(result, StravaServerError)

    def test_classifies_400_as_data_error(self):
        request = MagicMock()
        response = MagicMock()
        response.status_code = 400
        response.text = "Bad request"
        response.request = request
        exc = httpx.HTTPStatusError("400", request=request, response=response)
        result = classify_strava_error(exc)
        assert isinstance(result, StravaDataError)

    def test_classifies_connect_error_as_network_error(self):
        exc = httpx.ConnectError("Connection refused")
        result = classify_strava_error(exc)
        assert isinstance(result, StravaNetworkError)

    def test_classifies_timeout_as_network_error(self):
        exc = httpx.ReadTimeout("Timeout")
        result = classify_strava_error(exc)
        assert isinstance(result, StravaNetworkError)

    def test_classifies_not_authenticated_runtime_error(self):
        exc = RuntimeError("Not authenticated. Run 'tileharvester auth' first.")
        result = classify_strava_error(exc)
        assert isinstance(result, StravaAuthError)

    def test_passes_through_existing_strava_errors(self):
        orig = StravaRateLimitError("custom message")
        result = classify_strava_error(orig)
        assert result is orig
