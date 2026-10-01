import hashlib
import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("pacman", Path(__file__).with_name("check-pacman.py"))
pacman = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pacman)


class PacmanRecipeTests(unittest.TestCase):
    def setUp(self):
        self.config = {"version": "0.7.1", "pacman": {"depends": ["gtk3", "'webkit2gtk-4.1>=2.54.0'"]}}

    def test_quoted_version_constraint_is_one_dependency(self):
        self.assertEqual(pacman.dependencies(self.config), ["gtk3", "webkit2gtk-4.1>=2.54.0"])

    def test_original_unquoted_constraint_is_rejected(self):
        self.config["pacman"]["depends"][1] = "webkit2gtk-4.1>=2.54.0"
        with self.assertRaisesRegex(ValueError, "Bash syntax"):
            pacman.dependencies(self.config)

    def test_generated_archive_binding(self):
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            archive = root / "brick_0.7.1_x86_64.tar.gz"
            archive.write_bytes(b"package fixture")
            digest = hashlib.sha512(archive.read_bytes()).hexdigest()
            recipe = root / "PKGBUILD"
            recipe.write_text(f'pkgver=0.7.1\ndepends=(gtk3 \n\'webkit2gtk-4.1>=2.54.0\')\nsource=("{archive.name}")\nsha512sums=("{digest}")\n')
            pacman.check_recipe(self.config, recipe)
            archive.write_bytes(b"changed package")
            with self.assertRaisesRegex(ValueError, "checksum"):
                pacman.check_recipe(self.config, recipe)

    def test_shell_expansion_is_not_a_dependency(self):
        self.config["pacman"]["depends"] = ["$(echo gtk3)"]
        with self.assertRaisesRegex(ValueError, "token"):
            pacman.dependencies(self.config)


if __name__ == "__main__":
    unittest.main()
