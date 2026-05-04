"""Top-level entry point for the session-summary hook.

Delegates to gremlins.fleet.session_summary. Use as:
  python -m gremlins.session_summary
"""

from gremlins.fleet.session_summary import main

if __name__ == "__main__":
    import sys

    sys.exit(main(sys.argv[1:]))
