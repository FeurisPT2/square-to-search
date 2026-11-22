# Source this script to add project-local tools and Cargo bin to PATH:
#   . ./scripts/use-tools.sh

if [ -n "${BASH_SOURCE:-}" ]; then
    _script_dir=$(CDPATH= cd -- "$(dirname -- "$BASH_SOURCE")" && pwd)
    _repo_root=$(CDPATH= cd -- "$_script_dir/.." && pwd)
elif case "$0" in *use-tools.sh) true ;; *) false ;; esac; then
    _script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
    _repo_root=$(CDPATH= cd -- "$_script_dir/.." && pwd)
elif command -v git >/dev/null 2>&1 && git rev-parse --show-toplevel >/dev/null 2>&1; then
    _repo_root=$(git rev-parse --show-toplevel)
elif [ -d "./scripts" ]; then
    _repo_root=$(pwd)
elif [ -d "../scripts" ]; then
    _repo_root=$(CDPATH= cd -- ".." && pwd)
else
    _repo_root=$(pwd)
fi

_tools_bin="$_repo_root/.tools/bin"
if [ -d "$_tools_bin" ]; then
    case ":$PATH:" in
        *":$_tools_bin:"*) ;;
        *) PATH="$_tools_bin:$PATH" ;;
    esac
fi

_cargo_bin="${CARGO_HOME:-$HOME/.cargo}/bin"
if [ -d "$_cargo_bin" ]; then
    case ":$PATH:" in
        *":$_cargo_bin:"*) ;;
        *) PATH="$_cargo_bin:$PATH" ;;
    esac
fi

export PATH
unset _script_dir _repo_root _tools_bin _cargo_bin
