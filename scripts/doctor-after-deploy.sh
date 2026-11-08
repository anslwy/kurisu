# The swap and health check succeeded. Diagnostics must never invoke rollback.
doctor_after_deploy() {
    printf '==> doctor assertion after deployment\n'
    if KURISU_HOME="$HOME_DIR" "$LIVE_BIN" doctor; then
        printf '==> doctor assertion passed\n'
    else
        printf 'ERROR: doctor assertion FAIL after deployment; inspect the checks above. The healthy deployment remains installed; no rollback.\n' >&2
    fi
    return 0
}
