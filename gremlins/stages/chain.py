"""Chain stage for in-process child pipeline execution."""

from __future__ import annotations

import argparse
import json
import pathlib
from dataclasses import dataclass
from typing import Any, cast

from gremlins import handoff
from gremlins.clients import ClientSpec, collect_stage_specs, to_client
from gremlins.clients.resolve import validate_stage_specs
from gremlins.pipeline import StageEntry, load_pipeline, resolve_pipeline_path
from gremlins.prompts import BUNDLED_PROMPT_DIR, load_prompts
from gremlins.stages.base import Stage
from gremlins.stages.registry import register_stage
from gremlins.state import (
    clear_parallel_bail,
    emit_bail,
    patch_state,
    read_parallel_bail,
)


@dataclass(frozen=True)
class HandoffResult:
    exit_state: str
    rolling_plan_path: pathlib.Path
    signal_path: pathlib.Path
    child_plan_path: pathlib.Path | None
    reason: str


class Chain(Stage):
    def __init__(self, entry: StageEntry, model: str | None) -> None:
        super().__init__(entry, model)
        child = self.options.get("child")
        handoff_prompt = self.options.get("handoff_prompt")
        if not isinstance(child, str) or not child:
            raise ValueError(f"stage {self.name!r}: options.child must be a string")
        if not isinstance(handoff_prompt, str) or not handoff_prompt:
            raise ValueError(
                f"stage {self.name!r}: options.handoff_prompt must be a string"
            )
        self.child_pipeline_name = child
        self.handoff_prompt_path = pathlib.Path(handoff_prompt)
        self._bail_child_key = f"{self.name}-child"

    def run(self, pipe: Any) -> None:
        history = self._load_handoff_history(pipe.gr_id)
        current_child_stage = self._load_current_child_stage(pipe.gr_id)

        if current_child_stage:
            self._run_child_pipeline(
                pipe,
                history=history,
                resume_from=current_child_stage,
            )
            patch_state(
                pipe.gr_id,
                _delete=("current_child_stage",),
                handoff_history=history,
            )

        while True:
            handoff_result = self._run_handoff(pipe, history)
            history.append(
                {
                    "rolling_plan": str(handoff_result.rolling_plan_path),
                    "signal_file": str(handoff_result.signal_path),
                    "exit_state": handoff_result.exit_state,
                    "child_plan": (
                        str(handoff_result.child_plan_path)
                        if handoff_result.child_plan_path is not None
                        else ""
                    ),
                    "reason": handoff_result.reason,
                }
            )
            patch_state(
                pipe.gr_id,
                _delete=("current_child_stage",),
                handoff_history=history,
            )

            if handoff_result.exit_state == "chain-done":
                return
            if handoff_result.exit_state == "bail":
                emit_bail(
                    pipe.gr_id,
                    "other",
                    handoff_result.reason or "handoff requested a bail",
                )
                raise RuntimeError(handoff_result.reason or "handoff requested a bail")
            if handoff_result.child_plan_path is None:
                raise RuntimeError("handoff returned next-plan without a child plan")

            self._run_child_pipeline(pipe, history=history)
            patch_state(
                pipe.gr_id,
                _delete=("current_child_stage",),
                handoff_history=history,
            )

    def _run_handoff(
        self,
        pipe: Any,
        history: list[dict[str, str]],
    ) -> HandoffResult:
        current_plan_path = self._current_plan_path(history)
        rolling_plan_path = self._next_rolling_plan_path(len(history) + 1)
        child_plan_path = rolling_plan_path.with_name(
            f"{rolling_plan_path.stem}-child{rolling_plan_path.suffix}"
        )
        signal_path = rolling_plan_path.with_suffix(".state.json")

        plan_text = current_plan_path.read_text(encoding="utf-8")
        original_plan = self._load_original_plan(pipe.gr_id)
        branch, git_log, git_diff = handoff.collect_git_context(
            self._load_chain_base_ref(pipe.gr_id)
        )
        prompt = self._render_handoff_prompt(
            plan_text=plan_text,
            branch=branch,
            git_log=git_log,
            git_diff=git_diff,
            rolling_plan_path=rolling_plan_path,
            child_plan_path=child_plan_path,
            signal_path=signal_path,
            original_plan=original_plan,
        )

        handoff.with_reap_after(
            self.state.client,
            None,
            lambda: self.run_claude(
                prompt,
                label="handoff",
                raw_path=self.state.session_dir / f"stream-{self.name}-handoff.jsonl",
            ),
        )

        if not signal_path.exists():
            raise RuntimeError(f"handoff did not write {signal_path}")
        signal = cast(
            dict[str, Any], json.loads(signal_path.read_text(encoding="utf-8"))
        )
        exit_state = str(signal.get("exit_state") or "")
        if exit_state not in ("next-plan", "chain-done", "bail"):
            raise RuntimeError(f"handoff returned invalid exit_state {exit_state!r}")

        next_child_plan: pathlib.Path | None = None
        raw_child_plan = signal.get("child_plan")
        if isinstance(raw_child_plan, str) and raw_child_plan:
            next_child_plan = pathlib.Path(raw_child_plan)
        reason = str(signal.get("reason") or "")

        handoff.sanitize_rolling_plan(
            self.state.client,
            rolling_plan_path,
            pipe.stage_specs[self.name],
            timeout=60,
        )

        return HandoffResult(
            exit_state=exit_state,
            rolling_plan_path=rolling_plan_path,
            signal_path=signal_path,
            child_plan_path=next_child_plan,
            reason=reason,
        )

    def _run_child_pipeline(
        self,
        pipe: Any,
        *,
        history: list[dict[str, str]],
        resume_from: str | None = None,
    ) -> None:
        from gremlins.orchestrators.pipeline import LocalPipeline

        child_plan_path = self._current_child_plan_path(history)
        child_dir = self._child_session_dir(history)
        child_dir.mkdir(parents=True, exist_ok=True)
        self._write_child_spec(child_dir, pipe.gr_id)

        child_pipeline = load_pipeline(
            resolve_pipeline_path(self.child_pipeline_name, self.state.cwd)
        )
        child_stage_specs = collect_stage_specs(child_pipeline, None)
        validate_stage_specs(child_stage_specs, child_pipeline)

        if pipe.test_client is None:
            for spec in child_stage_specs.values():
                pipe.spec_clients.setdefault(str(spec), to_client(spec))

        child_args = argparse.Namespace(
            resume_from=resume_from,
            plan_path=str(child_plan_path),
            spec_path=None,
            cmds=None,
            test_max_attempts=3,
            instructions=[],
        )
        child_pipe = LocalPipeline(
            child_pipeline.stages,
            args=child_args,
            session_dir=child_dir,
            gr_id=pipe.gr_id,
            pipeline_data=child_pipeline,
            stage_specs=child_stage_specs,
            spec_clients=pipe.spec_clients,
            test_client=pipe.test_client,
            stage_recorder=lambda stage_name: patch_state(
                pipe.gr_id, current_child_stage=stage_name
            ),
            child_key=self._bail_child_key,
            parallel_aggregate_child_key=self._bail_child_key,
        )

        clear_parallel_bail(pipe.gr_id, self._bail_child_key)
        try:
            child_pipe.run(*self._child_signal_clients(pipe, child_stage_specs))
        except Exception as exc:
            shard = read_parallel_bail(pipe.gr_id, self._bail_child_key)
            clear_parallel_bail(pipe.gr_id, self._bail_child_key)
            bail_class = str(shard.get("bail_class") or "other")
            bail_detail = str(shard.get("bail_detail") or str(exc))
            emit_bail(pipe.gr_id, bail_class, bail_detail)
            patch_state(
                pipe.gr_id,
                bail_source="child",
                child_bail_class=bail_class,
                child_bail_detail=bail_detail,
            )
            raise RuntimeError(bail_detail) from exc
        clear_parallel_bail(pipe.gr_id, self._bail_child_key)

    def _child_signal_clients(
        self,
        pipe: Any,
        child_stage_specs: dict[str, ClientSpec],
    ) -> list[Any]:
        if pipe.test_client is not None:
            return [pipe.test_client]
        return list(
            {
                id(pipe.spec_clients[str(spec)]): pipe.spec_clients[str(spec)]
                for spec in child_stage_specs.values()
            }.values()
        )

    def _render_handoff_prompt(
        self,
        *,
        plan_text: str,
        branch: str,
        git_log: str,
        git_diff: str,
        rolling_plan_path: pathlib.Path,
        child_plan_path: pathlib.Path,
        signal_path: pathlib.Path,
        original_plan: str,
    ) -> str:
        prompt = load_prompts([self.handoff_prompt_path])
        diff_body = git_diff[:50000] if git_diff else "(empty — no changes yet)"
        diff_trunc = (
            f"\n(diff truncated to 50000 chars; {len(git_diff)} chars total)"
            if len(git_diff) > 50000
            else ""
        )
        log_body = git_log if git_log else "(no commits yet — branch just started)"
        spec_section = ""
        if original_plan:
            spec_body = original_plan[:50000]
            spec_trunc = (
                f"\n(spec truncated to 50000 chars; {len(original_plan)} chars total)"
                if len(original_plan) > 50000
                else ""
            )
            spec_section = (
                "## Overarching goal (north star)\n\n"
                "This is the original chain spec. It does not change between handoffs "
                "and is read-only context for understanding what the chain as a whole "
                "is working toward. Use it to judge whether the rolling input plan "
                "below is on track and to scope the next step coherently. Do not echo "
                "it into the updated plan.\n\n"
                f"~~~~\n{spec_body}\n~~~~{spec_trunc}\n\n"
            )
        style_section = (
            "## Coding style\n\n"
            "Respect these principles when writing child plans. Avoid proposing "
            "architectures that violate them:\n\n"
            f"{load_prompts([BUNDLED_PROMPT_DIR / 'code_style.md'])}\n\n"
        )
        return prompt.format(
            spec_section=spec_section,
            style_section=style_section,
            plan_text=plan_text,
            branch=branch,
            log_body=log_body,
            diff_body=diff_body,
            diff_trunc=diff_trunc,
            out_path=rolling_plan_path,
            child_plan_path=child_plan_path,
            signal_path=signal_path,
        )

    def _load_handoff_history(self, gr_id: str | None) -> list[dict[str, str]]:
        if gr_id is None:
            return []
        from gremlins.state import resolve_state_file

        state_file = resolve_state_file(gr_id)
        if state_file is None or not state_file.exists():
            return []
        data = cast(dict[str, Any], json.loads(state_file.read_text(encoding="utf-8")))
        raw_history = data.get("handoff_history")
        if not isinstance(raw_history, list):
            return []
        return [
            {
                "rolling_plan": str(item.get("rolling_plan") or ""),
                "signal_file": str(item.get("signal_file") or ""),
                "exit_state": str(item.get("exit_state") or ""),
                "child_plan": str(item.get("child_plan") or ""),
                "reason": str(item.get("reason") or ""),
            }
            for item in cast(list[dict[str, object]], raw_history)
        ]

    def _load_current_child_stage(self, gr_id: str | None) -> str:
        if gr_id is None:
            return ""
        from gremlins.state import resolve_state_file

        state_file = resolve_state_file(gr_id)
        if state_file is None or not state_file.exists():
            return ""
        data = cast(dict[str, Any], json.loads(state_file.read_text(encoding="utf-8")))
        return str(data.get("current_child_stage") or "")

    def _load_original_plan(self, gr_id: str | None) -> str:
        if gr_id is None:
            return ""
        from gremlins.state import resolve_state_file

        state_file = resolve_state_file(gr_id)
        if state_file is None or not state_file.exists():
            return ""
        data = cast(dict[str, Any], json.loads(state_file.read_text(encoding="utf-8")))
        return str(data.get("original_plan") or "")

    def _load_chain_base_ref(self, gr_id: str | None) -> str:
        if gr_id is None:
            return "HEAD"
        from gremlins.state import resolve_state_file

        state_file = resolve_state_file(gr_id)
        if state_file is None or not state_file.exists():
            return "HEAD"
        data = cast(dict[str, Any], json.loads(state_file.read_text(encoding="utf-8")))
        return str(data.get("chain_base_ref") or "HEAD")

    def _current_plan_path(self, history: list[dict[str, str]]) -> pathlib.Path:
        if history:
            return pathlib.Path(history[-1]["rolling_plan"])
        return self.state.session_dir / "plan.md"

    def _current_child_plan_path(self, history: list[dict[str, str]]) -> pathlib.Path:
        if not history or not history[-1].get("child_plan"):
            raise RuntimeError("chain cannot resume child without a child plan")
        return pathlib.Path(history[-1]["child_plan"])

    def _next_rolling_plan_path(self, handoff_number: int) -> pathlib.Path:
        return self.state.session_dir / f"handoff-{handoff_number:03d}.md"

    def _child_session_dir(self, history: list[dict[str, str]]) -> pathlib.Path:
        child_index = sum(
            1 for item in history if item.get("exit_state") == "next-plan"
        )
        return self.state.session_dir / self.name / f"child-{child_index:03d}"

    def _write_child_spec(self, child_dir: pathlib.Path, gr_id: str | None) -> None:
        original_plan = self._load_original_plan(gr_id)
        if not original_plan:
            return
        spec_file = child_dir / "spec.md"
        if spec_file.exists():
            return
        spec_file.write_text(original_plan, encoding="utf-8")


register_stage("chain", Chain)
