# An rc that keeps an interactive /bin/sh busy before its first prompt, so a
# restore's typed resume line waits in the tty input queue. bash, which is
# /bin/sh on the machines this runs on, reads it without job control, so the
# `sleep` stays in the shell's own process group.
sleep 2
