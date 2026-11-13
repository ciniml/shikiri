#!/usr/bin/env python3
# Copyright (c) 2026 Kenta IDA
# Distributed under the Boost Software License, Version 1.0.
# (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
# SPDX-License-Identifier: BSL-1.0

"""The apps whose install failed in a `t5log rtt` log of `make install-apps`
(doc/49 §20): t5log prints each install's request numbers (`-> #A..#B
install FILE`), the kernel answers a refused one with `ctl.failed
request_id=N cmd=install_begin ... error=E` (`store full`), or refuses
it at the end (`cmd=install_end`, `catalog full`). Prints `NAME: E` per failure
and exits 0 either way (the store keeps the app's current copy)."""

import re
import sys


def main() -> None:
    text = open(sys.argv[1], errors="replace").read()
    ranges = [(int(a), int(b), name) for a, b, name in re.findall(r"-> #(\d+)\.\.#(\d+) install \S*?([^/\s]+)\.t5a", text)]
    for rid, error in re.findall(r"ctl\.failed request_id=(\d+) cmd=install_(?:begin|end) ok=false error=([^\n]+)", text):
        rid = int(rid)
        name = next((n for a, b, n in ranges if a <= rid <= b), "?")
        print(f"{name}: {error.strip()}")


if __name__ == "__main__":
    main()
