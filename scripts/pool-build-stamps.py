"""Carry Swift Build's nanosecond file timestamps across Boat snapshots."""
import gzip
import hashlib
import json
import os
import sqlite3
import stat
import sys
import tempfile
from pathlib import Path


def capture(root, stamp_file):
    db = root / "xtool/.xtool-tmp/DerivedData/Intermediates.noindex/XCBuildData/build.db"
    if not db.is_file():
        stamp_file.unlink(missing_ok=True)
        return 0
    # Read only paths known to the build system, including SDK inputs.
    with sqlite3.connect(db.as_uri() + "?mode=ro", uri=True) as conn:
        paths = conn.execute("SELECT CAST(key AS BLOB) FROM key_names WHERE key LIKE 'N/%'")
        stamps = {}
        for (key,) in paths:
            path = Path(os.fsdecode(key[1:]))
            if not allowed(path, root):
                continue
            try:
                st = path.lstat()
            except (FileNotFoundError, NotADirectoryError):
                continue
            if st.st_uid != os.getuid():
                continue
            if stat.S_ISDIR(st.st_mode):
                stamps[str(path)] = (directory_signature(path), st.st_mtime_ns)
            elif st.st_mtime_ns % 1_000_000_000:
                stamps[str(path)] = (st.st_size, st.st_mtime_ns)
    stamp_file.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=stamp_file.parent)
    try:
        with os.fdopen(fd, "wb") as raw, gzip.GzipFile(fileobj=raw, mode="wb", compresslevel=1) as out:
            out.write(json.dumps(stamps).encode())
        os.replace(tmp, stamp_file)
    finally:
        Path(tmp).unlink(missing_ok=True)
    return len(stamps)


def allowed(path, root):
    return ".." not in path.parts and any(
        path.is_relative_to(base)
        for base in (root, Path.home() / ".config/xtool", Path.home() / ".local/share/swiftly")
    )


def directory_signature(path):
    # Extracting a directory can change both its inode size and modification time.
    names = sorted(os.listdir(os.fsencode(path)))
    return hashlib.sha256(b"\0".join(names)).hexdigest()


def restore(root, stamp_file):
    if not stamp_file.exists():
        return 0
    with gzip.open(stamp_file, "rt") as f:
        stamps = json.load(f)
    count = 0
    for name, (signature, original) in stamps.items():
        path = Path(name)
        if not allowed(path, root):
            raise ValueError(f"unexpected path in build timestamps: {path}")
        try:
            st = path.lstat()
        except (FileNotFoundError, NotADirectoryError):
            continue
        if st.st_uid != os.getuid() or st.st_mtime_ns == original:
            continue
        if isinstance(signature, str):
            inherited = stat.S_ISDIR(st.st_mode) and directory_signature(path) == signature
        else:
            rounded = original // 1_000_000_000 * 1_000_000_000
            inherited = st.st_mtime_ns == rounded and st.st_size == signature
        if inherited:
            os.utime(path, ns=(st.st_atime_ns, original), follow_symlinks=False)
            count += 1
    # Consume the inherited checkpoint before syncing this worktree's edits.
    # An interruption before this point can safely repeat the restoration.
    stamp_file.unlink()
    return count


if __name__ == "__main__":
    operation, workspace = sys.argv[1:]
    root = Path(workspace)
    stamp_file = Path.home() / "boop-cache/build-stamps" / (root.name + ".json.gz")
    print({"capture": capture, "restore": restore}[operation](root, stamp_file))
