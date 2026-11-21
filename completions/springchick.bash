# Candidates come from the binary: `springchick complete WORDS...`.
_springchick() {
    local words
    words=$(springchick complete "${COMP_WORDS[@]:1:COMP_CWORD-1}" 2>/dev/null | cut -f1)
    COMPREPLY=($(compgen -W "$words" -- "${COMP_WORDS[COMP_CWORD]}"))
}
# -o default: filenames where there are no candidates (`record start PATH`).
complete -o default -F _springchick springchick
