#!/usr/bin/env python3
"""Validate or export the exact public file list using only the Python stdlib.

    python scripts/export_open_source.py --check
    python scripts/export_open_source.py --output ../Server-open-source

The output directory must not exist; missing parent directories are created.
The manifest is .open-source-files.txt at the workspace root. No Git commands
are run. Failed exports are reported and never removed automatically.
"""

from __future__ import annotations

import argparse
import hashlib
import os
from pathlib import Path, PurePosixPath, PureWindowsPath
import re
import stat
import sys
from dataclasses import dataclass
from typing import Sequence


MANIFEST_NAME = ".open-source-files.txt"
CHECKSUM_NAME = "SHA256SUMS"
FORBIDDEN_DIRECTORIES = frozenset(
    {
        "katago", "worker", "models", "reports", ".runtime", ".venv-tools",
        "target", "target-linux", ".git", ".build", "__pycache__", "node_modules",
        ".ssh", ".aws", ".azure", ".gcloud",
    }
)
FORBIDDEN_SUFFIXES = frozenset(
    {
        ".exe", ".dll", ".bin", ".zip", ".sgf", ".gz", ".bz2", ".xz",
        ".7z", ".rar", ".tar", ".tgz", ".pt", ".pth", ".onnx",
        ".safetensors", ".pb", ".npy", ".npz", ".sqlite", ".sqlite3",
        ".db", ".log", ".pem", ".key", ".p12", ".pfx", ".so",
        ".dylib", ".pyc", ".pyo", ".class", ".o", ".obj", ".a", ".lib",
        ".srl", ".ckpt", ".weights", ".model", ".tflite",
    }
)
SENSITIVE_NAMES = frozenset(
    {"id_rsa", "id_dsa", "id_ecdsa", "id_ed25519", "credentials", "credentials.json"}
)
ENV_EXAMPLES = frozenset({".env.example", ".env.sample", ".env.template"})
WINDOWS_RESERVED = re.compile(r"^(?:con|prn|aux|nul|com[1-9]|lpt[1-9])(?:\.|$)", re.I)

# Findings identify the type and source line, never the matched credential.
SECRET_PATTERNS = (
    ("private key", re.compile(r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY-----")),
    ("GitHub token", re.compile(r"\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{30,})\b")),
    ("AWS access key", re.compile(r"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b")),
    ("Google API key", re.compile(r"\bAIza[A-Za-z0-9_-]{30,}\b")),
    ("Slack token", re.compile(r"\bxox[baprs]-[A-Za-z0-9-]{15,}\b")),
    ("API secret token", re.compile(r"\bsk-(?:proj-|ant-[A-Za-z0-9]+-)?[A-Za-z0-9_-]{20,}\b")),
    ("credential in URL", re.compile(r"\b[a-z][a-z0-9+.-]*://[^\s/:@]+:([^\s/@]+)@", re.I)),
)
LITERAL_CREDENTIAL = re.compile(
    r"[\"']?(?:api[_-]?key|access[_-]?token|auth[_-]?token|client[_-]?secret|"
    r"secret[_-]?key|aws[_-]?secret[_-]?access[_-]?key|password|passwd|token)"
    r"[\"']?\s*[:=]\s*[\"']([^\"'\r\n]+)[\"']",
    re.I,
)
UNQUOTED_CREDENTIAL = re.compile(
    r"^\s*(?:export\s+)?(?:[A-Z][A-Z0-9]*_)*(?:API_KEY|ACCESS_TOKEN|AUTH_TOKEN|"
    r"CLIENT_SECRET|SECRET_KEY|SECRET_ACCESS_KEY|PASSWORD|PASSWD|TOKEN)"
    r"\s*=\s*([^\s#\"']+)\s*(?:#.*)?$",
    re.I,
)
PLACEHOLDER = re.compile(
    r"^(?:<[^>]+>|\$\{[^}]+\}|\$[A-Z_][A-Z0-9_]*|\{\{.*\}\}|"
    r"(?:your|replace|example|sample|dummy|test|placeholder|insert)[_-].*|"
    r"change[-_]?me|redacted|none|null|false|true|x{3,}|\*{3,})$",
    re.I,
)


class ExportError(Exception):
    """A validation or export failure safe to display without secret values."""


@dataclass(frozen=True)
class SourceFile:
    relative_path: str
    content: bytes


def _reject_links(path: Path) -> None:
    """Check each existing ancestor, including Windows junction/reparse points."""
    for part in reversed((path, *path.parents)):
        try:
            info = part.lstat()
        except FileNotFoundError:
            continue
        except OSError:
            raise ExportError("Cannot inspect a source or output path.") from None
        if stat.S_ISLNK(info.st_mode) or (
            getattr(info, "st_file_attributes", 0)
            & getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0x400)
        ):
            raise ExportError("Symbolic links and Windows reparse points are forbidden.")


def _validate_relative_path(value: str, line_number: int) -> str:
    location = f"{MANIFEST_NAME}:{line_number}"
    posix = PurePosixPath(value)
    windows = PureWindowsPath(value)
    if posix.is_absolute() or windows.drive or windows.root:
        raise ExportError(f"{location}: absolute or drive-relative paths are forbidden.")
    if "\\" in value:
        raise ExportError(f"{location}: use forward slashes for portable relative paths.")
    parts = value.split("/")
    if any(part in {"", ".", ".."} for part in parts):
        raise ExportError(f"{location}: empty, '.' and '..' path components are forbidden.")
    if any(ord(char) < 32 or char in '<>:"|?*[]{}' for char in value):
        raise ExportError(f"{location}: wildcards and non-portable filename characters are forbidden.")
    if any(part.endswith((" ", ".")) or WINDOWS_RESERVED.match(part) for part in parts):
        raise ExportError(f"{location}: non-portable filename is forbidden.")
    folded = [part.casefold() for part in parts]
    if any(part in FORBIDDEN_DIRECTORIES for part in folded):
        raise ExportError(f"{location}: forbidden private or generated directory.")
    if any(
        (part.startswith(".env") and part not in ENV_EXAMPLES)
        or part in SENSITIVE_NAMES
        for part in folded
    ):
        raise ExportError(f"{location}: sensitive filename is forbidden.")
    if any(suffix.casefold() in FORBIDDEN_SUFFIXES for suffix in posix.suffixes):
        raise ExportError(f"{location}: binary, model, archive or sensitive artifact is forbidden.")
    if value.casefold() == CHECKSUM_NAME.casefold():
        raise ExportError(f"{location}: {CHECKSUM_NAME} is reserved for generated checksums.")
    return value


def _is_placeholder(value: str) -> bool:
    return not value.strip() or bool(PLACEHOLDER.fullmatch(value.strip()))


def _scan_secrets(relative_path: str, content: bytes) -> list[str]:
    try:
        decoded = content.decode("utf-8-sig")
    except UnicodeDecodeError:
        raise ExportError(f"{relative_path}: only UTF-8 source files may be exported.") from None
    if "\x00" in decoded:
        raise ExportError(f"{relative_path}: binary content is forbidden.")
    findings = []
    for line_number, line in enumerate(decoded.splitlines(), 1):
        types = set()
        for label, pattern in SECRET_PATTERNS:
            for match in pattern.finditer(line):
                candidate = match.group(1) if label == "credential in URL" else match.group()
                if not _is_placeholder(candidate):
                    types.add(label)
        for pattern in (LITERAL_CREDENTIAL, UNQUOTED_CREDENTIAL):
            for match in pattern.finditer(line):
                if not _is_placeholder(match.group(1)):
                    types.add("literal credential")
        findings.extend(f"{relative_path}:{line_number}: potential {label}." for label in sorted(types))
    return findings


def validate_sources(root: Path) -> list[SourceFile]:
    """Read and validate the exact manifest; return checked byte snapshots."""
    root = Path(os.path.abspath(root))
    manifest = root / MANIFEST_NAME
    _reject_links(manifest)
    try:
        if not manifest.is_file():
            raise ExportError(f"{MANIFEST_NAME}: manifest must be a regular file.")
        manifest_text = manifest.read_text(encoding="utf-8-sig")
    except (OSError, UnicodeDecodeError):
        raise ExportError(f"{MANIFEST_NAME}: cannot read a UTF-8 manifest.") from None
    entries = []
    seen = set()
    for line_number, line in enumerate(manifest_text.splitlines(), 1):
        value = line.strip()
        if not value or value.startswith("#"):
            continue
        relative_path = _validate_relative_path(value, line_number)
        folded = relative_path.casefold()
        if folded in seen:
            raise ExportError(f"{MANIFEST_NAME}:{line_number}: duplicate path (case-insensitive).")
        seen.add(folded)
        entries.append(relative_path)
    if not entries:
        raise ExportError(f"{MANIFEST_NAME}: the public file list is empty.")
    sources = []
    findings = []
    for relative_path in entries:
        source = root.joinpath(*relative_path.split("/"))
        try:
            _reject_links(source)
            if not stat.S_ISREG(source.stat().st_mode):
                raise ExportError(f"{relative_path}: source must be a regular file.")
            content = source.read_bytes()
        except OSError:
            raise ExportError(f"{relative_path}: source is missing or cannot be read.") from None
        findings.extend(_scan_secrets(relative_path, content))
        sources.append(SourceFile(relative_path, content))
    if findings:
        raise ExportError("Credential check failed:\n" + "\n".join(findings))
    return sources


def export_sources(root: Path, output: Path) -> list[SourceFile]:
    """Export checked snapshots to a new directory without modifying old files."""
    output = Path(os.path.abspath(output))
    if os.path.lexists(output):
        raise ExportError("Output already exists; choose a new directory.")
    _reject_links(output.parent)
    sources = validate_sources(root)
    created = False
    try:
        _reject_links(output.parent)
        created = True
        output.parent.mkdir(parents=True, exist_ok=True)
        _reject_links(output.parent)
        output.mkdir()
        checksums = []
        for source in sources:
            target = output.joinpath(*source.relative_path.split("/"))
            _reject_links(target.parent)
            target.parent.mkdir(parents=True, exist_ok=True)
            _reject_links(target)
            with target.open("xb") as stream:
                stream.write(source.content)
            checksums.append(f"{hashlib.sha256(source.content).hexdigest()}  {source.relative_path}\n")
        with (output / CHECKSUM_NAME).open("x", encoding="utf-8", newline="\n") as stream:
            stream.writelines(checksums)
    except (OSError, ExportError):
        suffix = " Any partially created directories were left in place; nothing was deleted." if created else ""
        raise ExportError("Export failed; no existing files were overwritten." + suffix) from None
    return sources


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--check", action="store_true", help="Validate the manifest and source files without exporting.")
    action.add_argument("--output", type=Path, metavar="NEW_DIRECTORY", help="Export to a new directory, creating missing parents.")
    args = parser.parse_args(argv)
    root = Path(__file__).absolute().parent.parent
    try:
        sources = validate_sources(root) if args.check else export_sources(root, args.output)
    except ExportError as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 1
    total_bytes = sum(len(source.content) for source in sources)
    print(f"Validated {len(sources)} public files ({total_bytes:,} bytes); credential checks passed.")
    if args.output is not None:
        print(f"Exported to {os.path.abspath(args.output)} with {CHECKSUM_NAME}.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
