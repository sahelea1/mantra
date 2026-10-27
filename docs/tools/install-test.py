#!/usr/bin/env python3
"""Offline installer regression: verified paired assets, compact output, running updates."""
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

INSTALLER = Path(__file__).resolve().parents[2] / "install.sh"


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="mantra installer ")
        self.root = Path(self.tmp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.install = self.root / "installed"
        self.install.mkdir()
        self.assets = self.root / "assets"
        self.assets.mkdir()
        for name in ("mantra", "bija"):
            p = self.assets / (name + "-linux-x86_64")
            p.write_text("#!/bin/sh\nprintf '%s\\n' '" + name + " 9.8.7'\n")
            p.with_suffix(".sha256").write_text(hashlib.sha256(p.read_bytes()).hexdigest() + "  " + p.name + "\n")
        self.script("uname", '#!/bin/sh\n[ "$1" = -s ] && echo Linux || echo x86_64\n')
        self.script("curl", "#!" + sys.executable + "\nimport os,sys,shutil\nfrom pathlib import Path\na=sys.argv[1:]\nu=next(x for x in a if x.startswith('https://'))\np=Path(os.environ['FIXTURES'])/u.rsplit('/',1)[1]\nif not p.is_file():sys.exit(22)\nshutil.copyfile(p,a[a.index('-o')+1])\n")
        # If a checksum fails, source fallback must also fail without touching the install.
        self.script("git", "#!/bin/sh\nexit 1\n")
        self.script("cargo", "#!/bin/sh\nexit 1\n")
        self.env = dict(os.environ, PATH=str(self.bin) + ":" + str(self.install) + ":" + os.environ["PATH"],
                        FIXTURES=str(self.assets), MANTRA_INSTALL_DIR=str(self.install),
                        MANTRA_VERSION="v9.8.7", MANTRA_FROM_SOURCE="0", MANTRA_INSTALL_VERBOSE="0")

    def tearDown(self):
        self.tmp.cleanup()

    def script(self, name, content):
        p = self.bin / name
        p.write_text(content)
        p.chmod(0o755)

    def run_installer(self, update=False):
        return subprocess.run(["sh", str(INSTALLER)], env=dict(self.env, MANTRA_UPDATE="1" if update else "0"),
                              capture_output=True, text=True, timeout=15)

    def test_fresh_install_is_short_and_has_both_verified_binaries(self):
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertLessEqual(len(result.stdout.strip().splitlines()), 4)
        self.assertIn("mantra update", result.stdout)
        self.assertNotIn("doctor...", result.stdout)
        for name in ("mantra", "bija"):
            self.assertEqual(subprocess.check_output([str(self.install / name), "--version"], text=True).strip(), name + " 9.8.7")
        self.assertEqual(list(self.install.glob(".mantra-install.*")), [])

    def test_running_executable_can_be_replaced_atomically(self):
        shutil.copyfile(shutil.which("sleep"), self.install / "mantra")
        (self.install / "mantra").chmod(0o755)
        child = subprocess.Popen([str(self.install / "mantra"), "30"])
        try:
            result = self.run_installer(update=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIsNone(child.poll(), "an existing session should continue on its old executable")
            self.assertIn("Restart mantra", result.stdout)
        finally:
            child.terminate()
            child.wait(timeout=5)

    def test_bad_bija_checksum_leaves_both_old_binaries_intact(self):
        for name in ("mantra", "bija"):
            (self.install / name).write_text("old " + name)
        (self.assets / "bija-linux-x86_64").write_text("corrupted")
        result = self.run_installer(update=True)
        self.assertNotEqual(result.returncode, 0)
        for name in ("mantra", "bija"):
            self.assertEqual((self.install / name).read_text(), "old " + name)

    def test_bad_mantra_checksum_cannot_replace_installation(self):
        (self.install / "mantra").write_text("old mantra")
        (self.assets / "mantra-linux-x86_64").write_text("corrupted")
        result = self.run_installer(update=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((self.install / "mantra").read_text(), "old mantra")


if __name__ == "__main__":
    unittest.main()
