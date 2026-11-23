#!/usr/bin/env python3
"""Reject unapproved endpoint and repository references in the Git index."""

import re
import subprocess
import sys
from pathlib import PurePosixPath


# Exact hosts only: an approved public service does not approve its subdomains.
PUBLIC_HOSTS = {
    "api.cohere.com",
    "api.e2b.dev",
    "api.jina.ai",
    "api.openai.com",
    "api.voyageai.com",
    "crates.io",
    "generativelanguage.googleapis.com",
    "github.com",
    "huggingface.co",
    "openrouter.ai",
}
FIXTURE_HOSTS = {
    "localhost", "local", "127.0.0.1", "169.254.169.254", "1.1.1.1",
    "example.test", "sandbox.example.com", "sandbox.example.test", "offline.invalid",
}
WEB_TEST_SOURCE = "crates/agent-tools/src/web.rs"
ADDRESS_FIXTURES = {
    "0.0.0.0", "10.0.0.1", "100.64.0.0", "100.64.0.1", "172.16.0.1",
    "192.168.1.1", "198.18.0.0", "93.184.216.34",
}
# Dotted language/build syntax, not hosts. Only exact contexts are allowed.
SYNTAX = {
    ("crates/agent-memory/src/raw.rs", "m.dev"): "m.dev()",
    ("Cargo.toml", "profile.dev"): "[profile.dev.package.",
    ("Cargo.toml", "profile.test"): "[profile.test.package.",
}
POLICY_SOURCE = "scripts/check-oss-purity.py"
# Shared hosting is not evidence that the target repository is public.
GITHUB_PATHS = (
    "/microsoft/onnxruntime/releases/download/",
    "/rust-lang/crates.io-index",
    "/sylphxai/agent-kit",
)
LABEL = r"[a-z0-9](?:[a-z0-9-]*[a-z0-9])?"
URL = re.compile(
    r"\b[a-z][a-z0-9+.-]*://(?P<authority>[^/\s<>\"'{}()\x60]+)"
    r"(?P<path>/[^\s<>\"'{}()\x60?#]*)?",
    re.IGNORECASE,
)
GITHUB = re.compile(r"(?<![\w.-])github\.com[/:]([^\s<>\"'{}()\x60?#]+)", re.IGNORECASE)
EMAIL = re.compile(rf"(?<![\w@])\b[\w.+%/-]+@(?:{LABEL}\.)+[a-z]{{2,}}\b", re.IGNORECASE)
# Catch bare DNS names in prose, configuration, and paths too. Limit the suffixes
# here so ordinary source syntax (foo.len, foo.rs) isn't interpreted as DNS.
BARE_DNS = re.compile(
    rf"(?<![\w.-])(?:{LABEL}\.)+(?:ai|app|cloud|com|corp|dev|example|internal|intranet|"
    r"invalid|io|lan|local|localhost|net|org|private|svc|test|cluster|co)(?![\w-])",
    re.IGNORECASE,
)
BARE_IP = re.compile(r"(?<![\w.])(?:\d{1,3}\.){3}\d{1,3}(?![\w.])")
def known_github_path(path):
    path = path.lower().rstrip(".,;:!}")
    return any(path == allowed or (allowed.endswith("/") and path.startswith(allowed))
               for allowed in GITHUB_PATHS)


def issue(line, filename):
    if EMAIL.search(line):
        return "email addresses are not allowed in source"
    for url in URL.finditer(line):
        authority = url["authority"].lower().rstrip(".,;:!")
        host = re.sub(r":\d+$", "", authority).rstrip(".")
        if host not in PUBLIC_HOSTS | FIXTURE_HOSTS:
            return "URL uses an unapproved host or authority"
    for reference in GITHUB.finditer(line):
        if not known_github_path("/" + reference[1]):
            return "unapproved GitHub repository reference"
    for match in BARE_DNS.finditer(line):
        hostname = match[0].lower()
        syntax = SYNTAX.get((filename, hostname))
        if syntax and syntax in line:
            continue
        if filename == POLICY_SOURCE and any(hostname == name for _, name in SYNTAX):
            continue
        if hostname not in PUBLIC_HOSTS | FIXTURE_HOSTS:
            return "unapproved domain reference"
    for address in BARE_IP.findall(line):
        if address not in FIXTURE_HOSTS and not (
            filename in (WEB_TEST_SOURCE, POLICY_SOURCE) and address in ADDRESS_FIXTURES
        ):
            return "unapproved IP address"
    return None


def git(*args):
    return subprocess.run(["git", *args], check=True, stdout=subprocess.PIPE).stdout


def main():
    if git("rev-parse", "--show-prefix").strip():
        raise ValueError("run from the repository root")
    entries = git("ls-files", "--stage", "-z").split(b"\0")[:-1]
    if not entries:
        raise ValueError("no tracked files to scan")
    failed = False
    for entry in entries:
        header, path = entry.split(b"\t", 1)
        mode, oid, stage = header.split()
        filename = path.decode("utf-8")
        if mode not in (b"100644", b"100755") or stage != b"0":
            print("OSS purity: unsupported Git entry (symlink, submodule or conflict)", file=sys.stderr)
            failed = True
            continue
        if any(issue(part, "") for part in PurePosixPath(filename).parts):
            print("OSS purity: unapproved reference in a tracked path", file=sys.stderr)
            failed = True
            continue
        blob = git("cat-file", "blob", oid.decode("ascii"))
        try:
            text = blob.decode("utf-8")
            if "\0" in text:
                raise ValueError("binary content")
        except ValueError:
            print(f"OSS purity: {filename}: expected UTF-8 text", file=sys.stderr)
            failed = True
            continue
        for lineno, line in enumerate(text.splitlines(), 1):
            problem = issue(line, filename)
            if problem:
                print(f"OSS purity: {filename}:{lineno}: {problem}", file=sys.stderr)
                failed = True
    if not failed:
        print(f"OSS purity: {len(entries)} tracked text files passed")
    return int(failed)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, subprocess.SubprocessError, ValueError):
        sys.exit("OSS purity: could not completely scan the Git index")
