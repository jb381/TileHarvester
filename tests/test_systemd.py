"""Tests for generated systemd units."""

import tileharvester.systemd as systemd_mod


def test_service_uses_active_python_and_environment_file(monkeypatch, tmp_path) -> None:
    python = tmp_path / "venv" / "bin" / "python"
    env_file = tmp_path / ".env"
    monkeypatch.setattr(systemd_mod.sys, "executable", str(python))

    service = systemd_mod.generate_service(data_dir="/data", env_file=str(env_file))

    assert f"ExecStart={python} -m tileharvester sync --once" in service
    assert f"EnvironmentFile=-{env_file}" in service
