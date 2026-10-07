"""Check every eframe vendor byte against the pinned crate and reviewed patches."""
import argparse
import hashlib
import io
from pathlib import Path
import re
import tarfile
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
CRATE_SHA256 = "f98fe83b2589105b69dd25ca1e0fa2135a6e864d502fd8e08978f937e128cfef"
PATCHES = (
    ("eframe-8461.patch", "27d8bb44be01756e9d794fdeeb34a366263f6987afd2d7c84334e0e22039bf27"),
    ("eframe-window-move.patch", "e9cc02e6c956a7beef4f01eac6d1327f766909f84a316d9c9cd0233e78744efd"),
)


def apply_patch(files, data):
    """Apply exact contextual hunks, allowing only the upstream line-number offset."""
    lines = data.decode("utf-8").splitlines(keepends=True)
    index = 0
    current_name = None
    cursor = 0
    while index < len(lines):
        line = lines[index]
        if line.startswith("+++ b/"):
            current_name = line[6:].strip()
            current_name = current_name.removeprefix("crates/eframe/")
            assert current_name in files, f"Unexpected patch file: {current_name}"
            cursor = 0
        elif line.startswith("@@ "):
            assert current_name is not None
            match = re.match(r"@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@", line)
            assert match, f"Malformed hunk: {line}"
            old_count = int(match[2] or "1")
            new_count = int(match[4] or "1")
            old, new = [], []
            index += 1
            while index < len(lines) and (len(old) < old_count or len(new) < new_count):
                hunk_line = lines[index]
                assert hunk_line[0] in " +-", f"Unexpected patch syntax: {hunk_line}"
                if hunk_line[0] in " -":
                    old.append(hunk_line[1:])
                if hunk_line[0] in " +":
                    new.append(hunk_line[1:])
                index += 1
            assert len(old) == old_count and len(new) == new_count
            contents = files[current_name].decode("utf-8").splitlines(keepends=True)
            matches = [at for at in range(cursor, len(contents) - len(old) + 1)
                       if contents[at:at + len(old)] == old]
            assert len(matches) == 1, f"Missing or ambiguous exact hunk in {current_name}"
            at = matches[0]
            contents[at:at + len(old)] = new
            cursor = at + len(new)
            files[current_name] = "".join(contents).encode("utf-8")
            continue
        index += 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--crate", type=Path, help="Optional already-downloaded original crate")
    args = parser.parse_args()
    if args.crate:
        data = args.crate.read_bytes()
    else:
        url = "https://static.crates.io/crates/eframe/eframe-0.34.3.crate"
        with urllib.request.urlopen(url, timeout=30) as response:
            data = response.read(2 * 1024 * 1024)
    assert hashlib.sha256(data).hexdigest() == CRATE_SHA256, "Original eframe checksum mismatch"
    expected = {}
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
        for member in archive:
            if member.isdir():
                continue
            assert member.isfile(), "Unexpected archive member"
            assert member.name.startswith("eframe-0.34.3/")
            name = member.name.removeprefix("eframe-0.34.3/")
            expected[name] = archive.extractfile(member).read()
    for name, digest in PATCHES:
        data = (ROOT / "security" / name).read_bytes()
        assert hashlib.sha256(data).hexdigest() == digest, f"Reviewed patch changed: {name}"
        apply_patch(expected, data)
    for name, digest in (
        ("eframe-LICENSE-APACHE", "8173d5c29b4f956d532781d2b86e4e30f83e6b7878dce18c919451d6ba707c90"),
        ("eframe-LICENSE-MIT", "95ca92f5f8ea5231f1580b3a2a799e8260af3114b900e1def5355a7f44bcf60c"),
    ):
        license_data = (ROOT / "security/licenses" / name).read_bytes()
        assert hashlib.sha256(license_data).hexdigest() == digest, f"Upstream license changed: {name}"
    actual = {}
    vendor = ROOT / "vendor/eframe-0.34.3"
    for path in vendor.rglob("*"):
        assert not path.is_symlink(), "Vendored source must not contain links"
        if path.is_file():
            actual[path.relative_to(vendor).as_posix()] = path.read_bytes()
    assert actual.keys() == expected.keys(), "Vendored file list differs from original crate"
    for name, contents in expected.items():
        assert actual[name] == contents, f"Unexpected eframe modification: {name}"
    print("eframe matches the pinned original crate, exact upstream #8461 fix, and opt-in Windows move policy.")


if __name__ == "__main__":
    main()
