"""The install has to keep describing the code it installs.

Packaging drift is silent in the worst way: a module left out of `py-modules`
imports perfectly from a clone and is simply absent once installed, so the
failure only ever reaches somebody who installed it properly.
"""

import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path

ROOT = Path(__file__).parent.parent
PYPROJECT = tomllib.loads((ROOT / "pyproject.toml").read_text())


class BuiltInstallationTests(unittest.TestCase):
    """Exercise the artifacts, not only the files visible from this checkout."""

    TIMEOUT = 120

    def test_wheel_and_sdist_install_and_ship_web_assets(self):
        uv = shutil.which("uv")
        build_module = importlib.util.find_spec("build")
        if not uv and not build_module:
            self.skipTest("neither uv nor the python build module is installed")

        with tempfile.TemporaryDirectory(prefix="lmloop-packaging-") as temporary:
            temporary = Path(temporary)
            clean_source = temporary / "source"
            # Build from a clean tracked snapshot, including staged changes under test.
            subprocess.run(
                ["git", "clone", "--shared", "--no-checkout", str(ROOT), str(clean_source)],
                check=True, capture_output=True, text=True, timeout=30,
            )
            subprocess.run(
                ["git", "checkout", "HEAD"],
                cwd=clean_source, check=True, capture_output=True, text=True, timeout=30,
            )
            staged = subprocess.run(
                ["git", "diff", "--cached", "--binary", "HEAD"], cwd=ROOT,
                check=True, capture_output=True, timeout=30,
            ).stdout
            if staged:
                subprocess.run(
                    ["git", "apply", "--index", "-"], cwd=clean_source,
                    input=staged, check=True, capture_output=True, timeout=30,
                )
            artifacts = temporary / "artifacts"
            build_command = ([uv, "build", "--wheel", "--sdist", "--out-dir", str(artifacts)]
                             if uv else
                             [sys.executable, "-m", "build", "--wheel", "--sdist",
                              "--outdir", str(artifacts)])
            result = subprocess.run(
                build_command, cwd=clean_source, capture_output=True, text=True,
                check=False, timeout=self.TIMEOUT,
            )
            self.assertEqual(0, result.returncode, result.stdout + result.stderr)
            self.assertEqual(1, len(list(artifacts.glob("*.whl"))))
            self.assertEqual(1, len(list(artifacts.glob("*.tar.gz"))))

            clean_environment = {**os.environ, "PYTHONNOUSERSITE": "1"}
            clean_environment.pop("PYTHONPATH", None)
            virtualenv = temporary / "venv"
            if uv:
                subprocess.run(
                    [uv, "venv", str(virtualenv), "--python", sys.executable],
                    check=True, capture_output=True, text=True, timeout=60,
                )
                python = virtualenv / "bin" / "python"
                subprocess.run(
                    [uv, "pip", "install", "--python", str(python), "--no-deps",
                     str(next(artifacts.glob("*.whl")))],
                    cwd=temporary, env=clean_environment,
                    check=True, capture_output=True, text=True, timeout=self.TIMEOUT,
                )
            else:
                subprocess.run(
                    [sys.executable, "-m", "venv", str(virtualenv)],
                    check=True, capture_output=True, text=True, timeout=60,
                )
                python = virtualenv / "bin" / "python"
                subprocess.run(
                    [str(python), "-m", "pip", "install", "--no-deps",
                     str(next(artifacts.glob("*.whl")))],
                    cwd=temporary, env=clean_environment,
                    check=True, capture_output=True, text=True, timeout=self.TIMEOUT,
                )

            for entry_point in ("lmloop", "lmloop-web"):
                completed = subprocess.run(
                    [str(virtualenv / "bin" / entry_point), "--help"],
                    cwd=temporary, env=clean_environment, capture_output=True,
                    text=True, check=False, timeout=30,
                )
                self.assertEqual(0, completed.returncode,
                                 completed.stdout + completed.stderr)

            expected_data = sorted(
                path.relative_to(ROOT / "web").as_posix()
                for directory in (ROOT / "web" / "static", ROOT / "web" / "deploy")
                for path in directory.rglob("*")
                if path.is_file()
            )
            probe = (
                "from importlib.resources import files; "
                "import json, os; "
                "root = files('web'); "
                "missing = [name for name in json.loads(os.environ['LMLOOP_DATA']) "
                "if not root.joinpath(name).is_file()]; "
                "assert not missing, 'missing installed web data: ' + repr(missing)"
            )
            data_check = subprocess.run(
                [str(python), "-c", probe], cwd=temporary, env={
                    **clean_environment, "LMLOOP_DATA": json.dumps(expected_data),
                }, check=False, capture_output=True, text=True, timeout=30,
            )
            self.assertEqual(0, data_check.returncode,
                             data_check.stdout + data_check.stderr)


class ModuleListingTests(unittest.TestCase):
    def listed(self):
        return set(PYPROJECT["tool"]["setuptools"]["py-modules"])

    def present(self):
        return {
            path.stem for path in ROOT.glob("*.py")
            if not path.name.startswith("test_") and path.name != "setup.py"
        }

    def test_every_module_in_the_repo_is_installed(self):
        missing = self.present() - self.listed()
        self.assertEqual(set(), missing,
                         f"modules that would vanish once installed: {sorted(missing)}")

    def test_nothing_is_listed_that_does_not_exist(self):
        extra = self.listed() - self.present()
        self.assertEqual(set(), extra, f"listed but absent: {sorted(extra)}")

    def test_tests_are_not_shipped(self):
        for name in self.listed():
            self.assertFalse(name.startswith("test_"), name)


class EntryPointTests(unittest.TestCase):
    def test_both_console_scripts_point_at_something_real(self):
        import lmloop
        for script, target in PYPROJECT["project"]["scripts"].items():
            module, _, attribute = target.partition(":")
            with self.subTest(script=script):
                self.assertEqual("lmloop", module)
                self.assertTrue(callable(getattr(lmloop, attribute, None)),
                                f"{target} is not callable")

    def test_the_runner_and_the_dashboard_both_have_one(self):
        self.assertIn("lmloop", PYPROJECT["project"]["scripts"])
        self.assertIn("lmloop-web", PYPROJECT["project"]["scripts"])


class DependencyTests(unittest.TestCase):
    def test_the_runner_needs_nothing(self):
        """Standard library only is what lets this be installed by cloning it."""
        self.assertEqual([], PYPROJECT["project"]["dependencies"])

    def test_the_dashboard_extra_covers_oidc_and_web_push(self):
        extras = " ".join(PYPROJECT["project"]["optional-dependencies"]["web"]).lower()
        self.assertIn("pyjwt", extras)
        self.assertIn("requests", extras)
        self.assertIn("pywebpush", extras)

    def test_the_python_floor_matches_what_the_code_needs(self):
        """`tomllib` is 3.11; claiming less would install and then fail on
        the first config read."""
        self.assertEqual(">=3.11", PYPROJECT["project"]["requires-python"])


class ServiceUnitTests(unittest.TestCase):
    UNIT = (ROOT / "web" / "deploy" / "lmloop-web.service").read_text()

    def test_it_names_no_particular_person_or_checkout(self):
        for pattern in (r"/home/\w", r"%h/git/", r"/usr/bin/python3"):
            with self.subTest(pattern=pattern):
                self.assertIsNone(re.search(pattern, self.UNIT),
                                  f"{pattern} is somebody's machine, not a default")

    def test_it_runs_the_installed_console_script(self):
        self.assertIn("ExecStart=%h/.local/bin/lmloop-web", self.UNIT)

    def test_it_still_pins_a_path_for_the_agent_and_git(self):
        """A manager-environment change would otherwise break run launching in
        a way that only surfaces hours later, inside a run."""
        self.assertIn("Environment=PATH=", self.UNIT)

    def test_dashboard_restart_does_not_kill_supervised_previews(self):
        self.assertIn("KillMode=process", self.UNIT)


class ExampleFileTests(unittest.TestCase):
    PERSONAL = re.compile(r"pierce|levangie|172\.20\.\d+|/home/\w")

    # Repo-wide, this has to be narrower: `/home/you` is a fine thing to write
    # in documentation, and an example file is the one place it is not. What is
    # never fine anywhere is a real name, a real host, or a real address on
    # somebody's network.
    IDENTIFYING = re.compile(r"pierce|levangie|\b172\.\d+\.\d+\.\d+\b")
    # This file names them in order to look for them.
    # The Android package namespace is dev.levangie.lmloop and exempt from this identity check.
    # LICENSE is the one intentional identity: its legal copyright notice.
    EXEMPT = {"tests/test_packaging.py", "LICENSE"}

    def test_nothing_in_the_repository_carries_a_real_identity(self):
        """The example files were guarded and nothing else was, which is the
        wrong half: this repository is public, and what gets published is every
        tracked file. Captured fixtures are the live risk -- one added recently
        was a verbatim copy of an operator's own agent settings."""
        listed = subprocess.run(
            ["git", "ls-files"], cwd=ROOT, capture_output=True, text=True, check=True
        ).stdout.split()
        self.assertGreater(len(listed), 40, "git ls-files returned almost nothing")
        offenders = {}
        for name in listed:
            if name in self.EXEMPT or name.startswith("android/"):
                continue
            try:
                text = (ROOT / name).read_text(errors="ignore")
            except OSError:
                continue
            if name == "README.md":
                # The documented Obtainium source is intentionally repository-specific.
                text = re.sub(r"https://git\.levangie\.dev/Pierce/lmloop(?=[`\s)]|$)", "", text)
            found = sorted(set(self.IDENTIFYING.findall(text)))
            if found:
                offenders[name] = found
        self.assertEqual({}, offenders, f"tracked files carrying an identity: {offenders}")

    def test_the_guard_can_actually_see_something(self):
        """A guard on the guard: an `ls-files` that returned nothing, or a
        pattern that matches nothing, would pass silently."""
        self.assertRegex("ran as pierce on 172.20.23.94", self.IDENTIFYING)
        self.assertNotRegex("/home/you/.config", self.IDENTIFYING)

    def test_no_shipped_example_carries_somebody_s_settings(self):
        import config
        examples = {
            "config.sample()": config.sample(),
            "model-budgets.example.json": (ROOT / "model-budgets.example.json").read_text(),
            "web.env.example": (ROOT / "web" / "deploy" / "web.env.example").read_text(),
        }
        for name, text in examples.items():
            with self.subTest(example=name):
                found = self.PERSONAL.findall(text)
                self.assertEqual([], found, f"{name} leaks {found}")


if __name__ == "__main__":
    unittest.main()
