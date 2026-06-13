"""Tests for description line manipulation."""

import pytest

from tileharvester.descriptions import (
    has_description_line,
    remove_description_line,
    update_description_line,
)


@pytest.fixture
def temp_settings():
    from tileharvester.config import Settings

    return Settings(
        db_path=":memory:",
        data_dir="/tmp",
        description_emoji="🗺️",
        description_prefix="TileHarvester",
        squadrat_offset=0,
        squadrat_zoom=14,
        squadratinho_zoom=17,
    )


def test_append_to_empty():
    result = update_description_line("", "🗺️ TileHarvester: +5 new Squadrats")
    assert result == "🗺️ TileHarvester: +5 new Squadrats"


def test_append_to_existing():
    result = update_description_line("Nice ride today!", "🗺️ TileHarvester: +5 new Squadrats")
    assert result == "Nice ride today!\n\n🗺️ TileHarvester: +5 new Squadrats"


def test_replace_existing():
    desc = "Nice ride!\n🗺️ TileHarvester: +3 new Squadrats · +100 this month"
    result = update_description_line(desc, "🗺️ TileHarvester: +5 new Squadrats · +200 this month")
    assert "+5 new Squadrats" in result
    assert "+3 new Squadrats" not in result


def test_replace_existing_with_different_emoji(temp_settings, monkeypatch):
    import tileharvester.config as config_mod
    import tileharvester.descriptions as descriptions_mod

    monkeypatch.setattr(config_mod, "settings", temp_settings)
    monkeypatch.setattr(descriptions_mod, "settings", temp_settings)

    desc = "Nice ride!\n🚴 TileHarvester: old stats"
    result = update_description_line(desc, "🗺️ TileHarvester: new stats")
    assert result == "Nice ride!\n🗺️ TileHarvester: new stats"


def test_collapses_duplicate_existing_lines():
    desc = "Nice ride!\n🗺️ TileHarvester: correct stats\n🗺️ TileHarvester: duplicate stats"
    result = update_description_line(desc, "🗺️ TileHarvester: latest stats")
    assert result == "Nice ride!\n🗺️ TileHarvester: latest stats"


def test_remove_line():
    desc = "Nice ride!\n🗺️ TileHarvester: +3 new Squadrats"
    result = remove_description_line(desc)
    assert "TileHarvester" not in result
    assert "Nice ride!" in result


def test_has_line():
    assert has_description_line("Some text\n🗺️ TileHarvester: +1") is True
    assert has_description_line("Some text") is False
    assert has_description_line("") is False
    assert has_description_line(None) is False
