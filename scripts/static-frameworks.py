"""Resolve missing wrappers against the project's matching vendor XCFramework slices.

Invoked on the build worker after artifact pull. An ambiguous or unknown match
never authorizes stub injection. Fat archives are checked at their arm64 offset.
"""
import json
import os
import plistlib
import struct
import sys
from pathlib import Path


def arm64_archive(path):
    with path.open("rb") as f:
        magic = f.read(8)
        if magic == b"!<arch>\n":
            return True
        formats = {b"\xca\xfe\xba\xbe": (">", False), b"\xca\xfe\xba\xbf": (">", True),
                   b"\xbe\xba\xfe\xca": ("<", False), b"\xbf\xba\xfe\xca": ("<", True)}
        if magic[:4] not in formats:
            return False
        endian, wide = formats[magic[:4]]
        fmt = endian + ("IIQQII" if wide else "IIIII")
        count, = struct.unpack(endian+"I", magic[4:])
        if count > 128:
            return False
        for _ in range(count):
            cpu, _, offset, *_ = struct.unpack(fmt, f.read(struct.calcsize(fmt)))
            if cpu == 0x0100000c:
                f.seek(offset)
                return f.read(8) == b"!<arch>\n"
        return False


def resolve(root, names, simulator):
    found = {n: [] for n in names}
    for base, dirs, _ in os.walk(root):
        for dirname in dirs[:]:
            if not dirname.endswith(".xcframework"):
                continue
            dirs.remove(dirname)
            source = Path(base, dirname)
            with (source/"Info.plist").open("rb") as f:
                metadata = plistlib.load(f)
            for lib in metadata["AvailableLibraries"]:
                if (lib["SupportedPlatform"] != "ios" or
                        lib.get("SupportedPlatformVariant", "") != ("simulator" if simulator else "") or
                        "arm64" not in lib["SupportedArchitectures"]):
                    continue
                fw = source/lib["LibraryIdentifier"]/lib["LibraryPath"]
                if fw.name not in found:
                    continue
                with (fw/"Info.plist").open("rb") as f:
                    info = plistlib.load(f)
                if info.get("CFBundleIdentifier") != names[fw.name]:
                    continue
                found[fw.name].append(arm64_archive(fw/info["CFBundleExecutable"]))
        dirs[:] = [d for d in dirs if d not in {".git","DerivedData",".boop"} and not d.endswith(".app")]
    return [n for n, types in found.items() if types and all(types)]


if __name__ == "__main__":
    print(json.dumps(resolve(".", json.loads(sys.argv[1]), sys.argv[2] == "simulator")))

