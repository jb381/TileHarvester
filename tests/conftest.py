"""Shared test fixtures."""

from pathlib import Path

import pytest

from tileharvester.config import settings
from tileharvester.db import migrate


@pytest.fixture
def isolated_db(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Point the global settings object at a fresh migrated database."""
    monkeypatch.setattr(settings, "data_dir", tmp_path)
    monkeypatch.setattr(settings, "rewrite_existing_annotations", False)
    migrate()
    return tmp_path


@pytest.fixture(autouse=True)
def protect_user_data(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    import httpx

    monkeypatch.setattr(settings, "data_dir", tmp_path)

    def block_request(*_args, **_kwargs):
        raise AssertionError("Tests must mock HTTP requests")

    monkeypatch.setattr(httpx.Client, "send", block_request)
