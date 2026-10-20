from __future__ import annotations

import importlib.util
import tempfile
import unittest
from pathlib import Path


ENTRYPOINT = Path(__file__).resolve().parents[1] / "container-entrypoint.py"
spec = importlib.util.spec_from_file_location("container_entrypoint", ENTRYPOINT)
entrypoint = importlib.util.module_from_spec(spec)
spec.loader.exec_module(entrypoint)


class ContainerEntrypointTests(unittest.TestCase):
    def test_token_environment_and_secret_file(self) -> None:
        token = "container-test-" + "x" * 32
        self.assertEqual(entrypoint.bridge_token({"QKBOT_NONEBOT_TOKEN": token}), token)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "bridge-secret"
            path.write_text(token + "\n", encoding="ascii")
            self.assertEqual(entrypoint.bridge_token({"QKBOT_NONEBOT_TOKEN_FILE": str(path)}), token)
            with self.assertRaisesRegex(ValueError, "either"):
                entrypoint.bridge_token({
                    "QKBOT_NONEBOT_TOKEN": token,
                    "QKBOT_NONEBOT_TOKEN_FILE": str(path),
                })

    def test_bad_or_oversize_secret_is_not_echoed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "private-file"
            for value in ("sensitive-but-too-short", "secret value " * 512, "\u4e2d" * 32, "x" * 32 + "\n\n"):
                path.write_text(value, encoding="utf-8")
                with self.assertRaises(ValueError) as failure:
                    entrypoint.bridge_token({"QKBOT_NONEBOT_TOKEN_FILE": str(path)})
                self.assertNotIn(value, str(failure.exception))
                self.assertNotIn(str(path), str(failure.exception))
            path.unlink()
            with self.assertRaisesRegex(ValueError, "cannot read"):
                entrypoint.bridge_token({"QKBOT_NONEBOT_TOKEN_FILE": str(path)})

    def test_profile_mapping_matches_existing_files(self) -> None:
        root = ENTRYPOINT.parent
        for installed, profiles in entrypoint.PROFILES.items():
            args = entrypoint.command({"QKBOT_NONEBOT_IMAGE_PROFILE": installed}, [], root / "run.py")
            for profile in profiles:
                self.assertIn(profile, args)
                self.assertTrue((root / "profiles" / f"{profile}.toml").is_file())
            lock = "requirements.lock" if installed == "core" else f"requirements-{installed}.lock"
            self.assertTrue((root / lock).is_file())
        core = entrypoint.command({}, [], root / "run.py")
        self.assertNotIn("--profile", core)

    def test_full_can_select_subset_but_core_cannot_expand(self) -> None:
        args = entrypoint.command({
            "QKBOT_NONEBOT_IMAGE_PROFILE": "full",
            "QKBOT_NONEBOT_PROFILE": "parser",
            "QKBOT_NONEBOT_PLUGIN_CONFIG": "/run/secrets/plugin-config",
            "QKBOT_NONEBOT_TASK_TIMEOUT": "300",
        }, [], Path("/app/run.py"))
        self.assertIn("parser-bilibili", args)
        self.assertIn("/run/secrets/plugin-config", args)
        self.assertIn("300", args)
        with self.assertRaisesRegex(ValueError, "not provided"):
            entrypoint.command({"QKBOT_NONEBOT_PROFILE": "full"}, [], Path("/app/run.py"))
        with self.assertRaisesRegex(ValueError, "not provided"):
            entrypoint.command({"QKBOT_NONEBOT_IMAGE_PROFILE": "../full"}, [], Path("/app/run.py"))

    def test_preferences_survive_container_recreation(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            entrypoint.prepare_preferences(path)
            preferences = path / "witff-prefs.json"
            self.assertEqual(preferences.read_text(encoding="utf-8"), "{}\n")
            preferences.write_text('{"test-user": "text"}', encoding="utf-8")
            entrypoint.prepare_preferences(path)
            self.assertEqual(preferences.read_text(encoding="utf-8"), '{"test-user": "text"}')


if __name__ == "__main__":
    unittest.main()
