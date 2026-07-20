"""Tests for CLI date calculations."""

from datetime import datetime

from tileharvester.cli import _period_starts


def test_period_starts_crosses_month_boundary() -> None:
    week_start, month_start = _period_starts(datetime(2026, 8, 1, 12, 30))

    assert week_start == datetime(2026, 7, 27)
    assert month_start == datetime(2026, 8, 1)
