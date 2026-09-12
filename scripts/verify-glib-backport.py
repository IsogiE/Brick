"""Verify the vendored GLib backport against the checksum-pinned original crate."""
import hashlib
import io
from pathlib import Path
import tarfile
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
URL = "https://static.crates.io/crates/glib/glib-0.18.5.crate"
SHA256 = "233daaf6e83ae6a12a52055f568f9d7cf4671dabb78ff9560ab6da230ce00ee5"
with urllib.request.urlopen(URL, timeout=30) as response:
    data = response.read(4 * 1024 * 1024)
assert hashlib.sha256(data).hexdigest() == SHA256, "Upstream crate checksum mismatch"
original = {}
with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
    for member in archive:
        if member.isdir():
            continue
        assert member.isfile(), "Unexpected archive member"
        original[member.name] = archive.extractfile(member).read()
name = "glib-0.18.5/src/variant_iter.rs"
before = original[name]
old = b"let p: *mut libc::c_char = std::ptr::null_mut();"
assert before.count(old) == 1
after = before.replace(old, b"let mut p: *mut libc::c_char = std::ptr::null_mut();")
old = b"                &p,\n                std::ptr::null::<i8>(),"
assert after.count(old) == 1
original[name] = after.replace(old, old.replace(b"&p,", b"&mut p,"))
vendored = ROOT / "vendor"
actual = {}
for path in (vendored / "glib-0.18.5").rglob("*"):
    assert not path.is_symlink(), "Vendored source cannot contain links"
    if path.is_file():
        actual[path.relative_to(vendored).as_posix()] = path.read_bytes()
assert actual.keys() == original.keys(), "Vendored file list differs from upstream"
for name, expected in original.items():
    assert actual[name] == expected, f"Unexpected vendored modification: {name}"
print("GLib backport matches the authenticated upstream crate and exact two-line fix.")
