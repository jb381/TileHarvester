"""Configuration and settings management."""
import os
from pathlib import Path

from pydantic import Field
from pydantic_settings import BaseSettings, SettingsConfigDict


DEFAULT_DATA_DIR = Path.home() / ".local" / "share" / "tileharvester"


class Settings(BaseSettings):
    model_config = SettingsConfigDict(
        env_prefix="TH_",
        env_file=".env",
        extra="ignore",
    )

    data_dir: Path = Field(default=DEFAULT_DATA_DIR)

    strava_client_id: str = ""
    strava_client_secret: str = ""
    strava_redirect_uri: str = "http://localhost:8000/callback"

    # Squadrats uses Mapbox/XYZ tile cells: z14 for Squadrats, z17 for Squadratinhos.
    squadrat_zoom: int = 14
    squadratinho_zoom: int = 17

    # Sync behavior
    poll_interval_minutes: int = 5
    backfill_per_run: int = 100
    rate_limit_buffer: int = 10
    ignored_sport_types: str = "VirtualRide,VirtualRun"
    stream_max_segment_meters: float = 300.0
    stream_max_time_gap_seconds: int = 60
    stream_max_speed_mps: float = 35.0
    stream_gap_min_meters: float = 50.0

    @property
    def ignored_sports(self) -> set[str]:
        return {s.strip() for s in self.ignored_sport_types.split(",") if s.strip()}

    # Description
    description_prefix: str = "TileHarvester"
    description_emoji: str = "🗺️"
    squadrat_offset: int = Field(default=0, description="Offset to add to squadrat totals (for tile count adjustments)")
    rewrite_existing_annotations: bool = False

    @property
    def db_path(self) -> Path:
        return self.data_dir / "tileharvester.db"

    @property
    def token_path(self) -> Path:
        return self.data_dir / "strava_tokens.json"

    def ensure_dirs(self) -> None:
        self.data_dir.mkdir(parents=True, exist_ok=True)


settings = Settings()
