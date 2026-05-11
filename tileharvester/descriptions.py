"""Description line manipulation for idempotent updates."""

import re

from tileharvester.config import settings


def _get_line_pattern() -> re.Pattern[str]:
    """Build the TileHarvester line regex from current settings (allows runtime changes)."""
    return re.compile(
        rf"^{re.escape(settings.description_emoji)}\s+{re.escape(settings.description_prefix)}: .*$",
        re.MULTILINE,
    )


def update_description_line(description: str, new_line: str) -> str:
    """
    Replace existing TileHarvester line if present, otherwise append.
    Handles None/empty descriptions gracefully.
    """
    if not description:
        return new_line

    pattern = _get_line_pattern()
    if pattern.search(description):
        return pattern.sub(new_line, description)

    # No existing line - append with blank line before
    if description.endswith("\n"):
        return description + "\n" + new_line
    else:
        return description + "\n\n" + new_line


def remove_description_line(description: str) -> str:
    """Remove the TileHarvester line entirely."""
    if not description:
        return ""
    pattern = _get_line_pattern()
    cleaned = pattern.sub("", description)
    # Remove trailing blank lines
    cleaned = cleaned.rstrip("\n")
    return cleaned


def has_description_line(description: str) -> bool:
    if not description:
        return False
    return bool(_get_line_pattern().search(description))
