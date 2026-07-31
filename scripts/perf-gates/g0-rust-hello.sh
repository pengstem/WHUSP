#!/musl/busybox ash

set -u

RUN_ID='@RUN_ID@'
ARCH='@ARCH@'
KIND='@KIND@'
SAMPLE='@SAMPLE@'
SMP='@SMP@'
MEM='@MEM@'
BLOCK_IO='@BLOCK_IO_MODE@'
PERF='@PERF_COUNTERS@'
WORKLOAD='@WORKLOAD@'
MULTICRATE_LEAF_CRATES='@MULTICRATE_LEAF_CRATES@'
MULTICRATE_FUNCTIONS_PER_CRATE='@MULTICRATE_FUNCTIONS_PER_CRATE@'

BB=/musl/busybox
TIMER=/x1/rust_build_timer
CARGO=/root/.cargo/bin/cargo
RUSTC=/root/.cargo/bin/rustc
PROJECT=/tmp/minibuild
TIMER_RESULT=/tmp/rust-build-timer.result
PROGRAM_OUTPUT=/tmp/minibuild.stdout
PROGRAM_STDERR=/tmp/minibuild.stderr
PERF_PATH=/proc/oskernel/perf
PERF_BEFORE=/tmp/g0-rust-hello-perf.before
PERF_AFTER=/tmp/g0-rust-hello-perf.after
PERF_SNAPSHOT_MAX_BYTES=262144
PERF_SNAPSHOT_MAX_LINES=4096

PATH=/root/.cargo/bin:/usr/local/bin:/usr/bin:/bin:/sbin:/usr/sbin
HOME=/root
RUSTUP_HOME=/root/.rustup
CARGO_HOME=/root/.cargo
RUSTUP_TOOLCHAIN=nightly-2026-05-28
CARGO_NET_OFFLINE=true
CARGO_TERM_COLOR=never
LANG=C
LC_ALL=C
TMPDIR=/tmp
CARGO_TARGET_DIR=/tmp/minibuild/target

unset LD_LIBRARY_PATH LD_PRELOAD CARGO_BUILD_JOBS
unset RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER RUSTFLAGS
unset CARGO_ENCODED_RUSTFLAGS CARGO_INCREMENTAL
unset SCCACHE_DIR SCCACHE_CACHE_SIZE CC REALGCC MAKEFLAGS
unset CDPATH ENV BASH_ENV RUSTDOCFLAGS

export PATH HOME RUSTUP_HOME CARGO_HOME RUSTUP_TOOLCHAIN
export CARGO_NET_OFFLINE CARGO_TERM_COLOR LANG LC_ALL TMPDIR
export CARGO_TARGET_DIR

umask 077

fail()
{
    FAIL_STAGE=$1
    FAIL_REASON=$2
    FAIL_RC=${3:--1}
    printf '%s\n' \
        "G0_RUST_HELLO_FAIL run_id=$RUN_ID arch=$ARCH kind=$KIND sample=$SAMPLE smp=$SMP mem=$MEM block_io=$BLOCK_IO perf=$PERF workload=$WORKLOAD stage=$FAIL_STAGE reason=$FAIL_REASON rc=$FAIL_RC"
    exit 1
}

is_marker_token()
{
    case "$1" in
        ''|*[!A-Za-z0-9._-]*) return 1 ;;
        *) return 0 ;;
    esac
}

is_uint()
{
    case "$1" in
        ''|*[!0-9]*) return 1 ;;
        *) return 0 ;;
    esac
}

is_uptime_value()
{
    case "$1" in
        *.*)
            UPTIME_WHOLE=${1%.*}
            UPTIME_FRACTION=${1#*.}
            is_uint "$UPTIME_WHOLE" && is_uint "$UPTIME_FRACTION"
            ;;
        *) return 1 ;;
    esac
}

path_exists()
{
    [ -e "$1" ] || [ -L "$1" ]
}

capture_perf_snapshot()
{
    PERF_POINT=$1
    PERF_DEST=$2
    [ "$PERF" -eq 1 ] || fail perf capture_disabled
    path_exists "$PERF_DEST" && fail perf stale_snapshot
    $BB cat "$PERF_PATH" > "$PERF_DEST"
    PERF_CAPTURE_RC=$?
    [ "$PERF_CAPTURE_RC" -eq 0 ] || fail perf "${PERF_POINT}_snapshot_read_failed" "$PERF_CAPTURE_RC"
    [ -f "$PERF_DEST" ] || fail perf "${PERF_POINT}_snapshot_missing"
}

validate_perf_snapshot()
{
    PERF_POINT=$1
    PERF_DEST=$2
    [ "$PERF" -eq 1 ] || fail perf validate_disabled
    [ -f "$PERF_DEST" ] || fail perf "${PERF_POINT}_snapshot_missing"
    PERF_CAPTURE_BYTES=$($BB wc -c < "$PERF_DEST") \
        || fail perf "${PERF_POINT}_snapshot_size_failed" "$?"
    PERF_CAPTURE_LINES=$($BB wc -l < "$PERF_DEST") \
        || fail perf "${PERF_POINT}_snapshot_lines_failed" "$?"
    is_uint "$PERF_CAPTURE_BYTES" || fail perf "${PERF_POINT}_snapshot_size_invalid"
    is_uint "$PERF_CAPTURE_LINES" || fail perf "${PERF_POINT}_snapshot_lines_invalid"
    [ "$PERF_CAPTURE_BYTES" -gt 0 ] || fail perf "${PERF_POINT}_snapshot_empty"
    [ "$PERF_CAPTURE_BYTES" -le "$PERF_SNAPSHOT_MAX_BYTES" ] \
        || fail perf "${PERF_POINT}_snapshot_too_large"
    [ "$PERF_CAPTURE_LINES" -gt 0 ] || fail perf "${PERF_POINT}_snapshot_no_lines"
    [ "$PERF_CAPTURE_LINES" -le "$PERF_SNAPSHOT_MAX_LINES" ] \
        || fail perf "${PERF_POINT}_snapshot_too_many_lines"
}

emit_perf_snapshot()
{
    PERF_POINT=$1
    PERF_SOURCE=$2
    [ "$PERF" -eq 1 ] || fail perf emit_disabled
    printf '%s\n' \
        "G0_RUST_HELLO_PERF_BEGIN run_id=$RUN_ID arch=$ARCH kind=$KIND sample=$SAMPLE smp=$SMP mem=$MEM block_io=$BLOCK_IO perf=$PERF workload=$WORKLOAD point=$PERF_POINT"
    $BB cat "$PERF_SOURCE" || fail perf "${PERF_POINT}_snapshot_emit_failed" "$?"
    printf '%s\n' \
        "G0_RUST_HELLO_PERF_END run_id=$RUN_ID arch=$ARCH kind=$KIND sample=$SAMPLE smp=$SMP mem=$MEM block_io=$BLOCK_IO perf=$PERF workload=$WORKLOAD point=$PERF_POINT"
}

prepare_multicrate_monitor()
{
    CRATES_DIR="$PROJECT/crates"
    RUSTC_ACTIVE_DIR=/tmp/g0-rustc-active
    RUSTC_ACTIVE_SAMPLES=/tmp/g0-rustc-active.samples
    RUSTC_WRAPPER_PATH=/tmp/g0-rustc-wrapper.sh
    $BB mkdir "$RUSTC_ACTIVE_DIR" || fail setup rustc_active_mkdir_failed "$?"
    $BB cat > "$RUSTC_WRAPPER_PATH" <<'EOF'
#!/musl/busybox ash
BB=/musl/busybox
ACTIVE_DIR=/tmp/g0-rustc-active
SAMPLES=/tmp/g0-rustc-active.samples
REAL_RUSTC=$1
shift
TOKEN="$ACTIVE_DIR/$$"
: > "$TOKEN" || exit 121
ACTIVE=$($BB find "$ACTIVE_DIR" -type f | $BB wc -l) || exit 122
printf '%s\n' "$ACTIVE" >> "$SAMPLES" || exit 123
"$REAL_RUSTC" "$@"
RC=$?
$BB rm -f "$TOKEN"
exit "$RC"
EOF
    $BB chmod 700 "$RUSTC_WRAPPER_PATH" || fail setup rustc_wrapper_chmod_failed "$?"
    RUSTC_WRAPPER="$RUSTC_WRAPPER_PATH"
    export RUSTC_WRAPPER
}

is_marker_token "$RUN_ID" || fail identity invalid_run_id
is_marker_token "$ARCH" || fail identity invalid_arch
is_marker_token "$KIND" || fail identity invalid_kind
is_marker_token "$SAMPLE" || fail identity invalid_sample
is_marker_token "$SMP" || fail identity invalid_smp
is_marker_token "$MEM" || fail identity invalid_mem
is_marker_token "$BLOCK_IO" || fail identity invalid_block_io
is_marker_token "$PERF" || fail identity invalid_perf
is_marker_token "$WORKLOAD" || fail identity invalid_workload

case "$ARCH" in
    rv) EXPECTED_MACHINE=riscv64 ;;
    la) EXPECTED_MACHINE=loongarch64 ;;
    *) fail identity unsupported_arch ;;
esac

case "$KIND" in
    warmup|measured) ;;
    *) fail identity unsupported_kind ;;
esac

case "$BLOCK_IO" in
    auto|force-sync) ;;
    *) fail identity unsupported_block_io ;;
esac

case "$PERF" in
    0|1) ;;
    *) fail identity unsupported_perf ;;
esac

case "$WORKLOAD" in
    hello|multicrate) ;;
    *) fail identity unsupported_workload ;;
esac

is_uint "$MULTICRATE_LEAF_CRATES" || fail identity multicrate_leaf_crates_not_uint
is_uint "$MULTICRATE_FUNCTIONS_PER_CRATE" \
    || fail identity multicrate_functions_not_uint
[ "$MULTICRATE_LEAF_CRATES" -gt 0 ] || fail identity multicrate_leaf_crates_zero
[ "$MULTICRATE_FUNCTIONS_PER_CRATE" -gt 0 ] \
    || fail identity multicrate_functions_zero

is_uint "$SAMPLE" || fail identity sample_not_uint
is_uint "$SMP" || fail identity smp_not_uint
[ "$SMP" -ge 1 ] || fail identity smp_not_positive

printf '%s\n' \
    "G0_RUST_HELLO_START run_id=$RUN_ID arch=$ARCH kind=$KIND sample=$SAMPLE smp=$SMP mem=$MEM block_io=$BLOCK_IO perf=$PERF workload=$WORKLOAD"

[ -x "$BB" ] || fail preflight busybox_missing
[ -x "$TIMER" ] || fail preflight timer_missing
[ -x "$CARGO" ] || fail preflight cargo_missing
[ -x "$RUSTC" ] || fail preflight rustc_missing
[ -r /proc/mounts ] || fail preflight proc_mounts_missing
[ -r /proc/uptime ] || fail preflight proc_uptime_missing
if [ "$PERF" -eq 1 ]; then
    [ -r "$PERF_PATH" ] || fail preflight perf_snapshot_missing
fi

MACHINE=$($BB uname -m) || fail identity uname_failed "$?"
[ "$MACHINE" = "$EXPECTED_MACHINE" ] || fail identity uname_mismatch

ONLINE_CPUS=$($BB nproc) || fail identity nproc_failed "$?"
is_uint "$ONLINE_CPUS" || fail identity nproc_not_uint
[ "$ONLINE_CPUS" -eq "$SMP" ] || fail identity nproc_mismatch

TMP_MOUNT_COUNT=$($BB awk '$2 == "/tmp" { count += 1 } END { print count + 0 }' /proc/mounts) \
    || fail scratch mount_scan_failed "$?"
ROOT_MOUNT_COUNT=$($BB awk '$2 == "/" { count += 1 } END { print count + 0 }' /proc/mounts) \
    || fail scratch root_mount_scan_failed "$?"
[ "$TMP_MOUNT_COUNT" -eq 1 ] || fail scratch tmp_not_distinct_mount
[ "$ROOT_MOUNT_COUNT" -eq 1 ] || fail scratch root_mount_invalid

WRITE_PROBE="/tmp/.g0b-write-probe-$RUN_ID-$ARCH-$KIND-$SAMPLE"
path_exists "$WRITE_PROBE" && fail scratch stale_write_probe
$BB mkdir "$WRITE_PROBE" || fail scratch tmp_not_writable "$?"
$BB rmdir "$WRITE_PROBE" || fail scratch probe_cleanup_failed "$?"

if [ "$WORKLOAD" = hello ]; then
    path_exists "$PROJECT" && fail setup stale_project
else
    [ -d "$PROJECT" ] || fail setup multicrate_project_missing
fi
path_exists "$TIMER_RESULT" && fail setup stale_timer_result
path_exists "$PROGRAM_OUTPUT" && fail setup stale_program_output
path_exists "$PROGRAM_STDERR" && fail setup stale_program_stderr
if [ "$PERF" -eq 1 ]; then
    path_exists "$PERF_BEFORE" && fail setup stale_perf_before
    path_exists "$PERF_AFTER" && fail setup stale_perf_after
fi

RUSTC_VERSION_LINE=$($RUSTC --version 2>/dev/null)
RUSTC_VERSION_RC=$?
[ "$RUSTC_VERSION_RC" -eq 0 ] || fail metadata rustc_version_failed "$RUSTC_VERSION_RC"
RUSTC_VERSION=$(printf '%s\n' "$RUSTC_VERSION_LINE" | $BB awk 'NR == 1 { print $2; exit }') \
    || fail metadata rustc_version_parse_failed "$?"
is_marker_token "$RUSTC_VERSION" || fail metadata rustc_version_invalid

CARGO_VERSION_LINE=$($CARGO --version 2>/dev/null)
CARGO_VERSION_RC=$?
[ "$CARGO_VERSION_RC" -eq 0 ] || fail metadata cargo_version_failed "$CARGO_VERSION_RC"
CARGO_VERSION=$(printf '%s\n' "$CARGO_VERSION_LINE" | $BB awk 'NR == 1 { print $2; exit }') \
    || fail metadata cargo_version_parse_failed "$?"
is_marker_token "$CARGO_VERSION" || fail metadata cargo_version_invalid

if [ "$WORKLOAD" = hello ]; then
    $CARGO new --vcs none "$PROJECT" >/dev/null 2>&1
    CARGO_NEW_RC=$?
    [ "$CARGO_NEW_RC" -eq 0 ] || fail setup cargo_new_failed "$CARGO_NEW_RC"

    [ -d "$PROJECT" ] || fail setup project_missing
    [ -f "$PROJECT/Cargo.toml" ] || fail setup manifest_missing
    [ -f "$PROJECT/src/main.rs" ] || fail setup source_missing

    EXPECTED_MANIFEST='[package]
name = "minibuild"
version = "0.1.0"
edition = "2024"

[dependencies]'
    EXPECTED_SOURCE='fn main() {
    println!("Hello, world!");
}'

    ACTUAL_MANIFEST=$($BB cat "$PROJECT/Cargo.toml") \
        || fail setup manifest_read_failed "$?"
    ACTUAL_SOURCE=$($BB cat "$PROJECT/src/main.rs") \
        || fail setup source_read_failed "$?"
    MANIFEST_BYTES=$($BB wc -c < "$PROJECT/Cargo.toml") \
        || fail setup manifest_size_failed "$?"
    SOURCE_BYTES=$($BB wc -c < "$PROJECT/src/main.rs") \
        || fail setup source_size_failed "$?"

    [ "$ACTUAL_MANIFEST" = "$EXPECTED_MANIFEST" ] \
        || fail setup manifest_content_mismatch
    [ "$ACTUAL_SOURCE" = "$EXPECTED_SOURCE" ] || fail setup source_content_mismatch
    [ "$MANIFEST_BYTES" -eq 80 ] || fail setup manifest_size_mismatch
    [ "$SOURCE_BYTES" -eq 45 ] || fail setup source_size_mismatch

    PROJECT_ENTRIES=$($BB find "$PROJECT" -mindepth 1 -maxdepth 2 -print | $BB sort) \
        || fail setup project_inventory_failed "$?"
    EXPECTED_ENTRIES="$PROJECT/Cargo.toml
$PROJECT/src
$PROJECT/src/main.rs"
    [ "$PROJECT_ENTRIES" = "$EXPECTED_ENTRIES" ] \
        || fail setup unexpected_project_entry
else
    [ -f "$PROJECT/Cargo.toml" ] || fail setup multicrate_manifest_missing
    [ -f "$PROJECT/src/main.rs" ] || fail setup multicrate_source_missing
    [ -d "$PROJECT/crates" ] || fail setup multicrate_crates_missing
    LEAF_MANIFESTS=$($BB find "$PROJECT/crates" -name Cargo.toml | $BB wc -l) \
        || fail setup multicrate_manifest_inventory_failed "$?"
    LEAF_SOURCES=$($BB find "$PROJECT/crates" -name lib.rs | $BB wc -l) \
        || fail setup multicrate_source_inventory_failed "$?"
    [ "$LEAF_MANIFESTS" -eq "$MULTICRATE_LEAF_CRATES" ] \
        || fail setup multicrate_manifest_count_mismatch
    [ "$LEAF_SOURCES" -eq "$MULTICRATE_LEAF_CRATES" ] \
        || fail setup multicrate_source_count_mismatch
fi

path_exists "$PROJECT/target" && fail prebuild target_present
path_exists "$PROJECT/Cargo.lock" && fail prebuild lock_present
path_exists "$PROJECT/.git" && fail prebuild git_present
[ "$CARGO_TARGET_DIR" = "$PROJECT/target" ] || fail prebuild target_dir_mismatch
[ "${RUSTC_WRAPPER+x}" != x ] || fail prebuild rustc_wrapper_present
[ "${RUSTC_WORKSPACE_WRAPPER+x}" != x ] || fail prebuild workspace_wrapper_present
[ "${RUSTFLAGS+x}" != x ] || fail prebuild rustflags_present
[ "${CARGO_ENCODED_RUSTFLAGS+x}" != x ] || fail prebuild encoded_rustflags_present
[ "${CARGO_BUILD_JOBS+x}" != x ] || fail prebuild cargo_jobs_present

LEAF_CRATES=0
FUNCTIONS_PER_CRATE=0
BUILD_JOBS=1
RUSTC_MAX_ACTIVE=1
if [ "$WORKLOAD" = multicrate ]; then
    prepare_multicrate_monitor
    LEAF_CRATES=$MULTICRATE_LEAF_CRATES
    FUNCTIONS_PER_CRATE=$MULTICRATE_FUNCTIONS_PER_CRATE
    BUILD_JOBS=$SMP
fi

cd "$PROJECT" || fail prebuild chdir_failed "$?"

UPTIME_IDLE_BEFORE=
UPTIME_EXTRA=
IFS=' ' read -r UPTIME_BEFORE UPTIME_IDLE_BEFORE UPTIME_EXTRA < /proc/uptime \
    || fail timer uptime_before_failed "$?"
[ -z "$UPTIME_EXTRA" ] || fail timer uptime_before_extra_field
is_uptime_value "$UPTIME_BEFORE" || fail timer uptime_before_invalid
is_uptime_value "$UPTIME_IDLE_BEFORE" || fail timer uptime_idle_before_invalid

if [ "$PERF" -eq 1 ]; then
    capture_perf_snapshot before "$PERF_BEFORE"
fi

printf '%s\n' \
    "G0_RUST_BUILD_BEGIN workload=$WORKLOAD leaf_crates=$LEAF_CRATES build_jobs=$BUILD_JOBS"
if [ "$WORKLOAD" = multicrate ]; then
    $TIMER "$TIMER_RESULT" "$CARGO" build --jobs "$BUILD_JOBS"
else
    $TIMER "$TIMER_RESULT" "$CARGO" build >/dev/null 2>&1
fi
TIMER_RC=$?
printf '%s\n' \
    "G0_RUST_BUILD_END workload=$WORKLOAD leaf_crates=$LEAF_CRATES build_jobs=$BUILD_JOBS rc=$TIMER_RC"

if [ "$WORKLOAD" = multicrate ]; then
    [ -f "$RUSTC_ACTIVE_SAMPLES" ] || fail validate rustc_samples_missing
    RUSTC_MAX_ACTIVE=$($BB sort -n "$RUSTC_ACTIVE_SAMPLES" | $BB tail -n 1) \
        || fail validate rustc_max_parse_failed "$?"
    is_uint "$RUSTC_MAX_ACTIVE" || fail validate rustc_max_not_uint
    MIN_RUSTC_ACTIVE=1
    if [ "$SMP" -ge 2 ]; then
        MIN_RUSTC_ACTIVE=2
    fi
    [ "$RUSTC_MAX_ACTIVE" -ge "$MIN_RUSTC_ACTIVE" ] \
        || fail validate rustc_not_parallel
    [ "$RUSTC_MAX_ACTIVE" -le "$BUILD_JOBS" ] || fail validate rustc_exceeds_jobs
    RUSTC_ACTIVE_REMAINDER=$($BB find "$RUSTC_ACTIVE_DIR" -type f | $BB wc -l) \
        || fail validate rustc_active_scan_failed "$?"
    [ "$RUSTC_ACTIVE_REMAINDER" -eq 0 ] || fail validate rustc_active_leak
fi

if [ "$PERF" -eq 1 ]; then
    capture_perf_snapshot after "$PERF_AFTER"
fi

UPTIME_IDLE_AFTER=
UPTIME_EXTRA=
IFS=' ' read -r UPTIME_AFTER UPTIME_IDLE_AFTER UPTIME_EXTRA < /proc/uptime \
    || fail timer uptime_after_failed "$?"
[ -z "$UPTIME_EXTRA" ] || fail timer uptime_after_extra_field
is_uptime_value "$UPTIME_AFTER" || fail timer uptime_after_invalid
is_uptime_value "$UPTIME_IDLE_AFTER" || fail timer uptime_idle_after_invalid

if [ "$PERF" -eq 1 ]; then
    validate_perf_snapshot before "$PERF_BEFORE"
    validate_perf_snapshot after "$PERF_AFTER"
fi

[ "$TIMER_RC" -eq 0 ] || fail build cargo_or_timer_failed "$TIMER_RC"
[ -f "$TIMER_RESULT" ] || fail timer result_missing

TIMER_LINE_COUNT=$($BB wc -l < "$TIMER_RESULT") \
    || fail timer result_line_count_failed "$?"
[ "$TIMER_LINE_COUNT" -eq 1 ] || fail timer result_line_count_invalid

TIMER_TEXT=$($BB cat "$TIMER_RESULT") \
    || fail timer result_read_failed "$?"
IFS=' ' read -r ELAPSED_FIELD EXITED_FIELD EXIT_CODE_FIELD \
    SIGNALED_FIELD SIGNAL_FIELD EXTRA_FIELD < "$TIMER_RESULT" \
    || fail timer result_parse_failed "$?"
[ -z "$EXTRA_FIELD" ] || fail timer result_extra_field
EXPECTED_TIMER_TEXT="$ELAPSED_FIELD $EXITED_FIELD $EXIT_CODE_FIELD $SIGNALED_FIELD $SIGNAL_FIELD"
[ "$TIMER_TEXT" = "$EXPECTED_TIMER_TEXT" ] || fail timer result_format_invalid

case "$ELAPSED_FIELD" in
    elapsed_ns=*) ELAPSED_NS=${ELAPSED_FIELD#elapsed_ns=} ;;
    *) fail timer elapsed_field_invalid ;;
esac
is_uint "$ELAPSED_NS" || fail timer elapsed_not_uint
[ "$ELAPSED_NS" -gt 0 ] || fail timer elapsed_zero
[ "$EXITED_FIELD" = exited=1 ] || fail timer exited_field_invalid
[ "$EXIT_CODE_FIELD" = exit_code=0 ] || fail timer exit_code_field_invalid
[ "$SIGNALED_FIELD" = signaled=0 ] || fail timer signaled_field_invalid
[ "$SIGNAL_FIELD" = signal=0 ] || fail timer signal_field_invalid

[ -f "$PROJECT/Cargo.lock" ] || fail validate lock_missing
ARTIFACT="$CARGO_TARGET_DIR/debug/minibuild"
[ -f "$ARTIFACT" ] || fail validate artifact_missing
[ -x "$ARTIFACT" ] || fail validate artifact_not_executable
ARTIFACT_BYTES=$($BB wc -c < "$ARTIFACT") \
    || fail validate artifact_size_failed "$?"
is_uint "$ARTIFACT_BYTES" || fail validate artifact_size_not_uint
[ "$ARTIFACT_BYTES" -gt 0 ] || fail validate artifact_empty

path_exists "$PROGRAM_OUTPUT" && fail validate stale_program_output
path_exists "$PROGRAM_STDERR" && fail validate stale_program_stderr
"$ARTIFACT" > "$PROGRAM_OUTPUT" 2> "$PROGRAM_STDERR"
PROGRAM_RC=$?
[ "$PROGRAM_RC" -eq 0 ] || fail validate artifact_run_failed "$PROGRAM_RC"
[ -f "$PROGRAM_OUTPUT" ] || fail validate output_missing
[ -f "$PROGRAM_STDERR" ] || fail validate stderr_missing

OUTPUT_TEXT=$($BB cat "$PROGRAM_OUTPUT") \
    || fail validate output_read_failed "$?"
OUTPUT_BYTES=$($BB wc -c < "$PROGRAM_OUTPUT") \
    || fail validate output_size_failed "$?"
STDERR_BYTES=$($BB wc -c < "$PROGRAM_STDERR") \
    || fail validate stderr_size_failed "$?"
[ "$OUTPUT_TEXT" = 'Hello, world!' ] || fail validate output_content_mismatch
[ "$OUTPUT_BYTES" -eq 14 ] || fail validate output_size_mismatch
[ "$STDERR_BYTES" -eq 0 ] || fail validate unexpected_stderr

if [ "$PERF" -eq 1 ]; then
    emit_perf_snapshot before "$PERF_BEFORE"
    emit_perf_snapshot after "$PERF_AFTER"
fi

printf '%s\n' \
    "G0_RUST_HELLO_RESULT run_id=$RUN_ID arch=$ARCH kind=$KIND sample=$SAMPLE smp=$SMP mem=$MEM block_io=$BLOCK_IO perf=$PERF workload=$WORKLOAD uname=$MACHINE nproc=$ONLINE_CPUS cargo_version=$CARGO_VERSION rustc_version=$RUSTC_VERSION tmp_mount=1 tmp_writable=1 elapsed_ns=$ELAPSED_NS timer_exited=1 timer_exit_code=0 timer_signaled=0 timer_signal=0 timer_rc=$TIMER_RC uptime_before=$UPTIME_BEFORE uptime_after=$UPTIME_AFTER lock_created=1 leaf_crates=$LEAF_CRATES functions_per_crate=$FUNCTIONS_PER_CRATE build_jobs=$BUILD_JOBS rustc_max_active=$RUSTC_MAX_ACTIVE artifact_bytes=$ARTIFACT_BYTES output_bytes=$OUTPUT_BYTES output_ok=1 ok=1"
printf '%s\n' \
    "G0_RUST_HELLO_PASS run_id=$RUN_ID arch=$ARCH kind=$KIND sample=$SAMPLE smp=$SMP mem=$MEM block_io=$BLOCK_IO perf=$PERF workload=$WORKLOAD"

exit 0
