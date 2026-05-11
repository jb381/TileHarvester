"""Tests for description line manipulation."""

from tileharvester.descriptions import (
    has_description_line,
    remove_description_line,
    update_description_line,
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
