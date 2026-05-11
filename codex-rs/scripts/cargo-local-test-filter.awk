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
    value = replace_all(value, workspace_dir "/", "./")
    value = replace_all(value, workspace_dir, ".")
    value = replace_all(value, home_dir "/", "~/")
    value = replace_all(value, home_dir, "~")

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

BEGIN {
    esc = sprintf("%c", 27)
    cr = sprintf("%c", 13)
}

{
    line = sanitize($0)
    comparable = comparable_line(line)

    if (comparable == "") {
        emit("")
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
    if (comparable ~ /^warning: `.*` .* generated [0-9]+ warnings?$/) {
        next
    }
    if (comparable ~ /^error(:|\[)/) {
        saw_error = 1
    }

    emit(line)
}

END {
    if (ok_suites > 0 && cargo_status == 0 && !saw_failed && !saw_error) {
        emit_ok_summary()
    }
}
