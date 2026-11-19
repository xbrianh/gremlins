#!/usr/bin/env bats

setup() {
    SCRIPT="$BATS_TEST_DIRNAME/../tree_snapshot"
    TMPROOT="$(mktemp -d)"
    export TMPDIR="$TMPROOT/tmp"; mkdir -p "$TMPDIR"
    MAIN="$TMPROOT/main"
    git init -q "$MAIN"
    git -C "$MAIN" config user.email t@t; git -C "$MAIN" config user.name t
    printf 'ignored/\n*.log\n' > "$MAIN/.gitignore"
    echo base > "$MAIN/a.txt"; echo gone > "$MAIN/gone.txt"
    echo forced > "$MAIN/forced.log"
    git -C "$MAIN" add -A
    git -C "$MAIN" commit -qm init
    WT="$TMPROOT/wt"
    git -C "$MAIN" worktree add -q --detach "$WT"
    cd "$WT"
    echo staged > staged.txt; git add staged.txt
    echo dirty >> a.txt
    echo forced > forced.log; git add -f forced.log
    before="$(state)"
    BASE="$("$SCRIPT" capture)"
    [ "$(state)" = "$before" ]
}

teardown() { rm -rf "$TMPROOT"; }

# Real index bytes plus a path-aware record of every worktree file.
state() {
    cksum < "$(git rev-parse --absolute-git-dir)/index"
    if [ -n "${GIT_INDEX_FILE:-}" ] && [ -e "$GIT_INDEX_FILE" ]; then
        printf 'effective-index '; cksum < "$GIT_INDEX_FILE"
    fi
    find . -path ./.git -prune -o -type f -print0 | sort -z \
        | xargs -0 -n1 sh -c 'printf "%s " "$1"; cksum < "$1"' sh
}

# Runs the script and asserts it left index and worktree untouched.
run_pure() {
    local pre post
    pre="$(state)"
    run "$@"
    post="$(state)"
    [ "$pre" = "$post" ]
}

assert_operational() {
    [ "$status" -ge 2 ]
    [[ "$output" == *tree_snapshot:* ]]
    [[ "$output" != *"before calling Done"* ]]
}

@test "capture emits only a tree id and leaves no temp files" {
    [[ "$BASE" =~ ^[0-9a-f]{40,64}$ ]]
    [ -z "$(ls "$TMPDIR")" ]
}

@test "no edits is unchanged and read-only" {
    run_pure "$SCRIPT" check "$BASE"
    [ "$status" -eq 1 ]
    [[ "$output" == *"before calling Done"* ]]
    [ -z "$(ls "$TMPDIR")" ]
}

@test "new nonignored file passes" {
    echo n > new.txt
    run_pure "$SCRIPT" check "$BASE"
    [ "$status" -eq 0 ]
}

@test "tracked deletion passes" {
    rm gone.txt
    run_pure "$SCRIPT" check "$BASE"
    [ "$status" -eq 0 ]
}

@test "modifying a force-added ignored file passes" {
    echo changed > forced.log
    run_pure "$SCRIPT" check "$BASE"
    [ "$status" -eq 0 ]
}

@test "ignored untracked edits and restored bytes are unchanged" {
    mkdir -p ignored; echo x > ignored/f
    run_pure "$SCRIPT" check "$BASE"
    [ "$status" -eq 1 ]
    cp a.txt "$TMPROOT/a.bak"; echo more >> a.txt; mv "$TMPROOT/a.bak" a.txt
    run_pure "$SCRIPT" check "$BASE"
    [ "$status" -eq 1 ]
}

@test "agent commit still counts" {
    echo n > new.txt
    git add -A; git -c user.email=t@t -c user.name=t commit -qm c
    run_pure "$SCRIPT" check "$BASE"
    [ "$status" -eq 0 ]
}

@test "bad baseline is operational" {
    run_pure "$SCRIPT" check "deadbeef"
    [ "$status" -eq 2 ]
    [[ "$output" != *"before calling Done"* ]]
    run_pure "$SCRIPT" check ""
    [ "$status" -eq 2 ]
}

@test "snapshot failure is operational even when git exits 1" {
    mkdir "$TMPROOT/fakebin"
    printf '#!/bin/sh\ncase " $* " in *" add "*) exit 1;; esac\nexec %s "$@"\n' "$(command -v git)" > "$TMPROOT/fakebin/git"
    chmod +x "$TMPROOT/fakebin/git"
    PATH="$TMPROOT/fakebin:$PATH" run_pure "$SCRIPT" check "$BASE"
    [ "$status" -eq 2 ]
    [[ "$output" == *"could not snapshot"* ]]
    PATH="$TMPROOT/fakebin:$PATH" run_pure "$SCRIPT" capture
    [ "$status" -eq 2 ]
    [ -z "$(ls "$TMPDIR")" ]
}

@test "seeds from the worktree index, not HEAD" {
    [ -z "$(git ls-tree -r --name-only HEAD | grep '^forced.log$')" ]
    git ls-tree -r --name-only "$BASE" | grep -qx forced.log
    git ls-tree -r --name-only "$BASE" | grep -qx staged.txt
}

@test "rejects HEAD and a valid commit id" {
    for ref in HEAD "$(git rev-parse HEAD)" "$(git rev-parse HEAD^{tree})x" "${BASE:0:12}" "$BASE^{tree}"; do
        run_pure "$SCRIPT" check "$ref"
        assert_operational
    done
    [ -z "$(ls "$TMPDIR")" ]
}

@test "accepts a baseline with a trailing newline only" {
    run_pure "$SCRIPT" check "$BASE"$'\n'
    [ "$status" -eq 1 ]
    run_pure "$SCRIPT" check " $BASE"
    assert_operational
}

@test "missing baseline is operational" {
    run_pure "$SCRIPT" check
    assert_operational
}

@test "GIT_INDEX_FILE override is honored" {
    cp "$(git rev-parse --absolute-git-dir)/index" "$TMPROOT/alt-index"
    git rm -q --cached forced.log
    GIT_INDEX_FILE="$TMPROOT/alt-index" run_pure "$SCRIPT" check "$BASE"
    [ "$status" -eq 1 ]
    run_pure "$SCRIPT" check "$BASE"
    [ "$status" -eq 0 ]
}

@test "absent effective index is legitimate" {
    GIT_INDEX_FILE="$TMPROOT/absent-index" run_pure "$SCRIPT" capture
    [ "$status" -eq 0 ]
    [ ! -e "$TMPROOT/absent-index" ]
    [ -z "$(ls "$TMPDIR")" ]
}

@test "invalid or inaccessible index path is operational" {
    GIT_INDEX_FILE="$TMPROOT/no-such-dir/index" run_pure "$SCRIPT" capture
    assert_operational
    GIT_INDEX_FILE="$TMPROOT" run_pure "$SCRIPT" capture
    assert_operational
    [ -z "$(ls "$TMPDIR")" ]
}

@test "temp dir inside the checkout is not used" {
    mkdir "$WT/scratch"
    TMPDIR="$WT/scratch" run_pure "$SCRIPT" check "$BASE"
    [ "$status" -eq 1 ]
    [ -z "$(ls "$WT/scratch")" ]
    TMPDIR="$WT/scratch/../scratch" run_pure "$SCRIPT" capture
    [ "$status" -eq 0 ]
    [ "$output" = "$BASE" ]
    [ -z "$(ls "$WT/scratch")" ]
}

@test "same-size edit matching cached stat at the index mtime is detected" {
    git config core.trustctime false
    echo aaaa > racy.txt
    touch -t 202001010000.00 racy.txt
    git add racy.txt
    touch -r racy.txt "$(git rev-parse --absolute-git-dir)/index"
    echo bbbb > racy.txt
    touch -t 202001010000.00 racy.txt
    run_pure "$SCRIPT" capture
    [ "$status" -eq 0 ]
    snapshot="$output"
    pre="$(state)"
    git add -A
    [ "$snapshot" = "$(git write-tree)" ]
    [ "$(git ls-files -s racy.txt | cut -d' ' -f2)" = "$(git hash-object racy.txt)" ]
    [ -n "$pre" ]
}

@test "index-change hooks do not run: capture and check stay read-only" {
    hook="$MAIN/.git/hooks/post-index-change"
    printf '#!/bin/sh\necho hooked >> "%s/a.txt"\n' "$WT" > "$hook"
    chmod +x "$hook"
    before="$(state)"
    run_pure "$SCRIPT" capture
    [ "$status" -eq 0 ]
    [ "$(state)" = "$before" ]
    base="$output"
    run_pure "$SCRIPT" check "$base"
    [ "$status" -eq 1 ]
    [[ "$output" == *"before calling Done"* ]]
    [ "$(state)" = "$before" ]
}

@test "inherited env core.excludesFile is honored by the snapshot" {
    printf 'secret.txt\n' > "$TMPROOT/excludes"
    export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.excludesFile \
        GIT_CONFIG_VALUE_0="$TMPROOT/excludes"
    echo hidden > secret.txt
    before="$(state)"
    run_pure "$SCRIPT" capture
    [ "$status" -eq 0 ]
    base="$output"
    [ -z "$(git ls-tree -r --name-only "$base" | grep '^secret.txt$')" ]
    echo more >> secret.txt
    before="$(state)"
    run_pure "$SCRIPT" check "$base"
    [ "$status" -eq 1 ]
    [[ "$output" == *"before calling Done"* ]]
    [ "$(state)" = "$before" ]
}

# Names, sizes and checksums of every shared-index file in the worktree's Git dir.
shared_index_state() {
    local dir; dir="$(git rev-parse --absolute-git-dir)"
    (cd "$dir" && find . -maxdepth 1 -name 'sharedindex.*' -print0 | sort -z \
        | xargs -0 -n1 sh -c 'printf "%s " "$1"; cksum < "$1"' sh)
}

@test "split index: snapshot writes no shared index and matches git add -A" {
    git config core.splitIndex true
    git config splitIndex.maxPercentChange 0
    for i in $(seq 1 40); do echo "f$i" > "f$i.txt"; done
    git add -A
    git update-index --split-index
    sleep 1 # keep the edits clear of the index mtime so Git rewrites entries
    for i in $(seq 1 40); do echo "edit$i" > "f$i.txt"; done
    echo extra > extra.txt
    local pre_shared pre_state snapshot
    pre_shared="$(shared_index_state)"
    [ -n "$pre_shared" ]
    pre_state="$(state)"
    run "$SCRIPT" capture
    [ "$status" -eq 0 ]
    snapshot="$output"
    run "$SCRIPT" check "$BASE"
    [ "$status" -eq 0 ]
    [ "$(shared_index_state)" = "$pre_shared" ]
    [ "$(state)" = "$pre_state" ]
    [ -z "$(ls "$TMPDIR")" ]
    # Only now let a real root-level add mutate the index, as the reference.
    git add -A
    [ "$snapshot" = "$(git write-tree)" ]
}
