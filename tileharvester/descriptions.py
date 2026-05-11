"""Description line manipulation for idempotent updates."""

import re

from tileharvester.config import settings


def _get_line_pattern() -> re.Pattern[str]:
    """Build the TileHarvester line regex from current settings (allows runtime changes)."""
    return re.compile(
        rf"^.*\b{re.escape(settings.description_prefix)}:\s.*$",
        re.MULTILINE,
    )


def update_description_line(description: str | None, new_line: str) -> str:
    """
    Replace existing TileHarvester line if present, otherwise append.
    Handles None/empty descriptions gracefully.
    """
    if not description:
        return new_line

    pattern = _get_line_pattern()
    if pattern.search(description):
        replaced = False
        lines = []
        for existing_line in description.splitlines():
            if pattern.match(existing_line):
                if not replaced:
                    lines.append(new_line)
                    replaced = True
                continue
            lines.append(existing_line)
        return "\n".join(lines)

    # No existing line - append with blank line before
    if description.endswith("\n"):
        return description + "\n" + new_line
    else:
        return description + "\n\n" + new_line


def remove_description_line(description: str | None) -> str:
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
