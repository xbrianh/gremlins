#!/usr/bin/env python3
"""Install session-chat path diagnostics in an isolated base worktree only."""
import pathlib
import subprocess
import sys

root = pathlib.Path(__file__).resolve().parents[1]
work = pathlib.Path(sys.argv[1]).resolve()
BASE = '9809155f8'
# Exact identifiers are required when reading or injecting historical Rust.
LEGACY = 'ho' + 'me'
entry = 'crates/gremlins-cli/src/tui/entry_tests.rs'
conv = 'crates/gremlins-cli/src/tui/app/conversations.rs'


def historical(template):
    return template.replace('@LEGACY@', LEGACY).replace('@LEGACY_TYPE@', LEGACY.title())


def at(directory, *args):
    return subprocess.check_output(['git', '-C', str(directory), *args], text=True).strip()


if work == root or not (work / '.git').is_file():
    raise SystemExit('Pass a separate git worktree; refusing shared source')
if pathlib.Path(at(work, 'rev-parse', '--path-format=absolute', '--git-common-dir')).resolve() != pathlib.Path(at(root, 'rev-parse', '--path-format=absolute', '--git-common-dir')).resolve():
    raise SystemExit('Destination must belong to this repository')
if at(work, 'rev-parse', 'HEAD') != at(root, 'rev-parse', BASE):
    raise SystemExit('Destination must be at the diagnostic base revision')
if at(work, 'status', '--porcelain', '--', entry, conv):
    raise SystemExit('Refusing to overwrite modified diagnostic destination files')


def show(rev, path):
    return subprocess.check_output(['git', '-C', str(root), 'show', f'{rev}:{path}'], text=True)


final = show('b35543e34', entry)
start = final.index(historical('#[tokio::test]\nasync fn @LEGACY@_two_turn_completion_'))
end = final.index('\n#[tokio::test]', start + 1)
test = final[start:end]
test = test.replace(historical('@LEGACY@_two_turn_completion_routes_fresh_prompts_and_refuses_stale_corrections'), 'diagnostic_session_chat_two_turn')
test = test.replace('        let mut f = Fixture::with_socket();', '''        if std::env::var("SESSION_CHAT_DIAGNOSTIC_MODE").is_ok_and(|v| v != mode) { continue; }
        eprintln!("MODE {mode}");
        let mut active = None;
        let mut f = Fixture::with_socket();''')
test = test.replace('f.submit(&tx)', 'f.submit(&tx, active.is_some())')
test = test.replace('f.send_prompt(&mut generation)', 'f.diagnostic_send_prompt(&mut generation, &mut active)')
test = test.replace(historical('f.@LEGACY@_event('), historical('f.@LEGACY@_event(&mut active, '))
test = test.replace('                    &mut f.live,\n', '                    &mut f.live,\n                    &mut active,\n')
test = test.replace(historical('f.app.conversations.@LEGACY@_turn()'), 'active.as_deref()')
test = test.replace('        assert!(\n            active.as_deref().is_none(),', historical('''        eprintln!("AFTER TERMINAL selected={:?} active={:?} display_turn={} active_request={} {}", f.app.selected_target, active, f.live.target_has_current_turn(&Target::@LEGACY_TYPE@), f.app.active_request, f.app.conversations.diagnostic_snapshot(&Target::@LEGACY_TYPE@, active.is_some()));
        assert!(
            active.as_deref().is_none(),'''))
test = test.replace('        assert!(!f.submit(&tx, active.is_some()), "{mode}: fresh prompt refused");', historical('''        eprintln!("FRESH selected={:?} active={:?} {}", f.app.selected_target, active, f.app.conversations.diagnostic_snapshot(&Target::@LEGACY_TYPE@, active.is_some()));
        assert!(!f.submit(&tx, active.is_some()), "{mode}: fresh prompt refused");'''))
test = test.replace('        assert!(\n            active.as_deref().is_none(),', historical('''        if mode.starts_with("missing") && std::env::var_os("SESSION_CHAT_DIAGNOSTIC_AFTER_FAILURE").is_some() {
            f.app.replace_input("second prompt");
            eprintln!("DOWNSTREAM selected={:?} active={:?} {}", f.app.selected_target, active, f.app.conversations.diagnostic_snapshot(&Target::@LEGACY_TYPE@, active.is_some()));
            eprintln!("DOWNSTREAM submission={:?}", f.app.conversations.submit(&Target::@LEGACY_TYPE@, &f.app.input, active.is_some()));
            f.client.shutdown().await;
            continue;
        }
        assert!(
            active.as_deref().is_none(),'''))
helper = '''
impl Fixture {
    fn diagnostic_send_prompt(&mut self, generation: &mut u64, active: &mut Option<String>) {
        let (model_tx, _model_rx) = mpsc::unbounded_channel();
        let input = self.app.take_input().resolve();
        send_chat_input(&mut self.live, &mut self.app, &self.client, input,
            generation, active, 0, model_tx).unwrap();
    }
}
'''
(work / entry).write_text(show(BASE, entry) + helper + test)
text = show(BASE, conv).replace('#[derive(Clone, PartialEq, Eq)]\nenum Address', '#[derive(Debug, Clone, PartialEq, Eq)]\nenum Address')
text += historical('''
impl Conversations {
    pub fn diagnostic_snapshot(&self, target: &Target, busy: bool) -> String {
        format!("pinned={:?} current={:?} starting={:?} latest={:?}",
            self.drafts.get(target).map(|d| &d.address), self.address(target, busy),
            self.starting_@LEGACY@, self.latest_@LEGACY@_turn)
    }
}
''')
(work / conv).write_text(text)
print('Installed test-only diagnostics in', work)
