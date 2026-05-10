"""Description line manipulation for idempotent updates."""
import re


LINE_PATTERN = re.compile(r"^🗺️ TileHarvester: .*$", re.MULTILINE)


def update_description_line(description: str, new_line: str) -> str:
    """
    Replace existing TileHarvester line if present, otherwise append.
    Handles None/empty descriptions gracefully.
    """
    if not description:
        return new_line

    if LINE_PATTERN.search(description):
        return LINE_PATTERN.sub(new_line, description)

    # No existing line - append with blank line before
    if description.endswith("\n"):
        return description + "\n" + new_line
    else:
        return description + "\n\n" + new_line


def remove_description_line(description: str) -> str:
    """Remove the TileHarvester line entirely."""
    if not description:
        return ""
    cleaned = LINE_PATTERN.sub("", description)
    # Remove trailing blank lines
    cleaned = cleaned.rstrip("\n")
    return cleaned


def has_description_line(description: str) -> bool:
    if not description:
        return False
    return bool(LINE_PATTERN.search(description))
