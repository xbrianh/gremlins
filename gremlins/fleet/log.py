"""Log subcommand."""

import os
import sys

from gremlins.fleet.resolve import resolve_gremlin


def do_log(target: str, *, full: bool = False) -> bool:
    """Follow (default) or dump (--full) the gremlin's log file. Execs ``less +F``
    or ``cat`` — the wrapper just resolves the id and prints the path."""
    match = resolve_gremlin(target)
    if match is None:
        return False

    gremlin_id, _, wdir = match
    log_path = os.path.join(wdir, "log")
    if not os.path.isfile(log_path):
        sys.stderr.write(f"error: no log file for gremlin {gremlin_id} at {log_path}\n")
        return False

    # Print the path to stderr so it survives even if the operator is piping
    # the tool's stdout into another tool. Flush immediately so the header
    # isn't interleaved after the tool starts writing.
    sys.stderr.write(f"==> log: {log_path}\n")
    sys.stderr.flush()

    if full:
        try:
            os.execvp("cat", ["cat", log_path])
        except FileNotFoundError:
            sys.stderr.write("error: cat not found in PATH\n")
            return False
        except OSError as e:
            sys.stderr.write(f"error: could not exec cat: {e}\n")
            return False

    try:
        os.execvp("less", ["less", "+F", log_path])
    except FileNotFoundError:
        sys.stderr.write("error: less not found in PATH\n")
        return False
    except OSError as e:
        sys.stderr.write(f"error: could not exec less: {e}\n")
        return False
