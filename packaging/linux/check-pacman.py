"""Check Pacman dependency syntax before builds and generated recipes before upload."""
import argparse
import hashlib
from pathlib import Path
import re
import shlex
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parents[2]


def check_syntax(recipe):
    # Parse only: never execute package functions or shell expansions.
    result = subprocess.run(["bash", "-n"], input=recipe.encode("utf-8"),
                            capture_output=True, timeout=10)
    if result.returncode:
        raise ValueError("Invalid PKGBUILD Bash syntax: " + result.stderr.decode("utf-8", errors="replace").strip())


def dependencies(config):
    tokens = config["pacman"]["depends"]
    if not tokens or not all(isinstance(token, str) for token in tokens):
        raise ValueError("Pacman dependencies are missing")
    # cargo-packager 0.11.8 inserts these strings verbatim into depends=(...).
    check_syntax("depends=(" + " \n".join(tokens) + ")\n")
    values = []
    for token in tokens:
        words = shlex.split(token)
        if len(words) != 1 or not re.fullmatch(r"[a-z0-9][a-z0-9+_.-]*(?:[<>=]{1,2}[a-zA-Z0-9.+:_-]+)?", words[0]):
            raise ValueError("Invalid Pacman dependency token")
        values.append(words[0])
    return values


def check_recipe(config, path):
    recipe = path.read_text(encoding="utf-8")
    check_syntax(recipe)
    expected = dependencies(config)
    found = re.findall(r"(?m)^depends=\(([^)]*)\)", recipe)
    if len(found) != 1 or shlex.split(found[0]) != expected:
        raise ValueError("Generated Pacman dependencies differ from configuration")
    if f"pkgver={config['version']}\n" not in recipe:
        raise ValueError("Generated Pacman version differs from configuration")
    archive = f"brick_{config['version']}_x86_64.tar.gz"
    if f'source=("{archive}")' not in recipe:
        raise ValueError("Unexpected Pacman source archive")
    with (path.parent / archive).open("rb") as source:
        digest = hashlib.file_digest(source, "sha512").hexdigest()
    if f'sha512sums=("{digest}")' not in recipe:
        raise ValueError("Pacman archive checksum does not match")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--recipe", type=Path)
    args = parser.parse_args()
    config = tomllib.loads((ROOT / "Packager.toml").read_text(encoding="utf-8"))
    dependencies(config)
    if args.recipe:
        check_recipe(config, args.recipe)
    print("Pacman dependency syntax" + (", generated recipe and archive checksum" if args.recipe else "") + " verified")


if __name__ == "__main__":
    main()
