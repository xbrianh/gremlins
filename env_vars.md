 ### Read (configuration)

 ┌──────────────────────┬─────────────────────────────────────────────────────────┐
 │ Env var              │ File                                                    │
 ├──────────────────────┼─────────────────────────────────────────────────────────┤
 │ GREMLINS_LOG_LEVEL   │ gremlins/logging_setup.py:13                            │
 ├──────────────────────┼─────────────────────────────────────────────────────────┤
 │ GREMLINS_RESUME_FROM │ gremlins/executor/run.py:150                            │
 ├──────────────────────┼─────────────────────────────────────────────────────────┤
 │ BG_STALL_SECS        │ gremlins/fleet/constants.py:5                           │
 ├──────────────────────┼─────────────────────────────────────────────────────────┤
 │ GREMLIN_SKIP_SUMMARY │ gremlins/fleet/session_summary.py:41                    │
 ├──────────────────────┼─────────────────────────────────────────────────────────┤
 │ BASH_ENV             │ gremlins/env_file.py:29,71 (filtered out, not consumed) │
 └──────────────────────┴─────────────────────────────────────────────────────────┘

 ### Written (set into os.environ for child stages / subprocesses)

 ┌─────────────────────────┬───────────────────────────────────────────────────────────────┐
 │ Env var                 │ File                                                          │
 ├─────────────────────────┼───────────────────────────────────────────────────────────────┤
 │ GREMLINS_CWD_OF_CLI_CMD │ gremlins/cli/__init__.py:75                                   │
 ├─────────────────────────┼───────────────────────────────────────────────────────────────┤
 │ GREMLINS_GREMLIN_ID     │ gremlins/launcher.py:67                                       │
 ├─────────────────────────┼───────────────────────────────────────────────────────────────┤
 │ GREMLINS_OVERLAY_DIR    │ gremlins/launcher.py:68                                       │
 ├─────────────────────────┼───────────────────────────────────────────────────────────────┤
 │ GREMLINS_TELEMETRY      │ gremlins/launcher.py:70                                       │
 ├─────────────────────────┼───────────────────────────────────────────────────────────────┤
 │ PYTHONSAFEPATH          │ gremlins/launcher.py:66                                       │
 ├─────────────────────────┼───────────────────────────────────────────────────────────────┤
 │ GREMLINS_PROJECT_ROOT   │ gremlins/executor/gremlin.py:471, gremlins/fleet/land.py:1013 │
 ├─────────────────────────┼───────────────────────────────────────────────────────────────┤
 │ GREMLINS_SCRATCH_DIR    │ gremlins/executor/run.py:282                                  │
 ├─────────────────────────┼───────────────────────────────────────────────────────────────┤
 │ GREMLINS_WORKTREE_PATH  │ gremlins/executor/run.py:240                                  │
 ├─────────────────────────┼───────────────────────────────────────────────────────────────┤
 │ GREMLINS_ARTIFACT_DIR   │ gremlins/executor/run.py:243                                  │
 ├─────────────────────────┼───────────────────────────────────────────────────────────────┤
 │ GREMLIN_WORKSPACE_DIR   │ gremlins/executor/run.py:244                                  │
 ├─────────────────────────┼───────────────────────────────────────────────────────────────┤
 │ GREMLIN_STATE_DIR       │ gremlins/executor/run.py:247                                  │
 ├─────────────────────────┼───────────────────────────────────────────────────────────────┤
 │ GREMLINS_BOOTSTRAP_CWD  │ gremlins/exector/boostrap.py:54                               │
 └─────────────────────────┴───────────────────────────────────────────────────────────────┘

 ### Pass-through (entire os.environ snaphot)

 ┌─────────────────────────────────────┬──────────────────────────────────────┐
 │ File                                │ Usage                                │
 ├─────────────────────────────────────┼──────────────────────────────────────┤
 │ gremlins/launcher.py:65             │ dict(os.environ) → spawn env         │
 ├─────────────────────────────────────┼──────────────────────────────────────┤
 │ g remlins/env_file.py:27            │ dict(os.environ) → env sourcing      │
 ├─────────────────────────────────────┼──────────────────────────────────────┤
 │ g remlins/e xecutor/bootstrap.py:53 │ dict(os.environ) → bootstrap cmds    │
 ├─────────────────────────────────────┼──────────────────────────────────────┤
 │ g remlins/ executor/run.py:256,265  │ dict(os.environ) → env isolation     │
 ├─────────────────────────────────────┼──────────────────────────────────────┤
 │ gremlins/fleet/land.py:1035         │ dict(os.environ) → land env          │
 ├─────────────────────────────────────┼──────────────────────────────────────┤
 │ gremlins/stages/exec.py:164         │ **os.environ → exec stage subprocess │
 └─────────────────────────────────────┴──────────────────────────────────────┘

 ────────────────────────────────────────────────────────────────────────────────

 ./crates/ (Rust)

 ### Read (configuration)

 ┌──────────────────────────────────┬────────────────────────────────────────────────────────────────────────────┐
 │ Env var                          │ File                                                                       │
 ├──────────────────────────────────┼────────────────────────────────────────────────────────────────────────────┤
 │ GREMLINS_OVERLAY_DIR             │ crates/gremlins/src/core/discovery/mod.rs:8                                │
 ├──────────────────────────────────┼────────────────────────────────────────────────────────────────────────────┤
 │ GREMLINS_SANDBOX_ROOT            │ crates/gremlins/src/config.rs:221 (+ ~20 more uses in config.rs)           │
 ├──────────────────────────────────┼────────────────────────────────────────────────────────────────────────────┤
 │ GREMLINS_PROJECT_ROOT            │ crates/gremlins/src/config.rs:227, crates/gremlins/src/clients/tools.rs:33 │
 ├──────────────────────────────────┼────────────────────────────────────────────────────────────────────────────┤
 │ GREMLINS_OVERLAY_DIR             │ crates/gremlins/src/config.rs:233                                          │
 ├──────────────────────────────────┼────────────────────────────────────────────────────────────────────────────┤
 │ GREMLINS_STREAM_IDLE_TIMEOUT     │ crates/gremlins/src/clients/config.rs:4                                    │
 ├──────────────────────────────────┼────────────────────────────────────────────────────────────────────────────┤
 │ GREMLINS_AGENT_MAX_TURNS           │ crates/gremlins/src/clients/config.rs:24                                   │
 ├──────────────────────────────────┼────────────────────────────────────────────────────────────────────────────┤
 │ GREMLINS_OPENAI_AGENTS_MAX_TURNS │ crates/gremlins/src/clients/config.rs:28 (fallback)                         │
 ├──────────────────────────────────┼────────────────────────────────────────────────────────────────────────────┤
 │ GREMLINS_REASONING_EFFORT        │ crates/gremlins/src/clients/config.rs:36                                   │
 ├──────────────────────────────────┼────────────────────────────────────────────────────────────────────────────┤
 │ GREMLINS_TELEMETRY               │ crates/gremlins/src/clients/stream.rs:101                                  │
 ├──────────────────────────────────┼────────────────────────────────────────────────────────────────────────────┤
 │ GREMLINS_SCRATCH_DIR             │ crates/gremlins/src/clients/tools.rs:44                                    │
 ├──────────────────────────────────┼────────────────────────────────────────────────────────────────────────────┤
 │ HOME                             │ crates/gremlins/src/clients/tools.rs:191,1608                              │
 ├──────────────────────────────────┼────────────────────────────────────────────────────────────────────────────┤
 │ OPENAI_API_KEY                   │ crates/gremlins/src/clients/openai_backend.rs:47 (via api_key_env())       │
 ├──────────────────────────────────┼────────────────────────────────────────────────────────────────────────────┤
 │ XAI_API_KEY                      │ crates/gremlins/src/clients/openai_backend.rs:48 (via api_key_env())       │
 ├──────────────────────────────────┼────────────────────────────────────────────────────────────────────────────┤
 │ OPENROUTER_API_KEY               │ crates/gremlins/src/clients/openai_backend.rs:49 (via api_key_env())       │
 ├──────────────────────────────────┼────────────────────────────────────────────────────────────────────────────┤
 │ All vars (bulk collect)          │ crates/gremlins/src/clients/tools.rs:801 — std::env::vars().collect()      │
 └──────────────────────────────────┴────────────────────────────────────────────────────────────────────────────┘

 ────────────────────────────────────────────────────────────────────────────────

 ./tests/ (test-only)

 ┌──────────────────────────────────────┬──────────────────────────────────────────────────────────────┐
 │ Env var                              │ File                                                         │
 ├──────────────────────────────────────┼──────────────────────────────────────────────────────────────┤
 │ GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME │ tests/conftest.py:22                                         │
 ├──────────────────────────────────────┼──────────────────────────────────────────────────────────────┤
 │ CI                                   │ tests/conftest.py:164                                        │
 ├──────────────────────────────────────┼──────────────────────────────────────────────────────────────┤
 │ GH_TOKEN                             │ tests/conftest.py:181                                        │
 ├──────────────────────────────────────┼──────────────────────────────────────────────────────────────┤
 │ GREMLINS_SCRATCH_DIR                 │ tests/conftest.py:223,341                                    │
 ├──────────────────────────────────────┼──────────────────────────────────────────────────────────────┤
 │ GREMLINS_TESTS_DIR                   │ tests/test_state_isolation.py:56,62,66,89                    │
 ├──────────────────────────────────────┼──────────────────────────────────────────────────────────────┤
 │ GREMLINS_GREMLIN_ID                  │ tests/test_state_isolation.py:79,81 (read to assert absence) │
 ├──────────────────────────────────────┼──────────────────────────────────────────────────────────────┤
 │ PATH                                 │ tests/conftest.py:297                                        │
 ├──────────────────────────────────────┼──────────────────────────────────────────────────────────────┤
 │ PYTHONPATH                           │ tests/conftest.py:300                                        │
 └──────────────────────────────────────┴──────────────────────────────────────────────────────────────┘

 ────────────────────────────────────────────────────────────────────────────────

 ### Summary — distinct env vars

 16 distinct GREMLINS_* vars: LOG_LEVEL, RESUME_FROM, CWD_OF_CLI_CMD, GREMLIN_ID, OVERLAY_DIR, TELEMETRY, PROJECT_ROOT, SCRATCH_DIR, WORKTREE_PATH, ARTIFACT_DIR, WORKSPACE_DIR, STATE_DIR, BOOTSTRAP_CWD, SANDBOX_ROOT, STREAM_IDLE_TIMEOUT,
 OPENAI_AGENTS_MAX_TURNS, REASONING_EFFORT

 1 GREMLIN_* var: GREMLIN_SKIP_SUMMARY

 3 API-key vars: OPENAI_API_KEY, XAI_API_KEY, OPENROUTER_API_KEY

 3 non-gremlin config vars: BG_STALL_SECS, HOME, BASH_ENV (filtered)

 4 test-infra vars: GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME, CI, GH_TOKEN, PYTHONSAFEPATH

 Plus bulk pass-through of the entire parent environment in 6 locations (launcher spawn, env sourcing, bootstrap, run isolation, fleet land, exec stage).

