#!/usr/bin/env python3
"""Actual Linux Podman smoke test; all data and credentials are synthetic.

Run from the checked-out project:
    python3 scripts/container-smoke.py --image localhost/qkbot-r:latest

Only the randomly named test container and volume are removed. The JSON report,
synthetic logs, and stopped-volume backup remain under data/qkbot-smoke-*.
No live QQ, model, production configuration, or administrator account is used.
"""
from __future__ import annotations

import argparse
import hashlib
import http.cookiejar
import http.server
import json
import os
from pathlib import Path
import platform
import socket
import sqlite3
import subprocess
import tarfile
import threading
import time
import tomllib
import urllib.error
import urllib.request
import uuid


class ProtocolStub(http.server.BaseHTTPRequestHandler):
    calls: list[str] = []

    def do_POST(self) -> None:
        self.calls.append(self.path)
        body = b'{"error":"synthetic smoke protocol is deliberately offline"}'
        self.send_response(503)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)
        self.close_connection = True

    def log_message(self, *_: object) -> None:
        pass


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", default="localhost/qkbot-r:latest")
    parser.add_argument("--output-dir", type=Path)
    args = parser.parse_args()
    if platform.system() != "Linux":
        parser.error("run this script on the Linux Podman host")
    root = Path(__file__).resolve().parents[1]
    identity = uuid.uuid4().hex
    name = f"qkbot-smoke-{identity[:16]}"
    volume = f"{name}-data"
    destination = (args.output_dir or root / "data" / name).resolve()
    destination.mkdir(mode=0o700, parents=True, exist_ok=False)
    environment = {
        key: value for key, value in os.environ.items()
        if not key.startswith("QKBOT_") and key not in {"CONTAINER_HOST", "CONTAINER_CONNECTION"}
    }
    environment.update({
        "QKBOT_IMAGE": args.image, "QKBOT_CONTAINER": name,
        "QKBOT_VOLUME": volume, "QKBOT_NETWORK": "host", "QKBOT_STOP_TIMEOUT": "30",
    })
    report: dict = {"image": args.image, "container": name, "volume": volume, "checks": []}
    exit_code = 1
    volume_created = False
    name_owned = False
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), ProtocolStub)
    server.daemon_threads = True
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    reservations = []

    def run(*command: str, data: bytes | None = None, check: bool = True,
            timeout: int = 60) -> subprocess.CompletedProcess:
        result = subprocess.run(
            command, input=data, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            env=environment, cwd=root, timeout=timeout, check=False,
        )
        if check and result.returncode:
            raise RuntimeError(f"{command[0]} {command[1:3]} failed: "
                               f"{result.stderr.decode(errors='replace')[-2000:]}")
        return result

    def podman(*command: str, **kwargs) -> subprocess.CompletedProcess:
        return run("podman", *command, **kwargs)

    def deploy(action: str, *extra: str, **kwargs) -> subprocess.CompletedProcess:
        return run("sh", str(root / "deploy/podman.sh"), action, *extra, **kwargs)

    def utility(*command: str, data: bytes | None = None) -> bytes:
        return podman(
            "run", "--rm", "--name", name, "--network", "none", "--pull", "never",
            "--read-only", "--cap-drop", "all", "--security-opt", "no-new-privileges",
            "-i", "--volume", f"{volume}:/data", args.image, *command, data=data,
        ).stdout

    def checked(label: str) -> None:
        report["checks"].append(label)
        print(f"PASS {label}", flush=True)

    def reserve_port() -> int:
        listener = socket.socket()
        listener.bind(("127.0.0.1", 0))
        reservations.append(listener)
        return listener.getsockname()[1]

    def new_client() -> urllib.request.OpenerDirector:
        return urllib.request.build_opener(
            urllib.request.ProxyHandler({}),
            urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()),
        )

    def api(client, endpoint: str, data: dict | None = None) -> dict:
        request = urllib.request.Request(
            f"http://127.0.0.1:{web_port}{endpoint}",
            data=None if data is None else json.dumps(data).encode(),
            headers={"Content-Type": "application/json"},
        )
        with client.open(request, timeout=3) as response:
            return json.load(response)

    def wait_api(client, endpoint: str, predicate) -> dict:
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            try:
                value = api(client, endpoint)
                if predicate(value):
                    return value
            except (OSError, urllib.error.URLError):
                pass
            time.sleep(0.2)
        raise RuntimeError(f"timed out waiting for isolated admin endpoint {endpoint}")

    def stop() -> None:
        started = time.monotonic()
        deploy("stop", timeout=45)
        state = json.loads(podman("inspect", name).stdout)[0]["State"]
        assert not state["Running"] and state["ExitCode"] == 0, state
        report.setdefault("shutdown_seconds", []).append(round(time.monotonic() - started, 3))

    def container_hash(path: str) -> str:
        return podman("exec", name, "sha256sum", path).stdout.decode().split()[0]

    try:
        assert podman("container", "exists", name, check=False).returncode == 1
        assert podman("volume", "exists", volume, check=False).returncode == 1
        report["image_id"] = podman("image", "inspect", "--format", "{{.Id}}", args.image).stdout.decode().strip()
        report["podman_version"] = podman("version", "--format", "{{.Client.Version}}").stdout.decode().strip()
        podman("volume", "create", "--label", f"io.github.fox-yun.smoke={identity}", volume)
        volume_created = name_owned = True
        utility("--init-config")
        original = utility("cat", "/data/config.toml")
        first = tomllib.loads(original.decode())
        assert first["web"]["listen"] == "127.0.0.1:3210"
        assert first["app"]["database"] == "qkbot.sqlite3"
        assert utility("id", "-u").strip() == b"10001"
        utility("sh", "-ceu", "test -r /usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc")
        checked("real entrypoint seeded config; volume writable as UID 10001; CJK font readable")

        meals = json.loads((root / "assets/native/meal/menu.json").read_text(encoding="utf-8"))
        hashes = {item["sha256"] for item in meals}
        assert len(meals) == len(hashes) == 151
        assert all(len(digest) == 64 and set(digest) <= set("0123456789abcdef") for digest in hashes)
        asset_dir = utility("printenv", "QKBOT_MEAL_IMAGES_DIR").decode().strip()
        assert asset_dir == "/usr/share/qkbot/meal-images"
        paths = sorted(f"{asset_dir}/{digest}" for digest in hashes)
        actual = utility("sha256sum", *paths).decode().splitlines()
        assert len(actual) == len(paths)
        assert all(line.split() == [Path(path).name, path] for line, path in zip(actual, paths))
        checked("all 151 bundled meal images pass SHA256 verification with network disabled")

        web_port, bridge_port = reserve_port(), reserve_port()
        stub_port = server.server_port
        setup_code = f"synthetic-setup-{identity}"
        password = f"synthetic-password-{identity}"
        config = f"""[app]
database = "qkbot.sqlite3"
log = "info"
[web]
enabled = true
listen = "127.0.0.1:{web_port}"
token = "{setup_code}"
[qq_login]
enabled = false
url = "http://127.0.0.1:{stub_port}"
[milky]
url = "http://127.0.0.1:{stub_port}"
request_timeout_secs = 2
[llm]
chat_completions_url = "http://127.0.0.1:{stub_port}/v1/chat/completions"
model = "synthetic-disabled-model"
[plugins.ai]
enabled = false
[plugins.clock]
enabled = false
[plugins.reminder]
enabled = false
[plugins.native]
enabled = []
[nonebot]
enabled = false
listen = "127.0.0.1:{bridge_port}"
media_dir = "data/plugin-media"
""".encode()
        utility("sh", "-ceu", "cat > /data/config.toml", data=config)
        utility("--check")
        utility("--init-config")
        assert utility("cat", "/data/config.toml") == config
        checked("synthetic config passes --check and initialization never overwrites it")
        for listener in reservations:
            listener.close()
        reservations.clear()
        deploy("start")
        client = new_client()
        wait_api(client, "/api/auth/state", lambda value: value["setup_required"])
        assert api(client, "/api/auth/setup", {"code": setup_code, "password": password})["authenticated"]
        wait_api(client, "/api/status", lambda value: value["phase"] == "waiting_login")
        assert podman("exec", name, "id", "-u").stdout.strip() == b"10001"
        assert container_hash("/data/config.toml") == hashlib.sha256(config).hexdigest()
        credential_hash = container_hash("/data/.qkbot-admin/credentials.json")
        assert podman("exec", name, "stat", "-c", "%u:%g:%a", "/data/.qkbot-admin/credentials.json").stdout.strip() == b"10001:10001:600"
        podman("exec", name, "sh", "-ceu",
               'printf "\\n# smoke restart marker\\n" >> /data/config.toml; printf "%s" "$1" > /data/smoke-marker',
               "smoke", identity)
        changed_hash = container_hash("/data/config.toml")
        live_backup = destination / "must-not-exist.tar"
        rejected = deploy("backup", str(live_backup), check=False)
        assert rejected.returncode != 0 and not live_backup.exists()
        assert b"Stop the bot before backup" in rejected.stderr
        checked("isolated administrator initialized; live SQLite volume export refused")

        stop()
        deploy("start")
        client = new_client()
        wait_api(client, "/api/auth/state", lambda value: not value["setup_required"])
        assert api(client, "/api/auth/login", {"password": password})["authenticated"]
        wait_api(client, "/api/status", lambda value: value["phase"] == "waiting_login")
        assert container_hash("/data/config.toml") == changed_hash
        assert container_hash("/data/.qkbot-admin/credentials.json") == credential_hash
        assert podman("exec", name, "cat", "/data/smoke-marker").stdout.decode() == identity
        checked("SIGTERM exits zero; restart preserves edited config, marker, and administrator")
        stop()
        archive = destination / "stopped-volume.tar"
        deploy("backup", str(archive), timeout=90)
        assert archive.is_file() and archive.stat().st_mode & 0o777 == 0o600
        with tarfile.open(archive) as exported:
            members = {member.name.removeprefix("./"): member for member in exported.getmembers()}
            for filename, digest in [
                ("config.toml", changed_hash),
                (".qkbot-admin/credentials.json", credential_hash),
            ]:
                source = exported.extractfile(members[filename])
                assert source is not None and hashlib.sha256(source.read()).hexdigest() == digest
            assert exported.extractfile(members["smoke-marker"]).read().decode() == identity
            # Extract only exact regular database files to fixed filenames; never
            # follow archive paths or links into the host filesystem.
            for filename in ("qkbot.sqlite3", "qkbot.sqlite3-wal", "qkbot.sqlite3-shm"):
                if filename in members:
                    member = members[filename]
                    assert member.isfile() and member.size < 64 * 1024 * 1024
                    (destination / filename).write_bytes(exported.extractfile(member).read())
        with sqlite3.connect((destination / "qkbot.sqlite3").as_uri() + "?mode=ro", uri=True) as database:
            assert database.execute("PRAGMA integrity_check").fetchall() == [("ok",)]
            assert database.execute("SELECT COUNT(*) FROM schema_migrations").fetchone()[0] > 0
            assert database.execute("SELECT COUNT(*) FROM outbox").fetchone()[0] == 0
        assert ProtocolStub.calls and set(ProtocolStub.calls) == {"/api/get_login_info"}
        checked("real stopped-volume export contains consistent SQLite and no outgoing messages")
        report["status"] = "passed"
        exit_code = 0
    except Exception as error:
        report["status"] = "failed"
        report["error"] = str(error)
        print(f"FAIL {error}", flush=True)
    finally:
        for listener in reservations:
            listener.close()
        server.shutdown()
        server.server_close()
        try:
            if name_owned and podman("container", "exists", name, check=False).returncode == 0:
                logs = podman("logs", name, check=False)
                (destination / "synthetic-container.log").write_bytes(logs.stdout + logs.stderr)
                podman("stop", "--time", "30", name, check=False, timeout=45)
                # This name was checked absent before creation and is unique
                # to this run. Force cleanup never names another container.
                podman("rm", "--force", name, timeout=45)
            if volume_created:
                podman("volume", "rm", volume)
                report["temporary_volume_removed"] = True
        except Exception as error:
            report["status"] = "failed"
            report["cleanup_error"] = str(error)
            exit_code = 1
        (destination / "report.json").write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n")
        print(f"Report: {destination / 'report.json'}", flush=True)
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
