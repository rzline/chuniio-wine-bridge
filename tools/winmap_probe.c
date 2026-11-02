/* Minimal real PE executable: Win32 file mapping, no CRT, no HID access.
 * Experimental layout mirrors src/lib.rs; synthetic verification cross-checks
 * all 32 pressure bytes and the heartbeat across Linux/Wine and x86/x64.
 * Build using tools/build_winmap_probe.sh and an existing Wine development tree.
 */
#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <stdarg.h>
#include <windef.h>
#include <winbase.h>
#include <shellapi.h>

#define REGION_SIZE 4096u
#define MAGIC 0x43504231u
#define VERSION 2u
#define SEQ 16u
#define SLIDER 24u

_Static_assert(sizeof(DWORD) == 4, "32-bit shared words required");
_Static_assert(sizeof(WCHAR) == 2, "Windows UTF-16 ABI required");
_Static_assert(__atomic_always_lock_free(4, 0), "lock-free word loads required");

typedef struct {
    DWORD session, heartbeat, online, source, aux, pressure[8];
} Frame;

static char output[2048];
static DWORD output_len;

static void text(const char *s) {
    while (*s && output_len < sizeof(output) - 1) output[output_len++] = *s++;
}

static void number(DWORD n) {
    char digits[10];
    DWORD count = 0;
    do { digits[count++] = (char)('0' + n % 10); n /= 10; } while (n);
    while (count) {
        if (output_len < sizeof(output) - 1) output[output_len++] = digits[--count];
        else break;
    }
}

static void line(void) {
    DWORD written;
    output[output_len++] = '\n';
    WriteFile(GetStdHandle(STD_OUTPUT_HANDLE), output, output_len, &written, NULL);
    output_len = 0;
}

static int equal(const WCHAR *a, const WCHAR *b) {
    while (*a && *a == *b) { ++a; ++b; }
    return *a == *b;
}

static int decimal(const WCHAR *s, DWORD *out) {
    DWORD n = 0;
    if (!*s) return 0;
    while (*s) {
        if (*s < L'0' || *s > L'9' || n > 8640000u) return 0;
        n = n * 10u + (DWORD)(*s++ - L'0');
    }
    if (n > 86400000u) return 0;
    *out = n;
    return 1;
}

static DWORD load_word(const DWORD *base, DWORD word) {
    return __atomic_load_n(base + word, __ATOMIC_SEQ_CST);
}

static int snapshot(const DWORD *base, Frame *f) {
    DWORD i, attempt;
    for (attempt = 0; attempt < 3; ++attempt) {
        DWORD before = load_word(base, SEQ);
        if (before & 1) continue;
        f->session = load_word(base, 17);
        f->heartbeat = load_word(base, 18);
        f->online = load_word(base, 19);
        f->source = load_word(base, 20);
        f->aux = load_word(base, 21);
        for (i = 0; i < 8; ++i) f->pressure[i] = load_word(base, SLIDER + i);
        if (before == load_word(base, SEQ)) return 1;
    }
    return 0;
}

static DWORD pressure(const Frame *f, DWORD i) {
    return (f->pressure[i / 4] >> ((i % 4) * 8)) & 255;
}

static int verify(const Frame *f) {
    DWORD i;
    if (f->online && f->source != 1) return 0;
    for (i = 0; i < 32; ++i) {
        DWORD expected = f->online ? (f->heartbeat + i) & 255 : 0;
        if (pressure(f, i) != expected) return 0;
    }
    return 1;
}

static void state_line(DWORD state, const Frame *f) {
    static const char *states[] = {"waiting", "live", "offline", "stale", "busy", "unavailable"};
    DWORD i;
    text("state="); text(states[state]);
    if (state < 4) {
        text(" session="); number(f->session);
        text(" heartbeat="); number(f->heartbeat);
        text(" source="); number(f->source);
    }
    text(" air="); number(state == 1 ? f->aux & 255 : 0);
    text(" buttons="); number(state == 1 ? (f->aux >> 8) & 255 : 0);
    text(" slider=[");
    for (i = 0; i < 32; ++i) {
        if (i) text(",");
        number(state == 1 ? pressure(f, i) : 0);
    }
    text("]"); line();
}

__declspec(noreturn) void probe_entry(void) {
    int argc, arg;
    WCHAR **argv = CommandLineToArgvW(GetCommandLineW(), &argc);
    const WCHAR *path = L"Z:\\dev\\shm\\chuni_bridge_probe";
    const WCHAR *stop_path = NULL; /* Diagnostic control only; never used by a game DLL. */
    DWORD duration_ms = 10000, stale_ms = 300, verify_synthetic = 1;
    DWORD snapshots = 0, live_samples = 0, busy_reads = 0, exit_code = 0;
    DWORD observed = 0, confirmed = 0, last_session = 0, last_heartbeat = 0;
    DWORD last_state = 99, last_error = 0;
    ULONGLONG start, last_change = 0, next_open = 0, last_print = 0;
    HANDLE file = INVALID_HANDLE_VALUE, mapping = NULL;
    const DWORD *base = NULL;
    Frame frame;

    if (!argv) ExitProcess(2);
    for (arg = 1; arg < argc; ++arg) {
        if (equal(argv[arg], L"--observe")) verify_synthetic = 0;
        else if (equal(argv[arg], L"--path") && arg + 1 < argc) path = argv[++arg];
        else if (equal(argv[arg], L"--stop-path") && arg + 1 < argc) stop_path = argv[++arg];
        else if (equal(argv[arg], L"--duration-secs") && arg + 1 < argc) {
            DWORD seconds;
            if (!decimal(argv[++arg], &seconds) || seconds > 86400 || !seconds) goto usage;
            duration_ms = seconds * 1000;
        } else if (equal(argv[arg], L"--stale-ms") && arg + 1 < argc) {
            if (!decimal(argv[++arg], &stale_ms) || !stale_ms) goto usage;
        } else goto usage;
    }
    text("winmap-probe bits="); number(sizeof(void *) * 8);
    text(" synthetic_verification="); number(verify_synthetic); line();
    start = GetTickCount64();
    while (GetTickCount64() - start < duration_ms) {
        ULONGLONG now = GetTickCount64();
        if (stop_path && GetFileAttributesW(stop_path) != INVALID_FILE_ATTRIBUTES) {
            text("stop_requested"); line();
            break;
        }
        if (!base && now >= next_open) {
            LARGE_INTEGER size;
            DWORD error = 0;
            next_open = now + 200;
            file = CreateFileW(path, GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                NULL, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, NULL);
            if (file == INVALID_HANDLE_VALUE) error = GetLastError();
            else if (!GetFileSizeEx(file, &size)) error = GetLastError();
            else if (size.QuadPart != REGION_SIZE) error = ERROR_BAD_LENGTH;
            else {
                mapping = CreateFileMappingW(file, NULL, PAGE_READONLY, 0, 0, NULL);
                if (!mapping) error = GetLastError();
                else {
                    base = (const DWORD *)MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, REGION_SIZE);
                    if (!base) error = GetLastError();
                    else if (load_word(base, 0) != MAGIC || load_word(base, 1) != VERSION
                             || load_word(base, 2) != REGION_SIZE) error = ERROR_INVALID_DATA;
                }
            }
            if (error) {
                if (base) { UnmapViewOfFile(base); base = NULL; }
                if (mapping) { CloseHandle(mapping); mapping = NULL; }
                if (file != INVALID_HANDLE_VALUE) { CloseHandle(file); file = INVALID_HANDLE_VALUE; }
                if (last_state != 5 || last_error != error) {
                    text("state=unavailable win32_error="); number(error);
                    text(" slider=[0;32]"); line();
                    last_state = 5; last_error = error;
                }
            } else {
                text("mapped_bytes="); number(REGION_SIZE); line();
            }
        }
        if (base) {
            DWORD state;
            if (!snapshot(base, &frame)) {
                ++busy_reads;
                state = 4;
            } else {
                ++snapshots;
                if (verify_synthetic && !verify(&frame)) {
                    text("error=synthetic_snapshot_mismatch"); line();
                    exit_code = 3; break;
                }
                if (!observed || frame.session != last_session) {
                    observed = 1; confirmed = 0; last_change = now;
                } else if (frame.heartbeat != last_heartbeat) {
                    confirmed = 1; last_change = now;
                }
                last_session = frame.session; last_heartbeat = frame.heartbeat;
                state = now - last_change >= stale_ms ? 3 : !frame.online ? 2 : !confirmed ? 0 : 1;
                if (state == 1) ++live_samples;
            }
            if (state != last_state || now - last_print >= 250) {
                state_line(state, &frame);
                last_state = state; last_print = now;
            }
        }
        Sleep(5);
    }
    text("summary snapshots="); number(snapshots);
    text(" live_samples="); number(live_samples);
    text(" busy_reads="); number(busy_reads);
    text(" elapsed_ms="); number((DWORD)(GetTickCount64() - start)); line();
    if (!live_samples && !exit_code) {
        text("error=no_heartbeat_confirmed_live_snapshot"); line();
        exit_code = 1;
    }
    if (base) UnmapViewOfFile(base);
    if (mapping) CloseHandle(mapping);
    if (file != INVALID_HANDLE_VALUE) CloseHandle(file);
    LocalFree(argv);
    ExitProcess(exit_code);
usage:
    text("usage: winmap-probe.exe [--path DOS_PATH] [--duration-secs 1..86400] [--stale-ms N] [--observe] [--stop-path DOS_PATH]"); line();
    LocalFree(argv);
    ExitProcess(2);
}
