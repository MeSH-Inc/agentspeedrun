#!/usr/bin/env python3
"""Write a reproducible .tar.gz of one directory.

  scripts/archive.py <parent dir> <name> <output.tar.gz>

The archive holds <parent dir>/<name> and everything under it. Entries are in
name order; every one is owned by 0:0 with no user or group name, dated
SOURCE_DATE_EPOCH (default 0), and given mode 0755 if any execute bit was set
and 0644 otherwise. The gzip header carries no file name and no time. So the
same files give the same bytes, whatever their timestamps, owners or umask.
"""

import gzip
import os
import sys
import tarfile
from pathlib import Path


def normalized(info: tarfile.TarInfo, mtime: int) -> tarfile.TarInfo:
    info.uid = info.gid = 0
    info.uname = info.gname = ""
    info.mtime = mtime
    executable = info.isdir() or info.mode & 0o111
    info.mode = 0o755 if executable else 0o644
    return info


def main() -> None:
    if len(sys.argv) != 4:
        sys.exit(__doc__.strip())
    parent, name, output = Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3])
    mtime = int(os.environ.get("SOURCE_DATE_EPOCH", "0"))
    root = parent / name
    paths = [root, *sorted(root.rglob("*"), key=lambda p: p.relative_to(root).as_posix())]
    with open(output, "wb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0, compresslevel=9) as zipped:
            with tarfile.open(fileobj=zipped, mode="w", format=tarfile.PAX_FORMAT) as tar:
                for path in paths:
                    arcname = path.relative_to(parent).as_posix()
                    info = normalized(tar.gettarinfo(str(path), arcname), mtime)
                    if info.isfile():
                        with open(path, "rb") as f:
                            tar.addfile(info, f)
                    else:
                        tar.addfile(info)


if __name__ == "__main__":
    main()
