# Filter Cargo/libtest output into a compact form for agent consumption.

function replace_all(value, from, to,    out, pos) {
    if (from == "") {
        return value
    }

    out = ""
    while ((pos = index(value, from)) > 0) {
        out = out substr(value, 1, pos - 1) to
        value = substr(value, pos + length(from))
    }

    return out value
}

function sanitize(value) {
    gsub(cr, "", value)
    gsub(esc "\\[[0-9;?]*[ -/]*[@-~]", "", value)
    gsub(esc "[@-Z\\\\-_]", "", value)

    value = replace_all(value, target_dir "/", "<target-dir>/")
    value = replace_all(value, target_dir, "<target-dir>")
    value = replace_all(value, tmp_dir_real "/", "<tmp-dir>/")
    value = replace_all(value, tmp_dir_real, "<tmp-dir>")
    value = replace_all(value, tmp_dir "/", "<tmp-dir>/")
    value = replace_all(value, tmp_dir, "<tmp-dir>")
    value = replace_all(value, workspace_dir "/", "./")
    value = replace_all(value, workspace_dir, ".")
    value = replace_all(value, home_dir "/", "~/")
    value = replace_all(value, home_dir, "~")

    return value
}

function strip_libtest_progress_prefix(value,    comparable) {
    comparable = comparable_line(value)

    if (comparable ~ /^[.iF]+( [0-9]+\/[0-9]+)?$/) {
        return ""
    }

    if (value ~ /^[.iF]+/) {
        comparable = value
        sub(/^[.iF]+[[:space:]]*/, "", comparable)
        if (comparable ~ /^(Snapshot test passes but|all doctests ran|test result:|failures:|running [0-9]+ tests?$|warning:|error(:|\[)|Initialized empty Git repository in |Switched to |branch .* set up to track |fatal: path )/) {
            return comparable
        }
        if (comparable ~ /^(\[[^]]+\] |To <tmp-dir>\/|\* \[new branch\]|[0-9]+ files? changed,|create mode [0-9]+ |delete mode [0-9]+ |rm ')/) {
            return comparable
        }
    }

    return value
}

function comparable_line(value,    comparable) {
    comparable = value
    sub(/^[[:space:]]+/, "", comparable)
    sub(/[[:space:]]+$/, "", comparable)
    return comparable
}

function emit(value) {
    if (value == "") {
        if (printed) {
            pending_blank = 1
        }
        return
    }

    if (pending_blank && printed) {
        print ""
    }
    print value
    printed = 1
    pending_blank = 0
}

function number_before(value, token,    parts, left, n) {
    n = split(value, parts, token)
    if (n < 2) {
        return 0
    }

    left = parts[1]
    sub(/^.*[^0-9]/, "", left)
    return left + 0
}

function seconds_from_result(value,    parts, seconds, n) {
    n = split(value, parts, "finished in ")
    if (n < 2) {
        return 0
    }

    seconds = parts[2]
    sub(/s.*/, "", seconds)
    return seconds + 0
}

function aggregate_ok_result(value) {
    ok_suites += 1
    passed += number_before(value, " passed")
    ignored += number_before(value, " ignored")
    measured += number_before(value, " measured")
    filtered += number_before(value, " filtered out")
    duration += seconds_from_result(value)
}

function emit_ok_summary(    summary) {
    summary = "cargo-local: tests ok (" passed " passed"
    if (ignored > 0) {
        summary = summary "; " ignored " ignored"
    }
    if (measured > 0) {
        summary = summary "; " measured " measured"
    }
    if (filtered > 0) {
        summary = summary "; " filtered " filtered"
    }
    if (ok_suites != 1) {
        summary = summary "; " ok_suites " suites"
    }
    summary = summary "; " sprintf("%.2fs", duration) ")"

    pending_blank = 0
    emit(summary)
}

function emit_suppressed_summary(    summary) {
    if (suppressed_legacy_snapshot_notices > 0) {
        summary = "cargo-local: suppressed " suppressed_legacy_snapshot_notices " passing legacy snapshot notice"
        if (suppressed_legacy_snapshot_notices != 1) {
            summary = summary "s"
        }
        emit(summary)
    }

    if (suppressed_passing_output_lines > 0) {
        summary = "cargo-local: suppressed " suppressed_passing_output_lines " passing-test stdout/stderr line"
        if (suppressed_passing_output_lines != 1) {
            summary = summary "s"
        }
        emit(summary)
    }
}

function is_legacy_snapshot_notice(value) {
    return value ~ /^Snapshot test passes but the existing value is in a legacy format\./
}

# Legacy snapshot notices include full snapshot bodies even when tests pass.
function is_legacy_snapshot_boundary(value) {
    if (value == "") {
        return 0
    }
    if (is_legacy_snapshot_notice(value)) {
        return 1
    }
    if (value ~ /^(failures:|test result:|running [0-9]+ tests?$|Finished `.*` profile |Running (unittests|tests|doctests|benchmarks) |Doc-tests |all doctests ran|warning:|error(:|\[)|Initialized empty Git repository in )/) {
        return 1
    }
    if (value ~ /^(\[[^]]+\] |Switched to |branch .* set up to track |To <tmp-dir>\/|\* \[new branch\]|fatal: path )/) {
        return 1
    }
    return 0
}

# Keep subprocess chatter for failed runs; on success it is usually fixture setup.
function is_passing_subprocess_noise(value) {
    if (cargo_status != 0) {
        return 0
    }
    if (value ~ /^Initialized empty Git repository in <tmp-dir>\//) {
        return 1
    }
    if (value ~ /^\[[^]]+\] /) {
        return 1
    }
    if (value ~ /^[0-9]+ files? changed,/) {
        return 1
    }
    if (value ~ /^(create|delete) mode [0-9]+ /) {
        return 1
    }
    if (value ~ /^Switched to (a new branch|branch) /) {
        return 1
    }
    if (value ~ /^branch '[^']+' set up to track /) {
        return 1
    }
    if (value ~ /^To <tmp-dir>\//) {
        return 1
    }
    if (value ~ /^\* \[new branch\] /) {
        return 1
    }
    if (value ~ /^rm '[^']+'$/) {
        return 1
    }
    if (value ~ /^fatal: path '[^']+' exists on disk, but not in '[0-9a-f]+'$/) {
        return 1
    }
    return 0
}

BEGIN {
    esc = sprintf("%c", 27)
    cr = sprintf("%c", 13)
}

{
    line = strip_libtest_progress_prefix(sanitize($0))
    comparable = comparable_line(line)

    if (skipping_legacy_snapshot_notice) {
        if (is_legacy_snapshot_notice(comparable)) {
            suppressed_legacy_snapshot_notices += 1
            next
        }
        if (!is_legacy_snapshot_boundary(comparable)) {
            next
        }
        skipping_legacy_snapshot_notice = 0
    }

    if (comparable == "") {
        emit("")
        next
    }
    if (is_legacy_snapshot_notice(comparable)) {
        suppressed_legacy_snapshot_notices += 1
        skipping_legacy_snapshot_notice = 1
        next
    }
    if (comparable == "failures:") {
        in_failure_section = 1
        emit(line)
        next
    }
    if (comparable ~ /^test result: FAILED\./) {
        in_failure_section = 0
        saw_failed = 1
        emit(line)
        next
    }
    if (in_failure_section) {
        emit(line)
        next
    }
    if (comparable ~ /^running [0-9]+ tests?$/) {
        next
    }
    if (comparable ~ /^[.iF]+( [0-9]+\/[0-9]+)?$/) {
        next
    }
    if (comparable ~ /^test result: ok\./) {
        aggregate_ok_result(comparable)
        next
    }
    if (comparable ~ /^Finished `.*` profile /) {
        next
    }
    if (comparable ~ /^Running (unittests|tests|doctests|benchmarks) /) {
        next
    }
    if (comparable ~ /^Doc-tests /) {
        next
    }
    if (comparable ~ /^all doctests ran in .*; merged doctests compilation took /) {
        next
    }
    if (comparable ~ /^warning: `.*` .* generated [0-9]+ warnings?$/) {
        next
    }
    if (is_passing_subprocess_noise(comparable)) {
        suppressed_passing_output_lines += 1
        next
    }
    if (comparable ~ /^error(:|\[)/) {
        saw_error = 1
    }

    emit(line)
}

END {
    if (!saw_failed && !saw_error) {
        emit_suppressed_summary()
    }
    if (ok_suites > 0 && cargo_status == 0 && !saw_failed && !saw_error) {
        emit_ok_summary()
    }
}
