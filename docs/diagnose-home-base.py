#!/usr/bin/env python3
"""Install final Home path diagnostics in an isolated base worktree only."""
import pathlib
import subprocess
import sys

root = pathlib.Path(__file__).resolve().parents[1]
work = pathlib.Path(sys.argv[1]).resolve()
BASE = '9809155f8'
entry = 'crates/gremlins-cli/src/tui/entry_tests.rs'
conv = 'crates/gremlins-cli/src/tui/app/conversations.rs'


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
start = final.index('#[tokio::test]\nasync fn home_two_turn_completion_')
end = final.index('\n#[tokio::test]', start + 1)
test = final[start:end]
test = test.replace('home_two_turn_completion_routes_fresh_prompts_and_refuses_stale_corrections', 'diagnostic_home_two_turn')
test = test.replace('        let mut f = Fixture::with_socket();', '''        if std::env::var("HOME_DIAGNOSTIC_MODE").is_ok_and(|v| v != mode) { continue; }
        eprintln!("MODE {mode}");
        let mut active = None;
        let mut f = Fixture::with_socket();''')
test = test.replace('f.submit(&tx)', 'f.submit(&tx, active.is_some())')
test = test.replace('f.send_prompt(&mut generation)', 'f.diagnostic_send_prompt(&mut generation, &mut active)')
test = test.replace('f.home_event(', 'f.home_event(&mut active, ')
test = test.replace('                    &mut f.live,\n', '                    &mut f.live,\n                    &mut active,\n')
test = test.replace('f.app.conversations.home_turn()', 'active.as_deref()')
test = test.replace('        assert!(\n            active.as_deref().is_none(),', '''        eprintln!("AFTER TERMINAL selected={:?} active={:?} display_turn={} active_request={} {}", f.app.selected_target, active, f.live.target_has_current_turn(&Target::Home), f.app.active_request, f.app.conversations.diagnostic_snapshot(&Target::Home, active.is_some()));
        assert!(
            active.as_deref().is_none(),''')
test = test.replace('        assert!(!f.submit(&tx, active.is_some()), "{mode}: fresh prompt refused");', '''        eprintln!("FRESH selected={:?} active={:?} {}", f.app.selected_target, active, f.app.conversations.diagnostic_snapshot(&Target::Home, active.is_some()));
        assert!(!f.submit(&tx, active.is_some()), "{mode}: fresh prompt refused");''')
test = test.replace('        assert!(\n            active.as_deref().is_none(),', '''        if mode.starts_with("missing") && std::env::var_os("HOME_DIAGNOSTIC_AFTER_FAILURE").is_some() {
            f.app.replace_input("second prompt");
            eprintln!("DOWNSTREAM selected={:?} active={:?} {}", f.app.selected_target, active, f.app.conversations.diagnostic_snapshot(&Target::Home, active.is_some()));
            eprintln!("DOWNSTREAM submission={:?}", f.app.conversations.submit(&Target::Home, &f.app.input, active.is_some()));
            f.client.shutdown().await;
            continue;
        }
        assert!(
            active.as_deref().is_none(),''')
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
(work / entry).write_text(show('9809155f8', entry) + helper + test)
text = show('9809155f8', conv).replace('#[derive(Clone, PartialEq, Eq)]\nenum Address', '#[derive(Debug, Clone, PartialEq, Eq)]\nenum Address')
text += '''
impl Conversations {
    pub fn diagnostic_snapshot(&self, target: &Target, busy: bool) -> String {
        format!("pinned={:?} current={:?} starting={:?} latest={:?}",
            self.drafts.get(target).map(|d| &d.address), self.address(target, busy),
            self.starting_home, self.latest_home_turn)
    }
}
'''
(work / conv).write_text(text)
print('Installed test-only diagnostics in', work)
