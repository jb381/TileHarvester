"""Tests for historical activity backfill."""

import tileharvester.backfill as backfill_mod


def test_backfill_honors_limit_exactly(isolated_db, monkeypatch) -> None:
    del isolated_db
    calls: list[tuple[int, int, int | None]] = []

    def fake_fetch(page: int, per_page: int, max_items: int | None):
        calls.append((page, per_page, max_items))
        fetched = min(per_page, max_items) if max_items is not None else per_page
        return {
            "fetched": fetched,
            "stored": fetched,
            "updated": 0,
            "skipped": 0,
            "ignored": 0,
        }

    monkeypatch.setattr(backfill_mod, "fetch_and_store_summaries", fake_fetch)

    result = backfill_mod.backfill(limit=10)

    assert calls == [(1, 200, 10)]
    assert result["stored"] == 10


def test_backfill_keeps_page_size_stable_across_partial_final_page(
    isolated_db, monkeypatch
) -> None:
    del isolated_db
    calls: list[tuple[int, int, int | None]] = []

    def fake_fetch(page: int, per_page: int, max_items: int | None):
        calls.append((page, per_page, max_items))
        fetched = min(per_page, max_items) if max_items is not None else per_page
        return {
            "fetched": fetched,
            "stored": fetched,
            "updated": 0,
            "skipped": 0,
            "ignored": 0,
        }

    monkeypatch.setattr(backfill_mod, "fetch_and_store_summaries", fake_fetch)

    result = backfill_mod.backfill(limit=250)

    assert calls == [(1, 200, 250), (2, 200, 50)]
    assert result["stored"] == 250
