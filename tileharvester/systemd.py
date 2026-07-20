"""Systemd service and timer generation."""

import sys
from pathlib import Path

from tileharvester.config import settings

SERVICE_TEMPLATE = """\
[Unit]
Description=TileHarvester Strava Sync
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
ExecStart={python} -m tileharvester sync --once
Environment=TH_DATA_DIR={data_dir}
EnvironmentFile=-{env_file}
"""

TIMER_TEMPLATE = """\
[Unit]
Description=TileHarvester Sync Timer

[Timer]
OnBootSec=2min
OnUnitActiveSec={interval}min
AccuracySec=1min
Persistent=true

[Install]
WantedBy=timers.target
"""


def generate_service(
    data_dir: str | None = None,
    python: str | None = None,
    env_file: str | None = None,
) -> str:
    data_dir = data_dir or str(settings.data_dir)
    python = python or sys.executable
    env_file = env_file or str(Path.cwd() / ".env")
    return SERVICE_TEMPLATE.format(python=python, data_dir=data_dir, env_file=env_file)


def generate_timer(interval_minutes: int = 5) -> str:
    return TIMER_TEMPLATE.format(interval=interval_minutes)


def print_service(
    data_dir: str | None = None,
    python: str | None = None,
    interval: int = 5,
    env_file: str | None = None,
) -> None:
    print("=== tileharvester.service ===")
    print(generate_service(data_dir, python, env_file))
    print()
    print("=== tileharvester.timer ===")
    print(generate_timer(interval))
    print()
    print("Install with:")
    print("  sudo cp tileharvester.service tileharvester.timer /etc/systemd/system/")
    print("  sudo systemctl daemon-reload")
    print("  sudo systemctl enable --now tileharvester.timer")


def install_service(
    data_dir: str | None = None,
    python: str | None = None,
    interval: int = 5,
    env_file: str | None = None,
) -> None:
    """Write systemd files to /etc/systemd/system/ (requires root)."""
    import subprocess

    service_path = Path("/etc/systemd/system/tileharvester.service")
    timer_path = Path("/etc/systemd/system/tileharvester.timer")

    service_path.write_text(generate_service(data_dir, python, env_file))
    timer_path.write_text(generate_timer(interval))

    subprocess.run(["systemctl", "daemon-reload"], check=True)
    subprocess.run(["systemctl", "enable", "--now", "tileharvester.timer"], check=True)
    print("Systemd timer installed and started.")
