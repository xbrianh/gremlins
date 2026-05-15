from __future__ import annotations

import subprocess
import time

from gremlins.utils.decorators import swallow


@swallow(Exception)
def terminate_quietly(p: subprocess.Popen[bytes]) -> None:
    p.terminate()


@swallow(Exception)
def wait_quietly(p: subprocess.Popen[bytes], timeout: float) -> None:
    p.wait(timeout=timeout)


@swallow(Exception)
def kill_quietly(p: subprocess.Popen[bytes]) -> None:
    p.kill()


def reap_processes(procs: list[subprocess.Popen[bytes]]) -> None:
    for p in procs:
        terminate_quietly(p)
    deadline = time.time() + 2.0
    for p in procs:
        wait_quietly(p, max(0.0, deadline - time.time()))
    for p in procs:
        if p.poll() is None:
            kill_quietly(p)
