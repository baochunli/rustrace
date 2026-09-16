#!/bin/sh
# Install the latest stable source release. Safe to pipe into POSIX sh.
set -eu

cargo_alternative='cargo +1.98.1 install --git https://github.com/baochunli/rustrace --tag vX.Y.Z rustrace --locked'
fail() { printf '%s\n' "$*" >&2; exit 1; }
case $(uname -s) in
    Linux) build_hint='Install native build tools (build-essential on Debian/Ubuntu).';;
    Darwin) build_hint='Install native build tools (Xcode Command Line Tools on macOS).';;
    *) fail "Unsupported OS. Use a supported Linux/macOS system and: $cargo_alternative (replace vX.Y.Z with the latest release tag).";;
esac
command -v curl >/dev/null 2>&1 || fail 'The Rustrace installer requires curl.'
if command -v rustup >/dev/null 2>&1; then
    rustup=rustup
elif [ -x "$HOME/.cargo/bin/rustup" ]; then
    rustup=$HOME/.cargo/bin/rustup
else
    printf '%s\n' 'Install rustup first:' "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" \
        'Course guide: https://github.com/baochunli/rustrace/blob/main/docs/student-guide.md#install' >&2
    exit 1
fi
if ! RUSTUP_AUTO_INSTALL=0 "$rustup" run 1.98.1 rustc --version >/dev/null 2>&1; then
    printf '%s\n' 'Rust 1.98.1 is absent; installing the minimal toolchain with clippy and rustfmt...'
    RUSTUP_AUTO_INSTALL=0 "$rustup" toolchain install 1.98.1 --profile minimal --component clippy,rustfmt || \
        fail 'Toolchain installation failed. Run: rustup toolchain install 1.98.1 --profile minimal --component clippy,rustfmt'
fi
if command -v cargo >/dev/null 2>&1; then
    cargo=cargo
elif [ -x "$HOME/.cargo/bin/cargo" ]; then
    cargo=$HOME/.cargo/bin/cargo
else
    fail "Cargo is missing. Check your rustup installation. Alternative: $cargo_alternative"
fi

repository=${RUSTRACE_SOURCE_REPOSITORY:-https://github.com/baochunli/rustrace}
manifest_url=${RUSTRACE_MANIFEST_URL:-https://github.com/baochunli/rustrace/releases/latest/download/latest.json}
work=$(mktemp -d)
receipt_temp=
trap 'rm -rf "$work"; if [ -n "$receipt_temp" ]; then rm -f "$receipt_temp"; fi' 0
trap 'exit 1' HUP INT TERM
curl --disable -fsSL --connect-timeout 10 --max-time 30 --max-filesize 65536 "$manifest_url" -o "$work/latest.json" || \
    fail 'Could not fetch the release manifest. Check your connection and retry.'

# The fallback parses JSON with a 64-level nesting limit, tracks object paths,
# and rejects duplicate or escaped object keys. Unrelated string values may use
# escapes; identity strings must be literal, as in the canonical release manifest.
json_field() {
    RUSTRACE_EXPECTED_REPOSITORY=$repository awk -v mode="$2" '
    function bad() { invalid=1; exit 1 }
    function ws() { while (substr(s,p,1) ~ /^[ \t\r\n]$/) p++ }
    function string(    c,out,e,i) {
        if (substr(s,p++,1)!="\"") bad()
        out=""; escaped=0
        while (p<=length(s)) {
            c=substr(s,p++,1)
            if (c=="\"") return out
            if (c ~ /[[:cntrl:]]/) bad()
            if (c=="\\") {
                escaped=1; e=substr(s,p++,1)
                if (e=="u") {
                    for(i=0;i<4;i++) if(substr(s,p+i,1)!~/^[0-9a-fA-F]$/) bad()
                    out=out "\\u" substr(s,p,4); p+=4
                } else {
                    if(e!~/^["\\\/bfnrt]$/) bad()
                    out=out "\\" e
                }
            } else out=out c
        }
        bad()
    }
    function value(path,depth,    c,key,child,v,start,typ,escape) {
        if(depth>64) bad()
        ws(); c=substr(s,p,1)
        if(c=="{" || c=="[") {
            typ=c; p++; ws()
            if(substr(s,p,1)==(typ=="{"?"}":"]")) {p++; return}
            while(1) {
                if(typ=="{") {
                    key=string(); if(escaped) bad()
                    # Escape path separators so flat keys cannot forge nesting.
                    gsub(/~/,"~0",key); gsub(/\//,"~1",key)
                    child=path "/" key
                    if(seen[child]++) bad()
                    ws(); if(substr(s,p++,1)!=":") bad()
                } else child=path "/[" ++index_count "]"
                value(child,depth+1); ws(); c=substr(s,p++,1)
                if(c==(typ=="{"?"}":"]")) break
                if(c!=",") bad()
                ws()
            }
        } else {
            if(c=="\"") {v=string(); typ="string"; escape=escaped}
            else {
                start=p
                while(substr(s,p,1) ~ /^[^ \t\r\n,}\]]$/ && p<=length(s)) p++
                v=substr(s,start,p-start); typ="literal"; escape=0
                if(v!~/^(true|false|null|-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?)$/) bad()
            }
            fields[path]=v; types[path]=typ; escapes[path]=escape
        }
    }
    {s=s $0 "\n"}
    END {
        if(invalid) exit 1
        p=1; ws(); if(substr(s,p,1)!="{") bad()
        value("",0); ws(); if(p<=length(s)) bad()
        if(mode=="method") {
            if(types["/method"]!="string" || escapes["/method"]) bad()
            print fields["/method"]; exit
        }
        tag=fields["/tag"]
        if(types["/schema_version"]!="literal" || fields["/schema_version"]!="1" ||
           types["/tag"]!="string" || escapes["/tag"] || tag!~/^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$/ ||
           types["/source/repository"]!="string" || escapes["/source/repository"] ||
           fields["/source/repository"]!=ENVIRON["RUSTRACE_EXPECTED_REPOSITORY"] ||
           types["/version"]!="string" || escapes["/version"] || fields["/version"]!=substr(tag,2) ||
           types["/source/tag"]!="string" || escapes["/source/tag"] || fields["/source/tag"]!=tag) bad()
        print tag
    }' "$1"
}
if command -v python3 >/dev/null 2>&1; then
    tag=$(python3 - "$work/latest.json" "$repository" <<'PY'
import json
import re
import sys


def unique(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError('duplicate JSON key')
        result[key] = value
    return result


try:
    with open(sys.argv[1], encoding='utf-8') as stream:
        data = json.load(stream, object_pairs_hook=unique)
    tag = data['tag']
    if not (type(data['schema_version']) is int and data['schema_version'] == 1
            and isinstance(tag, str) and re.fullmatch(r'v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)', tag)
            and data['version'] == tag[1:]
            and data['source']['repository'] == sys.argv[2]
            and data['source']['tag'] == tag):
        raise ValueError('invalid release identity')
    print(tag)
except (OSError, ValueError, KeyError, TypeError, AssertionError, RecursionError):
    sys.exit('Invalid release manifest.')
PY
    ) || fail 'Invalid release manifest.'
else
    tag=$(json_field "$work/latest.json" tag) || fail 'Invalid release manifest.'
fi
version=${tag#v}
cargo_home=${CARGO_HOME:-$HOME/.cargo}
config_root=
if [ -z "${RUSTRACE_INSTALL_DIR:-}" ] && [ -z "${CARGO_INSTALL_ROOT:-}" ]; then
    # Cargo prefers the legacy filename when both global configs exist.
    config=$cargo_home/config
    if [ ! -f "$config" ]; then config=$cargo_home/config.toml; fi
    if [ -f "$config" ]; then
        config_root=$(awk '
            /^[ \t]*\[install\][ \t]*(#.*)?$/ {install=1; next}
            /^[ \t]*\[/ {install=0}
            install && /^[ \t]*root[ \t]*=/ {
                if ($0 !~ /^[ \t]*root[ \t]*=[ \t]*"[^"\\[:cntrl:]]+"[ \t]*(#.*)?$/ || found++) {
                    invalid=1; next
                }
                root=$0
                sub(/^[ \t]*root[ \t]*=[ \t]*"/, "", root)
                sub(/"[ \t]*(#.*)?$/, "", root)
            }
            END {if(invalid) exit 1; if(found) print root}
        ' "$config") || fail 'Unsupported install.root in Cargo config. Set CARGO_INSTALL_ROOT explicitly.'
        case $config_root in
            ""|/*) ;;
            *) config_root=$(CDPATH='' cd -- "$cargo_home/.." && pwd)/$config_root;;
        esac
    fi
fi
install_root=${RUSTRACE_INSTALL_DIR:-${CARGO_INSTALL_ROOT:-${config_root:-$cargo_home}}}
# Keep configured symlink paths in validation, receipts and PATH entries.
mkdir -p "$install_root/bin"
install_root=$(CDPATH='' cd -- "$install_root" && pwd)
binary=$install_root/bin/rustrace
state_dir=${XDG_STATE_HOME:-$HOME/.local/state}/rustrace
if [ -e "$binary" ]; then
    method=$(json_field "$state_dir/install.json" method 2>/dev/null) || method=
    if [ "$method" != cargo-git ]; then
        printf 'replacing an existing Rustrace at %s\n' "$binary"
    fi
fi
printf 'Building Rustrace %s from source with Rust 1.98.1 (this takes a few minutes)...\n' "$version"
set -- +1.98.1 install --git "$repository" --tag "$tag" rustrace --locked
if [ -n "${RUSTRACE_INSTALL_DIR:-}" ]; then
    set -- "$@" --root "$install_root"
fi
if RUSTUP_AUTO_INSTALL=0 "$cargo" "$@"; then :; else
    status=$?
    printf '%s\n' "$build_hint" >&2
    exit "$status"
fi
"$binary" --version --verbose > "$work/version.txt" || fail 'Installed binary validation failed.'
awk -v version="$version" '
    NR==1 {if($0!="rustrace " version) exit 1; found=1}
    /^assignment format: [0-9]+$/ {assignment=1}
    /^package format: [0-9]+$/ {package=1}
    END {if(!found || !assignment || !package) exit 1}
' "$work/version.txt" || fail 'Installed binary version or format metadata differs from the release.'

# Encode strings without depending on Python; values are data, never shell code.
json_quote() {
    LC_ALL=C awk 'BEGIN {printf "\""}
        {if(NR>1) printf "\\n"; for(i=1;i<=length($0);i++) {
            c=substr($0,i,1)
            if(c=="\\" || c=="\"") printf "\\%s",c
            else if(c=="\t") printf "\\t"
            else if(c=="\r") printf "\\r"
            else if(c ~ /[[:cntrl:]]/) {
                for(n=1;n<32;n++) if(c==sprintf("%c",n)) printf "\\u%04x",n
            } else printf "%s",c
        }} END {printf "\""}'
}
mkdir -p "$state_dir"
receipt_temp=$(mktemp "$state_dir/.install.json.XXXXXX")
{
    printf '{"schema_version":1,"method":"cargo-git","path":'
    printf '%s\n' "$binary" | json_quote
    printf ',"tag":"%s","version":"%s","repository":' "$tag" "$version"
    printf '%s\n' "$repository" | json_quote
    printf '}\n'
} > "$receipt_temp"
mv -f "$receipt_temp" "$state_dir/install.json"
receipt_temp=

# Single-quote shell literals, including paths containing apostrophes.
quoted_bin=$(printf '%s\n' "$install_root/bin" | sed "s/'/'\\\\''/g")
path_export="export PATH='$quoted_bin':\$PATH"
add_path() {
    if ! grep -F -x "$path_export" "$1" >/dev/null 2>&1; then
        if grep -F -x '# >>> rustrace installer >>>' "$1" >/dev/null 2>&1; then
            printf 'Adding %s to PATH in %s for the changed install root.\n' "$install_root/bin" "$1"
        fi
        {
            printf '\n%s\n' '# >>> rustrace installer >>>'
            printf "case :\$PATH: in *:'%s':*) ;; *)\n" "$quoted_bin"
            printf '%s\n' "$path_export" ';; esac'
            printf '%s\n' '# <<< rustrace installer <<<'
        } >> "$1"
    fi
}
case :${PATH:-}: in
    *:"$install_root/bin":*) printf 'Installed rustrace %s at %s.\n' "$version" "$binary";;
    *)
        shell=${SHELL:-}
        case ${shell##*/} in
            zsh) add_path "$HOME/.zshrc";;
            bash) add_path "$HOME/.bashrc"; if [ -f "$HOME/.bash_profile" ]; then add_path "$HOME/.bash_profile"; fi;;
            fish) printf "Add Cargo's binary directory in fish: fish_add_path '%s'\n" "$quoted_bin";;
            *) printf "Add Cargo's binary directory to your shell startup file: export PATH='%s':\$PATH\n" "$quoted_bin";;
        esac
        printf 'Installed rustrace %s at %s.\n' "$version" "$binary"
        printf '%s\n' 'Open a new terminal, then run: rustrace --version'
        ;;
esac
