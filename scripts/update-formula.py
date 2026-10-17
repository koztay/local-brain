#!/usr/bin/env python3
"""Point Formula/local-brain.rb at a release: version, archive URLs and their SHA-256 sums.

Usage: update-formula.py <version> <SHA256SUMS> [formula]
"""
import re
import sys


def main() -> None:
    version, sums_path = sys.argv[1].lstrip("v"), sys.argv[2]
    formula = sys.argv[3] if len(sys.argv) > 3 else "Formula/local-brain.rb"
    sums = {}
    for line in open(sums_path):
        parts = line.split()
        if len(parts) == 2:
            sums[parts[1].lstrip("*")] = parts[0]

    text = open(formula).read()
    text = re.sub(r'^(\s*version ")[^"]+(")', rf"\g<1>{version}\g<2>", text, count=1, flags=re.M)
    text = re.sub(r"/releases/download/v[^/]+/", f"/releases/download/v{version}/", text)

    def fix_sha(m: re.Match) -> str:
        archive = m.group(2)
        if archive not in sums:
            sys.exit(f"{archive} is missing from {sums_path}")
        return f'{m.group(1)}{m.group(3)}sha256 "{sums[archive]}"'

    text, n = re.subn(
        r'(url "[^"]*/(local-brain-[^"/]+)"\n)(\s*)sha256 "[0-9a-f]*"', fix_sha, text
    )
    if n == 0:
        sys.exit("no url/sha256 pairs found in the formula")
    open(formula, "w").write(text)
    print(f"{formula}: version {version}, {n} checksums updated")


if __name__ == "__main__":
    main()
