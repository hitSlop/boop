#!/usr/bin/env python3
"""Remote-only Xcode project patcher for Boop.

Safely disables remote code signing (and optionally enables Swift compilation caching)
across both classic OpenStep `project.pbxproj` files and JSON `project.pbxproj` /
Xcode 27.2 `project.xcproj` files, while preserving project file mtimes across
unchanged SHA-256 outputs so Swift Build keeps its incremental cache warm.
"""

import argparse
import glob
import hashlib
import json
import os
import re
from pathlib import Path
from typing import Any, List, Optional, Tuple

CODESIGN_BASE_KEYS = (
    "AD_HOC_CODE_SIGNING_ALLOWED",
    "CODE_SIGN_IDENTITY",
    "CODE_SIGN_STYLE",
    "CODE_SIGNING_ALLOWED",
    "CODE_SIGNING_REQUIRED",
    "PROVISIONING_PROFILE",
    "PROVISIONING_PROFILE_SPECIFIER",
)

CAS_BASE_KEYS = (
    "COMPILATION_CACHE_CAS_PATH",
    "COMPILATION_CACHE_ENABLE_CACHING",
    "SWIFT_ENABLE_EXPLICIT_MODULES",
)

CODESIGN_LINE_RE = re.compile(
    r'^\s*"?(?:'
    + "|".join(CODESIGN_BASE_KEYS)
    + r')(?:\[[^\]]*\])?"?\s*=\s*[^;]*;\s*$'
)

CAS_LINE_RE = re.compile(
    r'^\s*"?(?:'
    + "|".join(CAS_BASE_KEYS)
    + r')(?:\[[^\]]*\])?"?\s*=\s*[^;]*;\s*$'
)

BUILD_SETTINGS_HEADER_RE = re.compile(r"buildSettings\s*=\s*\{")


def is_managed_json_key(key: str, base_keys: Tuple[str, ...]) -> bool:
    base = key.split("[", 1)[0]
    return base in base_keys


def patch_json_build_settings_walk(
    node: Any, disable_codesign: bool, cas_dir: Optional[str]
) -> bool:
    changed = False
    if isinstance(node, dict):
        bs = node.get("buildSettings")
        if isinstance(bs, dict):
            if disable_codesign:
                for k in list(bs.keys()):
                    if is_managed_json_key(k, CODESIGN_BASE_KEYS):
                        del bs[k]
                        changed = True
                desired = {
                    "AD_HOC_CODE_SIGNING_ALLOWED": "NO",
                    "CODE_SIGNING_ALLOWED": "NO",
                    "CODE_SIGNING_REQUIRED": "NO",
                    "CODE_SIGN_IDENTITY": "",
                    "CODE_SIGN_STYLE": "Manual",
                }
                for k, v in desired.items():
                    if bs.get(k) != v:
                        bs[k] = v
                        changed = True
            if cas_dir:
                desired_cas = {
                    "COMPILATION_CACHE_CAS_PATH": cas_dir,
                    "COMPILATION_CACHE_ENABLE_CACHING": "YES",
                    "SWIFT_ENABLE_EXPLICIT_MODULES": "YES",
                }
                for k, v in desired_cas.items():
                    if bs.get(k) != v:
                        bs[k] = v
                        changed = True
        for v in node.values():
            if patch_json_build_settings_walk(v, disable_codesign, cas_dir):
                changed = True
    elif isinstance(node, list):
        for item in node:
            if patch_json_build_settings_walk(item, disable_codesign, cas_dir):
                changed = True
    return changed


def patch_json_text(
    text: str, disable_codesign: bool, cas_dir: Optional[str]
) -> str:
    data = json.loads(text)
    if not patch_json_build_settings_walk(data, disable_codesign, cas_dir):
        return text
    return json.dumps(data, indent=2, sort_keys=True) + "\n"


def escape_openstep_string(value: str) -> str:
    escaped = value.replace("\\", "\\\\").replace('"', '\\"')
    return f'"{escaped}"'


def count_braces_outside_strings(line: str) -> int:
    delta = 0
    i = 0
    n = len(line)
    in_str = False
    while i < n:
        ch = line[i]
        if in_str:
            if ch == "\\" and i + 1 < n:
                i += 2
                continue
            if ch == '"':
                in_str = False
            i += 1
            continue
        if ch == '"':
            in_str = True
            i += 1
            continue
        if ch == "/" and i + 1 < n and line[i + 1] == "/":
            break
        if ch == "/" and i + 1 < n and line[i + 1] == "*":
            end = line.find("*/", i + 2)
            if end == -1:
                break
            i = end + 2
            continue
        if ch in "({":
            delta += 1
        elif ch in ")}":
            delta -= 1
        i += 1
    return delta


def patch_openstep_body(
    body: str, disable_codesign: bool, cas_dir: Optional[str]
) -> str:
    lines = body.splitlines(keepends=True)

    # Detect indentation from the first non-empty entry line inside buildSettings.
    indent = "\t\t\t\t"
    closing_indent = "\t\t\t"
    for line in lines:
        stripped = line.strip()
        if stripped:
            ws_len = len(line) - len(line.lstrip(" \t"))
            indent = line[:ws_len]
            break
    if lines and lines[-1].strip() == "":
        closing_indent = lines[-1]

    kept: List[str] = []
    nested_depth = 0
    for line in lines:
        if nested_depth == 0:
            if disable_codesign and CODESIGN_LINE_RE.match(line):
                continue
            if cas_dir and CAS_LINE_RE.match(line):
                continue
        kept.append(line)
        nested_depth = max(0, nested_depth + count_braces_outside_strings(line))

    injected: List[str] = []
    if disable_codesign:
        injected.extend(
            [
                f"{indent}AD_HOC_CODE_SIGNING_ALLOWED = NO;\n",
                f"{indent}CODE_SIGNING_ALLOWED = NO;\n",
                f"{indent}CODE_SIGNING_REQUIRED = NO;\n",
                f'{indent}CODE_SIGN_IDENTITY = "";\n',
                f"{indent}CODE_SIGN_STYLE = Manual;\n",
            ]
        )
    if cas_dir:
        injected.extend(
            [
                f"{indent}COMPILATION_CACHE_CAS_PATH = {escape_openstep_string(cas_dir)};\n",
                f"{indent}COMPILATION_CACHE_ENABLE_CACHING = YES;\n",
                f"{indent}SWIFT_ENABLE_EXPLICIT_MODULES = YES;\n",
            ]
        )

    if not injected:
        return body

    # Preserve leading newline after '{' if present; otherwise normalize an empty `{}` block.
    if kept and kept[0].strip() == "" and kept[0].endswith("\n"):
        return kept[0] + "".join(injected) + "".join(kept[1:])
    if not any(line.strip() for line in kept):
        return "\n" + "".join(injected) + closing_indent
    return "\n" + "".join(injected) + "".join(kept)


def find_matching_brace(text: str, open_idx: int) -> int:
    depth = 1
    i = open_idx + 1
    n = len(text)
    while i < n:
        ch = text[i]
        if ch == '"':
            i += 1
            while i < n:
                if text[i] == "\\" and i + 1 < n:
                    i += 2
                    continue
                if text[i] == '"':
                    i += 1
                    break
                i += 1
            continue
        if ch == "/" and i + 1 < n and text[i + 1] == "*":
            end = text.find("*/", i + 2)
            i = n if end == -1 else end + 2
            continue
        if ch == "/" and i + 1 < n and text[i + 1] == "/":
            nl = text.find("\n", i + 2)
            i = n if nl == -1 else nl + 1
            continue
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return -1


def patch_openstep_text(
    text: str, disable_codesign: bool, cas_dir: Optional[str]
) -> str:
    if not disable_codesign and not cas_dir:
        return text

    out: List[str] = []
    i = 0
    n = len(text)
    last_emit = 0

    while i < n:
        ch = text[i]
        if ch == '"':
            i += 1
            while i < n:
                if text[i] == "\\" and i + 1 < n:
                    i += 2
                    continue
                if text[i] == '"':
                    i += 1
                    break
                i += 1
            continue
        if ch == "/" and i + 1 < n and text[i + 1] == "*":
            end = text.find("*/", i + 2)
            i = n if end == -1 else end + 2
            continue
        if ch == "/" and i + 1 < n and text[i + 1] == "/":
            nl = text.find("\n", i + 2)
            i = n if nl == -1 else nl + 1
            continue

        if text.startswith("buildSettings", i) and (
            i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")
        ):
            m = BUILD_SETTINGS_HEADER_RE.match(text, i)
            if m:
                open_brace = m.end() - 1
                close_brace = find_matching_brace(text, open_brace)
                if close_brace != -1:
                    out.append(text[last_emit : open_brace + 1])
                    body = text[open_brace + 1 : close_brace]
                    out.append(
                        patch_openstep_body(body, disable_codesign, cas_dir)
                    )
                    out.append("}")
                    last_emit = close_brace + 1
                    i = close_brace + 1
                    continue
        i += 1

    out.append(text[last_emit:])
    return "".join(out)


def discover_project_files() -> List[str]:
    files = set()
    for pattern in ("*.xcodeproj/project.pbxproj", "*.xcodeproj/project.xcproj"):
        files.update(glob.glob(pattern))
    return sorted(files)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--disable-codesign", action="store_true")
    parser.add_argument("--cas-dir", default=None)
    parser.add_argument("--stamp-file", default="xtool/.boop-project-inputs.json")
    args = parser.parse_args()

    cas_dir = os.path.expanduser(args.cas_dir) if args.cas_dir else None
    project_files = discover_project_files()

    for name in project_files:
        p = Path(name)
        original = p.read_text(encoding="utf-8")
        if args.disable_codesign or cas_dir:
            if original.lstrip().startswith("{"):
                patched = patch_json_text(original, args.disable_codesign, cas_dir)
            else:
                patched = patch_openstep_text(
                    original, args.disable_codesign, cas_dir
                )
            if patched != original:
                p.write_text(patched, encoding="utf-8")

    # Preserve the timestamp of the final patched project across checksum syncs.
    # Swift Build keys parts of its dependency graph on the project file timestamp.
    stamp_path = Path(args.stamp_file)
    old = json.loads(stamp_path.read_text()) if stamp_path.is_file() else {}
    current = {}
    for name in project_files:
        p = Path(name)
        digest = hashlib.sha256(p.read_bytes()).hexdigest()
        st = p.stat()
        stamp = st.st_mtime_ns
        if name in old and old[name][0] == digest:
            stamp = old[name][1]
            os.utime(p, ns=(st.st_atime_ns, stamp))
        current[name] = [digest, stamp]
    stamp_path.parent.mkdir(parents=True, exist_ok=True)
    stamp_path.write_text(json.dumps(current))


if __name__ == "__main__":
    main()
