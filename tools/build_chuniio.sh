#!/bin/sh
# Build the DLL and ABI consumer using existing Clang/LLD and Wine headers.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
wine_root=${1:?Usage: sh tools/build_chuniio.sh /path/to/wine-development-tree}
wine_root=$(CDPATH= cd -- "$wine_root" && pwd)
clang=${CLANG:-clang}
linker=${LLD_LINK:-lld-link}

build() {
    triple=$1
    machine=$2
    dll_machine=$3
    out="$root/target/chuniio/$4"
    mkdir -p "$out"
    for source in windows/chuniio tools/chuniio_probe; do
        name=${source##*/}
        "$clang" --target="$triple" -std=c11 -O2 -Wall -Wextra -Werror \
            -ffreestanding -fno-builtin -fno-stack-protector -mno-stack-arg-probe -funwind-tables \
            -DWINVER=0x0601 -D_WIN32_WINNT=0x0601 \
            -isystem "$wine_root/include/wine/windows" \
            -isystem "$wine_root/include/wine/msvcrt" \
            -c "$root/$source.c" -o "$out/$name.obj"
    done
    # Generate actual COFF imports instead of using Wine's x86 GNU import
    # stubs. This keeps SafeSEH enabled for the 32-bit DLL, without a MinGW SDK.
    python3 - "$out" "$dll_machine" <<'PY'
import pathlib
import subprocess
import sys
out = pathlib.Path(sys.argv[1])
imports = {'kernel32': set(), 'user32': set(), 'shell32': set()}
for obj in ('chuniio.obj', 'chuniio_probe.obj'):
    for line in subprocess.check_output(['llvm-nm', '--undefined-only', str(out / obj)], text=True).splitlines():
        symbol = line.split()[-1]
        if not symbol.startswith('__imp_'):
            raise SystemExit('Unexpected non-import runtime dependency: ' + symbol)
        symbol = symbol.removeprefix('__imp_').removeprefix('_')
        name = symbol.split('@')[0]
        library = 'user32' if name == 'GetAsyncKeyState' else 'shell32' if name == 'CommandLineToArgvW' else 'kernel32'
        imports[library].add(symbol)
for library, symbols in imports.items():
    definition = out / (library + '.def')
    definition.write_text('LIBRARY ' + library + '.dll\nEXPORTS\n' + ''.join('    ' + s + '\n' for s in sorted(symbols)))
    subprocess.run(['llvm-dlltool', '-m', sys.argv[2], '-k', '-d', str(definition), '-l', str(out / (library + '.lib'))], check=True)
PY
    set --
    if [ "$machine" = x86 ]; then set -- /safeseh; fi
    "$linker" /dll /noentry /nodefaultlib /dynamicbase /nxcompat /opt:ref /opt:icf \
        "$@" "/machine:$machine" "/def:$root/windows/chuniio.def" \
        "/out:$out/chuniio.dll" "/implib:$out/chuniio.lib" "$out/chuniio.obj" \
        "$out/kernel32.lib" "$out/user32.lib"
    "$linker" /entry:probe_entry /subsystem:console /nodefaultlib /dynamicbase /nxcompat \
        "$@" "/machine:$machine" "/out:$out/chuniio-probe.exe" \
        "$out/chuniio_probe.obj" "$out/kernel32.lib" "$out/shell32.lib"
    printf 'Built %s and ABI consumer\n' "$out/chuniio.dll"
}

build x86_64-pc-windows-msvc x64 i386:x86-64 x64
build i686-pc-windows-msvc x86 i386 x86
