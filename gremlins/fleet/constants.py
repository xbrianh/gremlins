"""Fleet manager constants."""

import os

BG_STALL_SECS = int(os.environ.get("BG_STALL_SECS") or 2700)

FMT = "%-15s  %-47s  %-22s  %-28s  %-5s  %-20s  %-7s  %s"
