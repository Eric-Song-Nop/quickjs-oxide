from pathlib import Path
import sys

path = Path(sys.argv[1])
before = sys.argv[2]
after = sys.argv[3]
source = path.read_text(encoding="utf-8")
if before not in source and path.with_suffix("").is_dir():
    matches = [candidate for candidate in path.with_suffix("").rglob("*.rs")
               if before in candidate.read_text(encoding="utf-8")]
    if len(matches) == 1:
        path = matches[0]
        source = path.read_text(encoding="utf-8")
if source.count(before) != 1:
    raise SystemExit(f"rewrite canary expected one occurrence of {before!r}")
path.write_text(source.replace(before, after), encoding="utf-8")
