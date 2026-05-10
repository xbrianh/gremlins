"""Stage base types: RuntimeState dataclass and Stage base class."""

from __future__ import annotations

import argparse
import dataclasses
import json
import pathlib
import shlex
import subprocess
from collections.abc import Callable
from typing import TYPE_CHECKING, Any, NamedTuple, cast

from gremlins.clients.client import Client
from gremlins.clients.protocol import CompletedRun

if TYPE_CHECKING:
    from gremlins.git import PreImplState
    from gremlins.schema import PipelineDef, StageEntry


@dataclasses.dataclass
class RuntimeState:
    # required per-stage
    client: Client
    session_dir: pathlib.Path
    # pipeline-wide (all have defaults so tests can omit them)
    gr_id: str | None = None
    state_file: pathlib.Path | None = None
    args: argparse.Namespace = dataclasses.field(default_factory=argparse.Namespace)
    pipeline_data: PipelineDef | None = None
    repo: str = ""
    instructions: str = ""
    stage_specs: dict[str, Client] = dataclasses.field(default_factory=dict)
    spec_clients: dict[str, Client] = dataclasses.field(default_factory=dict)
    is_git: bool = False
    test_client: Client | None = None
    # per-stage optional
    child_key: str | None = None
    worktree: pathlib.Path | None = None
    current_scope: list[StageEntry] = dataclasses.field(default_factory=list)
    # runtime-derived (populated from state.json before each stage run)
    issue_url: str = ""
    base_ref_name: str = ""
    impl_materialized_branch: str = ""
    issue_num: str = ""
    impl_pre_state: PreImplState | None = None

    @property
    def cwd(self) -> pathlib.Path:
        return self.worktree if self.worktree is not None else pathlib.Path.cwd()

    def get_client(self, spec: Client) -> Client:
        if self.test_client is not None:
            return self.test_client
        return self.spec_clients[str(spec)]

    def make_runner(
        self,
        entry: StageEntry,
        spec: Client,
        scope: list[StageEntry] | None = None,
    ) -> Callable[[], None]:
        base_state = self
        gr_id = self.gr_id
        scope_list = list(scope) if scope is not None else []

        def _run() -> None:
            from gremlins.stages.registry import STAGE_BUILDERS
            from gremlins.state import resolve_state_file, set_stage

            set_stage(gr_id, entry.name)
            sf = base_state.state_file if base_state.state_file is not None else resolve_state_file(gr_id)
            sd = _read_state_json(sf)
            state = dataclasses.replace(
                base_state,
                current_scope=scope_list,
                issue_url=sd.get("issue_url") or "",
                base_ref_name=sd.get("base_ref_name") or "",
                impl_materialized_branch=sd.get("impl_materialized_branch") or "",
                issue_num=sd.get("issue_num") or "",
                impl_pre_state=_read_impl_pre_state(base_state.session_dir, sd),
            )
            builder = STAGE_BUILDERS[entry.type]
            stage = builder(entry, spec, state)
            stage.run(state)

        return _run


def _read_state_json(sf: pathlib.Path | None) -> dict[str, Any]:
    if sf is None or not sf.exists():
        return {}
    try:
        return json.loads(sf.read_text(encoding="utf-8"))
    except Exception:
        return {}


def _read_impl_pre_state(
    session_dir: pathlib.Path, sd: dict[str, Any]
) -> PreImplState | None:
    from gremlins.git import PreImplState

    sidecar = session_dir / ".impl-pre-state.json"
    if sidecar.exists():
        try:
            data: dict[str, Any] = json.loads(sidecar.read_text(encoding="utf-8"))
            head = data.get("head") or ""
            if head:
                return PreImplState(head=head, branch=data.get("branch") or "")
        except Exception:
            pass
    head = sd.get("impl_pre_head") or ""
    if head:
        return PreImplState(head=head, branch=sd.get("impl_pre_branch") or "")
    return None


class StageInput(NamedTuple):
    name: str
    type: type
    required: bool
    default: Any
    help: str


class Stage:
    def __init__(
        self, name: str, model: str | None, prompts: list[str], options: dict[str, Any]
    ) -> None:
        self.name = name
        self.model = model
        self.prompts = prompts
        self.options = options

    def run_claude(
        self,
        prompt: str,
        *,
        state: RuntimeState,
        label: str,
        raw_path: pathlib.Path | None = None,
        **kw: Any,
    ) -> CompletedRun:
        return state.client.run(
            prompt,
            label=label,
            model=self.model,
            raw_path=raw_path,
            cwd=state.worktree,
            **kw,
        )

    def bail_command(self, state: RuntimeState) -> str:
        command = ["python", "-m", "gremlins.bail"]
        if state.child_key:
            command.extend(["--child-key", state.child_key])
        return shlex.join(command)

    def run_subprocess(
        self, argv: list[str], state: RuntimeState, **kw: Any
    ) -> subprocess.CompletedProcess[Any]:
        kw.setdefault("cwd", str(state.cwd))
        return cast(subprocess.CompletedProcess[Any], subprocess.run(argv, **kw))

    @classmethod
    def orchestration_args(cls) -> list[StageInput]:
        return []

    def run(self, state: RuntimeState) -> Any:  # noqa: ARG002
        raise NotImplementedError
