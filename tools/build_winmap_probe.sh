#!/bin/sh
# Build actual Windows PE32/PE32+ probes with installed Clang/LLD and Wine's
# development headers/import libraries. No CRT, MinGW download, or system install.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
wine_root=${1:?Usage: sh tools/build_winmap_probe.sh /path/to/wine-development-tree}
wine_root=$(CDPATH= cd -- "$wine_root" && pwd)
clang=${CLANG:-clang}
linker=${LLD_LINK:-lld-link}
out="$root/target/winmap-probe"
mkdir -p "$out"

build() {
    triple=$1
    arch=$2
    machine=$3
    bits=$4
    libs="$wine_root/lib/wine/$arch-windows"
    test -f "$libs/libkernel32.a"
    test -f "$libs/libshell32.a"
    "$clang" --target="$triple" -std=c11 -O2 -Wall -Wextra -Werror \
        -ffreestanding -fno-builtin -fno-stack-protector -mno-stack-arg-probe \
        -DWINVER=0x0601 -D_WIN32_WINNT=0x0601 \
        -isystem "$wine_root/include/wine/windows" \
        -isystem "$wine_root/include/wine/msvcrt" \
        -c "$root/tools/winmap_probe.c" -o "$out/probe$bits.obj"
    # Wine's GNU-style x86 import stubs do not carry SafeSEH tables. This
    # freestanding diagnostic has no SEH handlers; do not reuse as DLL flags.
    set --
    if [ "$bits" = 32 ]; then set -- /safeseh:no; fi
    "$linker" /entry:probe_entry /subsystem:console /nodefaultlib "$@" \
        "/machine:$machine" "/out:$out/winmap-probe$bits.exe" \
        "$out/probe$bits.obj" "$libs/libkernel32.a" "$libs/libshell32.a"
    printf 'Built %s\n' "$out/winmap-probe$bits.exe"
}

build x86_64-pc-windows-msvc x86_64 x64 64
build i686-pc-windows-msvc i386 x86 32
