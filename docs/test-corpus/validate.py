#!/usr/bin/env python3
"""Validate the self-contained JJ conflict corpus with Python's stdlib only."""
from __future__ import annotations

import hashlib
import json
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
PROJECT = ROOT.parent.parent
PIN = "6b27ec86af32ff84c209367d2710d234cb192622"
REPO = "https://github.com/jj-vcs/jj"
REQUIRED_SUPPORTED = {
    "snapshot-basic-2-sided",
    "snapshot-3-sided-with-multiple-bases",
    "snapshot-multiple-regions",
    "snapshot-long-markers-and-marker-like-content",
    "snapshot-missing-final-newlines",
    "snapshot-crlf",
    "snapshot-custom-labels",
    "snapshot-empty-term-or-deletion",
}
REQUIRED_REFERENCES = {
    "default-diff-style-2-sided",
    "git-diff3-style-2-sided",
    "malformed-missing-section-header",
    "malformed-missing-diff",
    "malformed-mixed-header-style",
    "wrong-arity",
    "git-too-many-sides",
    "diff-whitespace-stripped",
}


class ValidationError(Exception):
    pass


def fail(message: str):
    raise ValidationError(message)


def read_json(path: Path):
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except Exception as exc:
        fail(f"{path}: invalid JSON: {exc}")


def safe_rel(value: str, context: str) -> Path:
    if not isinstance(value, str) or not value or os.path.isabs(value):
        fail(f"{context}: path must be repository-relative: {value!r}")
    p = Path(value)
    if any(part in ("", ".", "..") for part in p.parts):
        fail(f"{context}: path contains unsafe component: {value!r}")
    if value.startswith("/") or re.match(r"^[A-Za-z]:", value):
        fail(f"{context}: absolute path: {value!r}")
    return p


def read_artifact(rel: str, context: str) -> bytes:
    p = safe_rel(rel, context)
    full = PROJECT / p
    try:
        resolved = full.resolve(strict=False)
    except OSError as exc:
        fail(f"{context}: cannot resolve {rel!r}: {exc}")
    try:
        resolved.relative_to(PROJECT.resolve())
    except ValueError:
        fail(f"{context}: path escapes repository: {rel!r}")
    if full.is_symlink():
        fail(f"{context}: symlink is not allowed: {rel}")
    if not full.is_file():
        fail(f"{context}: missing fixture: {rel}")
    return full.read_bytes()


def byte_facts(data: bytes):
    if b"\r\n" in data:
        rest = data.replace(b"\r\n", b"")
        eol = "mixed" if b"\n" in rest or b"\r" in rest else "crlf"
    elif b"\n" in data:
        eol = "lf"
    else:
        eol = "none"
    return {
        "byte_length": len(data),
        "sha256": hashlib.sha256(data).hexdigest(),
        "line_ending_mode": eol,
        "final_newline": data.endswith((b"\n", b"\r")),
    }


def iter_lines(data: bytes):
    pos = 0
    while pos < len(data):
        end = pos
        while end < len(data) and data[end] not in b"\r\n":
            end += 1
        if end == len(data):
            yield pos, data[pos:]
            return
        if data[end:end + 2] == b"\r\n":
            end += 2
        else:
            end += 1
        yield pos, data[pos:end]
        pos = end


def split_line(line: bytes):
    if line.endswith(b"\r\n"):
        return line[:-2], b"\r\n"
    if line.endswith((b"\n", b"\r")):
        return line[:-1], line[-1:]
    return line, b""


def parse_snapshot(data: bytes, context: str):
    regions = []
    current = None
    for start, line in iter_lines(data):
        body, eol = split_line(line)
        if current is None:
            if body.startswith(b"<<<<<<<"):
                width = len(body) - len(body.lstrip(b"<"))
                if width < 7:
                    fail(f"{context}: opening marker shorter than seven at byte {start}")
                current = {"start": start, "width": width, "sections": []}
            continue
        width = current["width"]
        if body.startswith(b">" * width):
            current["close_start"] = start
            current["close_end"] = start + len(line)
            current["close_final_newline"] = bool(eol)
            regions.append(current)
            current = None
            continue
        if body.startswith(b"+" * width) or body.startswith(b"-" * width):
            kind = "side" if body.startswith(b"+" * width) else "base"
            current["sections"].append({
                "header_start": start,
                "header_end": start + len(line),
                "kind": kind,
                "label": body[width:].decode("utf-8"),
            })
    if current is not None:
        fail(f"{context}: unterminated snapshot region")
    if not regions:
        fail(f"{context}: no snapshot region")
    for region in regions:
        if len(region["sections"]) < 2 or region["sections"][0]["kind"] != "side":
            fail(f"{context}: invalid snapshot section sequence")
        for i, section in enumerate(region["sections"]):
            end = (region["sections"][i + 1]["header_start"]
                   if i + 1 < len(region["sections"])
                   else region["close_start"])
            payload = data[section["header_end"]:end]
            section["synthetic_separator_eol"] = not region["close_final_newline"]
            if section["synthetic_separator_eol"]:
                if payload.endswith(b"\r\n"):
                    payload = payload[:-2]
                elif payload.endswith((b"\n", b"\r")):
                    payload = payload[:-1]
                else:
                    fail(f"{context}: missing synthetic separator in section {i}")
            section["payload"] = payload
    return regions


def reconstruct(data: bytes, regions):
    result = bytearray()
    cursor = 0
    for region in regions:
        result.extend(data[cursor:region["start"]])
        result.extend(region["sections"][0]["payload"])
        cursor = region["close_end"]
    result.extend(data[cursor:])
    return bytes(result)


def check_artifact(rel: str, declared: dict, context: str):
    data = read_artifact(declared["path"], f"{context} artifact {rel}")
    actual = byte_facts(data)
    for key in ("byte_length", "sha256", "line_ending_mode", "final_newline"):
        if declared.get(key) != actual[key]:
            fail(f"{context} artifact {rel}: {key}={declared.get(key)!r}, actual {actual[key]!r}")
    if "synthetic_separator_eol" in declared and not isinstance(declared["synthetic_separator_eol"], bool):
        fail(f"{context} artifact {rel}: synthetic_separator_eol must be boolean")
    return data


def load_cases():
    index = read_json(ROOT / "index.json")
    if index.get("upstream", {}).get("repository") != REPO or index.get("upstream", {}).get("commit") != PIN:
        fail("index.json: upstream repository or pinned commit is incorrect")
    cases = {}
    for entry in index.get("cases", []):
        name = entry.get("case_name")
        if not isinstance(name, str) or name in cases:
            fail(f"index.json: duplicate or missing case_name: {name!r}")
        root = ROOT / ("cases" if entry.get("status") == "supported" else "reference") / name
        meta = read_json(root / "case.json")
        cases[name] = (entry, meta, root)
    return cases


def validate_corpus_layout_and_hashes():
    cases = load_cases()
    if REQUIRED_SUPPORTED - set(cases):
        fail(f"missing supported cases: {sorted(REQUIRED_SUPPORTED - set(cases))}")
    if REQUIRED_REFERENCES - set(cases):
        fail(f"missing reference cases: {sorted(REQUIRED_REFERENCES - set(cases))}")
    if len(cases) != len(REQUIRED_SUPPORTED | REQUIRED_REFERENCES):
        fail(f"unexpected case count: {len(cases)}")
    for name, (entry, meta, root) in cases.items():
        context = f"{name}"
        if meta.get("case_name") != name or meta.get("schema_version") != 1:
            fail(f"{context}: invalid case metadata identity/schema")
        status = meta.get("status")
        expected_status = entry.get("status")
        if status != expected_status:
            fail(f"{context}: status mismatch between index and case.json")
        if status not in ("supported", "reference"):
            fail(f"{context}: unsupported status {status!r}")
        source = meta.get("source", {})
        if source.get("url", "").find(PIN) < 0 or source.get("path", "").startswith("/"):
            fail(f"{context}: invalid pinned source metadata")
        artifacts = meta.get("artifacts")
        if not isinstance(artifacts, dict) or "input.snapshot" not in artifacts:
            fail(f"{context}: artifact map must include input.snapshot")
        for rel, declared in artifacts.items():
            safe_rel(declared.get("path", ""), f"{context} artifact {rel}")
            check_artifact(rel, declared, context)
        indexed = entry.get("input_path")
        if indexed != artifacts["input.snapshot"]["path"]:
            fail(f"{context}: index input_path disagrees with case artifact map")
        indexed_facts = {key: entry.get(key) for key in ("sha256", "byte_length", "line_ending_mode", "final_newline")}
        input_facts = {key: artifacts["input.snapshot"].get(key) for key in indexed_facts}
        if indexed_facts != input_facts:
            fail(f"{context}: index input facts disagree with case artifact map")
        if status == "supported":
            if meta.get("style") != "snapshot" or not isinstance(meta.get("region_count"), int):
                fail(f"{context}: invalid supported metadata")
            regions = meta.get("regions", [])
            if len(regions) != meta["region_count"]:
                fail(f"{context}: region_count does not match regions")
            if "resolved" not in artifacts:
                fail(f"{context}: resolved artifact is missing")
            for ri, region in enumerate(regions):
                if region.get("region_id") != ri:
                    fail(f"{context}: region IDs are not contiguous")
                terms = region.get("terms", [])
                if region.get("term_count") != len(terms) or len(terms) < 2:
                    fail(f"{context}: invalid term count in region {ri}")
                for ti, term in enumerate(terms):
                    if term.get("ordinal") != ti or term.get("kind") not in ("side", "base"):
                        fail(f"{context}: invalid term ordinal/kind in region {ri}")
                    rel = f"regions/region-{ri:03d}/term-{ti:03d}.term"
                    if rel not in artifacts:
                        fail(f"{context}: missing artifact metadata for {rel}")
                    if term.get("path") != artifacts[rel].get("path"):
                        fail(f"{context}: term path mismatch for {rel}")
        else:
            if meta.get("style") != "reference":
                fail(f"{context}: reference style must be reference")
            if meta.get("expected_disposition") not in {"unsupported_style", "malformed", "wrong_arity", "reference_only"}:
                fail(f"{context}: invalid expected_disposition")
            if not isinstance(meta.get("reason"), str) or not meta["reason"].strip():
                fail(f"{context}: reference reason is required")
            if not isinstance(meta.get("upstream_parser_behavior"), str):
                fail(f"{context}: upstream_parser_behavior is required")
    for p in ROOT.rglob("*"):
        if p.is_symlink():
            fail(f"corpus symlink is forbidden: {p.relative_to(PROJECT)}")


def validate_corpus_semantics():
    cases = load_cases()
    for name, (entry, meta, root) in cases.items():
        data = check_artifact("input.snapshot", meta["artifacts"]["input.snapshot"], name)
        if meta["status"] == "supported":
            regions = parse_snapshot(data, name)
            if len(regions) != meta["region_count"]:
                fail(f"{name}: parsed region count differs from metadata")
            for ri, parsed in enumerate(regions):
                declared = meta["regions"][ri]
                if len(parsed["sections"]) != declared["term_count"]:
                    fail(f"{name}: parsed term count differs in region {ri}")
                for ti, section in enumerate(parsed["sections"]):
                    term_meta = declared["terms"][ti]
                    if term_meta["kind"] != section["kind"] or term_meta["label"] != section["label"]:
                        fail(f"{name}: section metadata mismatch at region {ri}, term {ti}")
                    if term_meta["synthetic_separator_eol"] != section["synthetic_separator_eol"]:
                        fail(f"{name}: synthetic separator metadata mismatch at region {ri}, term {ti}")
                    rel = f"regions/region-{ri:03d}/term-{ti:03d}.term"
                    actual = read_artifact(meta["artifacts"][rel]["path"], f"{name} semantic term {rel}")
                    if actual != section["payload"]:
                        fail(f"{name}: term bytes differ at region {ri}, term {ti}")
            resolved = check_artifact("resolved", meta["artifacts"]["resolved"], name)
            if resolved != reconstruct(data, regions):
                fail(f"{name}: resolved scaffold does not reconstruct from term-000")
        else:
            disposition = meta["expected_disposition"]
            if disposition == "wrong_arity" and "expected_arity" not in meta:
                fail(f"{name}: wrong_arity requires expected_arity")
            if disposition == "reference_only" and meta.get("upstream_parser_behavior") != "accepted":
                fail(f"{name}: reference_only must record upstream accepted behavior")


def validate_corpus_byte_properties():
    cases = load_cases()
    for name, (_, meta, _) in cases.items():
        if meta["status"] != "supported":
            continue
        data = check_artifact("input.snapshot", meta["artifacts"]["input.snapshot"], name)
        marker = meta["marker"]
        width = marker["outer_marker_width"]
        if name == "snapshot-long-markers-and-marker-like-content" and width <= 7:
            fail(f"{name}: long-marker width must exceed seven")
        if width < 7:
            fail(f"{name}: marker width is below seven")
        if name == "snapshot-crlf":
            if b"\n" not in data or any(line.endswith(b"\n") and not line.endswith(b"\r\n") for _, line in iter_lines(data)):
                fail(f"{name}: expected all terminated lines to use CRLF")
            if meta["artifacts"]["input.snapshot"]["line_ending_mode"] != "crlf":
                fail(f"{name}: metadata does not declare CRLF")
        if name == "snapshot-missing-final-newlines":
            if data.endswith((b"\n", b"\r")):
                fail(f"{name}: input unexpectedly has a final newline")
            for term, declared in meta["artifacts"].items():
                if term.startswith("regions/region-001/") and declared.get("synthetic_separator_eol") is not True:
                    fail(f"{name}: second-region term missing synthetic separator declaration")
        if name == "snapshot-empty-term-or-deletion":
            empty = read_artifact(meta["artifacts"]["regions/region-000/term-000.term"]["path"], name)
            if empty != b"" or meta["artifacts"]["regions/region-000/term-000.term"]["byte_length"] != 0:
                fail(f"{name}: empty term is not a zero-byte artifact")
        for rel, declared in meta["artifacts"].items():
            if declared.get("line_ending_mode") == "crlf":
                value = read_artifact(declared["path"], f"{name} artifact {rel}")
                remainder = value.replace(b"\r\n", b"")
                if b"\n" in remainder or b"\r" in remainder:
                    fail(f"{name} artifact {rel}: lone EOL in CRLF fixture")


def validate_corpus_documentation_contract():
    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    provenance = (ROOT / "PROVENANCE.md").read_text(encoding="utf-8")
    required_readme = [REPO, PIN, "one-time vendored snapshot", "input.snapshot", "resolved", "term-NNN.term", "reference/", "python3 docs/test-corpus/validate.py", "CRLF", "missing-final"]
    for phrase in required_readme:
        if phrase not in readme:
            fail(f"README.md: missing contract phrase {phrase!r}")
    required_provenance = [REPO, PIN, "lib/tests/test_conflicts.rs", "lib/src/conflicts.rs", "cli/tests/test_resolve_command.rs", "docs/conflicts.md", "Apache License", "adapted test data"]
    for phrase in required_provenance:
        if phrase not in provenance:
            fail(f"PROVENANCE.md: missing provenance phrase {phrase!r}")
    license_path = ROOT / "JJ-LICENSE-APACHE-2.0.txt"
    if not license_path.is_file() or "Apache License" not in license_path.read_text(encoding="utf-8", errors="replace"):
        fail("JJ-LICENSE-APACHE-2.0.txt: Apache license text is missing")
    attrs = (ROOT / ".gitattributes").read_text(encoding="utf-8")
    for phrase in ("cases/**/input.snapshot -text", "cases/**/resolved -text", "cases/**/regions/**/term-*.term -text", "reference/**/input.snapshot -text"):
        if phrase not in attrs:
            fail(f".gitattributes: missing byte rule {phrase!r}")


def test_no_live_jj_dependency():
    for p in PROJECT.rglob("*"):
        if not p.is_file() or p.is_symlink() or ".git" in p.parts or ".jj" in p.parts:
            continue
        try:
            text = p.read_text(encoding="utf-8")
        except (UnicodeDecodeError, OSError):
            continue
        local_jj_path = "/" + "Users/timo/src/" + "jj"
        if local_jj_path in text:
            fail(f"live local JJ checkout path appears in {p.relative_to(PROJECT)}")
    cargo = PROJECT / "Cargo.toml"
    if cargo.exists() and re.search(r"(?im)^\s*(jj|jj-lib|jj_lib)\s*=", cargo.read_text(encoding="utf-8")):
        fail("Cargo.toml: live JJ dependency is forbidden")
    for p in PROJECT.rglob("build.rs"):
        if p.is_file() and "jj" in p.read_text(encoding="utf-8", errors="ignore").lower():
            fail(f"build-time JJ lookup/dependency in {p.relative_to(PROJECT)}")
    source = (ROOT / "validate.py").read_text(encoding="utf-8")
    if "shutil.which(\"jj\")" in source:
        fail("validate.py: validator must not invoke JJ")
    try:
        result = subprocess.run(["git", "check-attr", "--all", "--", str(ROOT / "cases" / "snapshot-crlf" / "input.snapshot")], cwd=PROJECT, capture_output=True, text=True, check=False)
    except OSError as exc:
        fail(f"git check-attr unavailable: {exc}")
    if result.returncode not in (0, 1):
        fail(f"git check-attr failed: {result.stderr.strip()}")
    fixtures = []
    for base in (ROOT / "cases", ROOT / "reference"):
        fixtures.extend(p for p in base.rglob("*") if p.is_file() and (p.name in {"input.snapshot", "resolved"} or p.name.endswith(".term")))
    for fixture in fixtures:
        rel = fixture.relative_to(PROJECT).as_posix()
        result = subprocess.run(["git", "check-attr", "text", "filter", "--", rel], cwd=PROJECT, capture_output=True, text=True, check=False)
        if result.returncode not in (0, 1):
            fail(f"git check-attr failed for {rel}: {result.stderr.strip()}")
        for line in result.stdout.splitlines():
            if ": text: set" in line or ": text: auto" in line:
                fail(f"{rel}: byte-sensitive fixture is treated as text: {line}")
            if ": filter: " in line and not (line.endswith(": filter: unset") or line.endswith(": filter: unspecified")):
                fail(f"{rel}: byte-sensitive fixture has a filter: {line}")


def main():
    checks = [
        validate_corpus_layout_and_hashes,
        validate_corpus_semantics,
        validate_corpus_byte_properties,
        validate_corpus_documentation_contract,
        test_no_live_jj_dependency,
    ]
    try:
        for check in checks:
            check()
            print(f"PASS {check.__name__}")
    except ValidationError as exc:
        print(f"FAIL {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
