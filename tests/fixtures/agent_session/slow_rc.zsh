# The zsh twin of slow_rc.sh. zsh reads .zshrc with job control on, so this
# `sleep` takes the foreground and gives it back before the first prompt.
sleep 2
# A login zsh rebuilds PATH from /etc/profile; the harness's stubs go first.
PATH="$STUB_BIN:$PATH"
PS1='$ '
