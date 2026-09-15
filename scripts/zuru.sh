z() {
    local cwd_file status destination zuru_executable
    cwd_file="$(mktemp "${TMPDIR:-/tmp}/zuru-cwd.XXXXXX")" || return 1
    if command -v zuru >/dev/null 2>&1; then
        zuru_executable="zuru"
    elif [ -x "./target/release/zuru" ]; then
        zuru_executable="./target/release/zuru"
    else
        printf '%s\n' 'zuru: not found in PATH and ./target/release/zuru has not been built' >&2
        rm -f -- "$cwd_file"
        return 127
    fi
    command "$zuru_executable" "$@" --cwd-file "$cwd_file"
    status=$?
    if [ "$status" -eq 0 ] && [ -s "$cwd_file" ]; then
        IFS= read -r destination < "$cwd_file"
        if [ -n "$destination" ] && [ -d "$destination" ]; then
            cd -- "$destination" || status=$?
        fi
    fi
    rm -f -- "$cwd_file"
    return "$status"
}
