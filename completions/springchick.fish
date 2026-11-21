# Candidates come from the binary: `springchick complete WORDS...`.
complete -c springchick -f -a '(springchick complete (commandline -opc)[2..] 2>/dev/null)'
complete -c springchick -n 'set -l t (commandline -opc); test "$t[2..]" = "ipc record start"' -F
