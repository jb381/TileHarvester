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
