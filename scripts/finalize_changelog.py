#!/usr/bin/env python3
"""Deterministic changelog finalization for the llmu release process.

Two commands, stdlib-only:

  finalize_changelog.py finalize --changelog PATH --cargo PATH [--date YYYY-MM-DD]
      Reads the root ``[package]`` version from Cargo.toml and renames the
      non-empty ``## [Unreleased]`` section into exactly one dated version
      section, recreating one empty ``## [Unreleased]`` directly above it.
      Intro text and older sections are preserved byte-for-byte. Re-running
      on an already-finalized working copy (target version directly below an
      empty Unreleased heading) is a byte-level no-op that preserves the
      original release date. Fails loudly when Unreleased is missing or
      empty, or when the target version already exists elsewhere.

  finalize_changelog.py extract --version X.Y.Z --changelog PATH
      Emits exactly the requested version's heading and body to stdout for
      GitHub Release notes, and fails loudly if that version has no section.

The changelog is replaced atomically in its own directory.
"""

import argparse
import os
import re
import shutil
import sys
import tempfile
from datetime import date, datetime, timezone

PROG = "finalize_changelog.py"
VERSION_RE = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+$")
DATE_RE = re.compile(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}$")


def fail(message):
    print(f"{PROG}: {message}", file=sys.stderr)
    sys.exit(1)


def cargo_package_version(path):
    """Return the root `[package]` version as a string, or exit loudly."""
    try:
        with open(path, "r", encoding="utf-8", newline="") as fh:
            lines = fh.read().splitlines()
    except OSError as exc:
        fail(f"cannot read Cargo.toml at {path}: {exc}")
    table = None
    for line in lines:
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            table = stripped[1:-1].strip()
            continue
        if table != "package":
            continue
        match = re.match(r'^version\s*=\s*(["\'])([^"\']+)\1\s*(#.*)?$', stripped)
        if match:
            version = match.group(2)
            if not VERSION_RE.match(version):
                fail(f"Cargo.toml at {path} has a non-SemVer version {version!r}")
            return version
    fail(f"no root [package] version found in Cargo.toml at {path}")


def utc_today_iso():
    return datetime.now(timezone.utc).date().isoformat()


def validate_date(value):
    if not DATE_RE.match(value):
        fail(
            f"invalid --date {value!r}: expected strict YYYY-MM-DD"
            " (e.g. 2026-08-11)"
        )
    try:
        date.fromisoformat(value)
    except ValueError:
        fail(f"invalid --date {value!r}: not a real calendar date")


def read_changelog(path):
    try:
        with open(path, "rb") as fh:
            data = fh.read()
    except OSError as exc:
        fail(f"cannot read changelog at {path}: {exc}")
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError as exc:
        fail(f"changelog at {path} is not valid UTF-8: {exc}")
    return text


def parse_sections(text):
    """Split text into (heading_line, body, offset) tuples at `## ` headings.

    heading_line keeps the leading `## ` and trailing newline; body keeps all
    lines between it and the next `## ` heading; offset is the byte position
    where the heading line starts in the original text.
    """
    sections = []
    current_heading = None
    body = []
    offset = 0
    pos = 0
    for line in text.splitlines(keepends=True):
        if line.startswith("## "):
            if current_heading is not None:
                sections.append((current_heading, "".join(body), offset))
            current_heading = line
            body = []
            offset = pos
        elif current_heading is not None:
            body.append(line)
        pos += len(line)
    if current_heading is not None:
        sections.append((current_heading, "".join(body), offset))
    return sections


def heading_version(heading):
    match = re.match(r"^## \[([^\]]+)\]", heading)
    return match.group(1) if match else None


def section_matches_version(heading, version):
    return re.match(r"^## \[" + re.escape(version) + r"\]", heading) is not None


def has_release_notes(body):
    """True if the section body has any note beyond headings/comments/blanks.

    Comments (including multi-line `<!-- ... -->` blocks), markdown headings,
    and blank lines alone do not count as release notes.
    """
    in_comment = False
    for line in body.splitlines():
        stripped = line.strip()
        if in_comment:
            if "-->" in stripped:
                in_comment = False
            continue
        if stripped.startswith("<!--"):
            if "-->" not in stripped[len("<!--"):]:
                in_comment = True
            continue
        if not stripped:
            continue
        if stripped.startswith("#"):
            continue
        return True
    return False


def find_unreleased(sections, path):
    for index, (heading, _, _) in enumerate(sections):
        if heading_version(heading) == "Unreleased":
            return index
    fail(f"no '## [Unreleased]' section found in {path}")


def atomic_write(path, content):
    directory = os.path.dirname(os.path.abspath(path))
    try:
        fd, tmp = tempfile.mkstemp(prefix=".changelog-", dir=directory)
        try:
            with os.fdopen(fd, "wb") as fh:
                fh.write(content)
            try:
                shutil.copymode(path, tmp)
            except OSError:
                pass
            os.replace(tmp, path)
        except BaseException:
            try:
                os.unlink(tmp)
            except OSError:
                pass
            raise
    except OSError as exc:
        fail(f"cannot atomically replace {path}: {exc}")


def cmd_finalize(args):
    if args.date is None:
        args.date = utc_today_iso()
    validate_date(args.date)
    version = cargo_package_version(args.cargo)
    text = read_changelog(args.changelog)
    sections = parse_sections(text)
    unreleased_idx = find_unreleased(sections, args.changelog)
    heading, body, offset = sections[unreleased_idx]
    next_heading = (
        sections[unreleased_idx + 1][0]
        if unreleased_idx + 1 < len(sections)
        else None
    )

    target_found = any(
        i != unreleased_idx and section_matches_version(h, version)
        for i, (h, _, _) in enumerate(sections)
    )
    if has_release_notes(body):
        if target_found:
            fail(
                f"version {version} already has a section in {args.changelog};"
                " refusing to create a duplicate"
            )
    else:
        if next_heading is not None and section_matches_version(next_heading, version):
            sys.exit(0)
        if target_found:
            fail(
                f"version {version} already has a section in {args.changelog};"
                " refusing to create a duplicate"
            )
        fail(
            f"the '## [Unreleased]' section in {args.changelog} contains no"
            " release notes (headings, comments, and blank lines do not count)"
        )

    intro = text[:offset]
    tail = text[offset + len(heading):]
    new_text = (
        intro
        + "## [Unreleased]\n\n"
        + f"## [{version}] - {args.date}\n"
        + tail
    )
    atomic_write(args.changelog, new_text.encode("utf-8"))
    return 0


def cmd_extract(args):
    if not VERSION_RE.match(args.version):
        fail(
            f"invalid --version {args.version!r}: expected SemVer X.Y.Z"
            " (e.g. 0.1.0)"
        )
    text = read_changelog(args.changelog)
    matches = [
        (heading, body)
        for heading, body, _ in parse_sections(text)
        if section_matches_version(heading, args.version)
    ]
    if not matches:
        fail(
            f"version {args.version} has no section in {args.changelog}"
        )
    if len(matches) > 1:
        fail(
            f"version {args.version} appears more than once in {args.changelog};"
            " ambiguous extraction"
        )
    heading, body = matches[0]
    sys.stdout.write(heading)
    sys.stdout.write(body)
    sys.stdout.flush()
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(
        prog=PROG,
        description="Transition the changelog Unreleased notes into a dated "
        "version section, or extract one version section.",
    )
    subparsers = parser.add_subparsers(dest="command", required=True)
    finalize = subparsers.add_parser("finalize", help="finalize Unreleased notes")
    finalize.add_argument("--changelog", required=True, metavar="PATH")
    finalize.add_argument("--cargo", required=True, metavar="PATH")
    finalize.add_argument("--date", default=None, metavar="YYYY-MM-DD")
    extract = subparsers.add_parser("extract", help="extract one version section")
    extract.add_argument("--version", required=True, metavar="X.Y.Z")
    extract.add_argument("--changelog", required=True, metavar="PATH")
    args = parser.parse_args(argv)
    if args.command == "finalize":
        return cmd_finalize(args)
    return cmd_extract(args)


if __name__ == "__main__":
    main()
