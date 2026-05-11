"""Systemd service and timer generation."""

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


def generate_service(data_dir: str | None = None, python: str | None = None) -> str:
    data_dir = data_dir or str(settings.data_dir)
    python = python or "/usr/bin/python3"
    return SERVICE_TEMPLATE.format(python=python, data_dir=data_dir)


def generate_timer(interval_minutes: int = 5) -> str:
    return TIMER_TEMPLATE.format(interval=interval_minutes)


def print_service(data_dir: str | None = None, python: str | None = None) -> None:
    print("=== tileharvester.service ===")
    print(generate_service(data_dir, python))
    print()
    print("=== tileharvester.timer ===")
    print(generate_timer(settings.poll_interval_minutes))
    print()
    print("Install with:")
    print("  sudo cp tileharvester.service tileharvester.timer /etc/systemd/system/")
    print("  sudo systemctl daemon-reload")
    print("  sudo systemctl enable --now tileharvester.timer")


def install_service(
    data_dir: str | None = None, python: str | None = None, interval: int = 5
) -> None:
    """Write systemd files to /etc/systemd/system/ (requires root)."""
    import subprocess

    service_path = Path("/etc/systemd/system/tileharvester.service")
    timer_path = Path("/etc/systemd/system/tileharvester.timer")

    service_path.write_text(generate_service(data_dir, python))
    timer_path.write_text(generate_timer(interval))

    subprocess.run(["systemctl", "daemon-reload"], check=True)
    subprocess.run(["systemctl", "enable", "--now", "tileharvester.timer"], check=True)
    print("Systemd timer installed and started.")
