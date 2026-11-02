/* A small game-side ABI consumer: loads the actual DLL and exercises callbacks. */
#include "../windows/chuniio.h"
#include <shellapi.h>

static WORD (__cdecl *api_version)(void);
static HRESULT (__cdecl *jvs_init)(void);
static void (__cdecl *jvs_poll)(BYTE *, BYTE *);
static void (__cdecl *read_coin)(WORD *);
static HRESULT (__cdecl *slider_init)(void);
static void (__cdecl *slider_start)(chuni_io_slider_callback_t);
static void (__cdecl *slider_stop)(void);
static void (__cdecl *slider_leds)(const BYTE *);
static HRESULT (__cdecl *led_init)(void);
static void (__cdecl *led_colors)(BYTE, const BYTE *);

static DWORD callbacks, active_callbacks, zero_callbacks, bad_callbacks, in_callback;
static DWORD verify_synthetic = 1, sample_seq, latest[8];
static DWORD latency_events, last_latency_id;
static DWORD led_frames, disable_led;
static char output[2048];
static DWORD output_len;

static void text(const char *s) {
    while (*s && output_len < sizeof(output) - 1) output[output_len++] = *s++;
}

static void number(DWORD value) {
    char digits[10];
    DWORD n = 0;
    do { digits[n++] = (char)('0' + value % 10); value /= 10; } while (value);
    while (n && output_len < sizeof(output) - 1) output[output_len++] = digits[--n];
}

static void line(void) {
    DWORD written;
    output[output_len++] = '\n';
    WriteFile(GetStdHandle(STD_OUTPUT_HANDLE), output, output_len, &written, NULL);
    output_len = 0;
}

static DWORD load(const DWORD *word) { return __atomic_load_n(word, __ATOMIC_SEQ_CST); }
static void store(DWORD *word, DWORD value) { __atomic_store_n(word, value, __ATOMIC_SEQ_CST); }
static void increment(DWORD *word) { __atomic_fetch_add(word, 1, __ATOMIC_SEQ_CST); }

/* Diagnostic only: synthetic IDs plus their complement occupy raw bytes 0..7.
 * Use a private buffer: the main thread also writes complete lines to stdout.
 * The host timestamps receipt; this measures a software-path upper bound,
 * including stdout/collector overhead, not touch-to-display latency. */
static void emit_latency_event(const BYTE *pressure) {
    DWORD id = (DWORD)pressure[30] | ((DWORD)pressure[31] << 8)
        | ((DWORD)pressure[28] << 16) | ((DWORD)pressure[29] << 24);
    DWORD inverse = (DWORD)pressure[26] | ((DWORD)pressure[27] << 8)
        | ((DWORD)pressure[24] << 16) | ((DWORD)pressure[25] << 24);
    char buffer[32] = "latency_id=", digits[10];
    DWORD length = 11, count = 0, value = id, written;
    if (!id || (id ^ inverse) != 0xffffffffu || id == last_latency_id) return;
    last_latency_id = id;
    do { digits[count++] = (char)('0' + value % 10); value /= 10; } while (value);
    while (count) buffer[length++] = digits[--count];
    buffer[length++] = '\n';
    WriteFile(GetStdHandle(STD_OUTPUT_HANDLE), buffer, length, &written, NULL);
}

static void __cdecl on_slider(const BYTE *pressure) {
    DWORD i, nonzero = 0, seq;
    if (__atomic_exchange_n(&in_callback, 1, __ATOMIC_SEQ_CST)) increment(&bad_callbacks);
    for (i = 0; i < 32; ++i) nonzero |= pressure[i];
    increment(&callbacks);
    if (nonzero) {
        increment(&active_callbacks);
        if (verify_synthetic) {
            DWORD first = pressure[30];
            for (i = 0; i < 32; ++i) {
                DWORD dest = (i & 1) ? 32 - i : 30 - i;
                if (pressure[dest] != (BYTE)(first + i)) { increment(&bad_callbacks); break; }
            }
        }
    } else increment(&zero_callbacks);
    seq = load(&sample_seq);
    store(&sample_seq, seq + 1);
    for (i = 0; i < 8; ++i) {
        const BYTE *p = pressure + i * 4;
        store(latest + i, (DWORD)p[0] | ((DWORD)p[1] << 8)
              | ((DWORD)p[2] << 16) | ((DWORD)p[3] << 24));
    }
    store(&sample_seq, seq + 2);
    if (latency_events) emit_latency_event(pressure);
    store(&in_callback, 0);
}

/* Deterministic GBR slider ramp: raw[i] == (elapsed/100 + i) mod 256.
 * Linux must observe each triple rotated [1, 2, 0] into device BRG,
 * plus unchanged billboard colors. This is a software check of the trial,
 * not proof that actual game colors display correctly.
 * Only one probe may exercise LEDs: two processes would race the LED seqlock. */
static void led_frame(DWORD elapsed) {
    BYTE slider_bytes[96], billboard[183];
    DWORD i;
    for (i = 0; i < 96; ++i) slider_bytes[i] = (BYTE)(elapsed / 100 + i);
    for (i = 0; i < 183; ++i) billboard[i] = 0;
    billboard[150] = 0x11; billboard[151] = 0x22; billboard[152] = 0x33;
    billboard[180] = 0xAA; billboard[181] = 0xBB; billboard[182] = 0xCC;
    slider_leds(slider_bytes);
    led_colors(0, billboard);
    led_colors(1, billboard);
    ++led_frames;
}

static int equal(const WCHAR *a, const WCHAR *b) {
    while (*a && *a == *b) { ++a; ++b; }
    return *a == *b;
}

static int decimal(const WCHAR *p, DWORD *result) {
    DWORD n = 0;
    if (!*p) return 0;
    while (*p) {
        if (*p < L'0' || *p > L'9' || n > 120) return 0;
        n = n * 10 + (DWORD)(*p++ - L'0');
    }
    if (!n || n > 120) return 0;
    *result = n;
    return 1;
}

static void sample(DWORD elapsed, BYTE opbtn, BYTE beams, WORD coins) {
    DWORD words[8], attempt, i, nonzero = 0;
    for (attempt = 0; attempt < 3; ++attempt) {
        DWORD seq = load(&sample_seq);
        if (seq & 1) continue;
        for (i = 0; i < 8; ++i) words[i] = load(latest + i);
        if (seq == load(&sample_seq)) break;
    }
    if (attempt == 3) return;
    for (i = 0; i < 8; ++i) nonzero |= words[i];
    text("sample elapsed_ms="); number(elapsed);
    text(" state="); text(nonzero ? "active" : "zero");
    text(" callbacks="); number(load(&callbacks));
    text(" active_callbacks="); number(load(&active_callbacks));
    text(" zero_callbacks="); number(load(&zero_callbacks));
    text(" bad_callbacks="); number(load(&bad_callbacks));
    text(" led_frames="); number(load(&led_frames));
    text(" opbtn="); number(opbtn); text(" beams="); number(beams);
    text(" coins="); number(coins); text(" slider=[");
    for (i = 0; i < 32; ++i) {
        if (i) text(",");
        number((words[i / 4] >> (8 * (i % 4))) & 255);
    }
    text("]"); line();
}

#define RESOLVE(variable, symbol) do { \
    variable = (void *)GetProcAddress(dll, symbol); \
    if (!variable) { text("error=missing_export "); text(symbol); line(); ExitProcess(2); } \
} while (0)

__declspec(noreturn) void probe_entry(void) {
    HMODULE dll;
    int argc, i;
    WCHAR **argv = CommandLineToArgvW(GetCommandLineW(), &argc);
    DWORD duration = 12, start, last_print = 0, restarted = 0, count, code = 0;
    const WCHAR *path = NULL;
    if (!argv) ExitProcess(2);
    for (i = 1; i < argc; ++i) {
        if (equal(argv[i], L"--dll") && i + 1 < argc) path = argv[++i];
        else if (equal(argv[i], L"--duration-secs") && i + 1 < argc) {
            if (!decimal(argv[++i], &duration) || duration < 4) ExitProcess(2);
        } else if (equal(argv[i], L"--observe")) verify_synthetic = 0;
        else if (equal(argv[i], L"--no-led")) disable_led = 1;
        else if (equal(argv[i], L"--latency-events")) {
            latency_events = 1;
            verify_synthetic = 0;
        }
        else ExitProcess(2);
    }
    if (!path) ExitProcess(2);
    dll = LoadLibraryW(path);
    if (!dll) { text("error=LoadLibraryW win32_error="); number(GetLastError()); line(); ExitProcess(2); }
    RESOLVE(api_version, "chuni_io_get_api_version");
    RESOLVE(jvs_init, "chuni_io_jvs_init");
    RESOLVE(jvs_poll, "chuni_io_jvs_poll");
    RESOLVE(read_coin, "chuni_io_jvs_read_coin_counter");
    RESOLVE(slider_init, "chuni_io_slider_init");
    RESOLVE(slider_start, "chuni_io_slider_start");
    RESOLVE(slider_stop, "chuni_io_slider_stop");
    RESOLVE(slider_leds, "chuni_io_slider_set_leds");
    RESOLVE(led_init, "chuni_io_led_init");
    RESOLVE(led_colors, "chuni_io_led_set_colors");
    if (api_version() != 0x0102 || jvs_init() || slider_init() || led_init()) ExitProcess(3);
    text("dll_loaded bits="); number(sizeof(void *) * 8);
    text(" api_version="); number(api_version()); line();
    jvs_poll(NULL, NULL); read_coin(NULL);
    slider_leds(NULL); led_colors(0, NULL);
    slider_start(NULL); slider_stop(); slider_stop();
    slider_start(on_slider); slider_start(on_slider);
    start = GetTickCount();
    while (GetTickCount() - start < duration * 1000) {
        DWORD elapsed = GetTickCount() - start;
        BYTE opbtn = 0, beams = 0;
        WORD coins = 0;
        jvs_poll(&opbtn, &beams);
        read_coin(&coins);
        if (verify_synthetic && (opbtn || beams || coins)) ++code;
        if (!latency_events && !restarted && elapsed >= 2000) {
            slider_stop();
            count = load(&callbacks);
            Sleep(60);
            if (count != load(&callbacks)) { text("error=callback_after_stop"); line(); ++code; }
            slider_start(on_slider);
            restarted = 1;
            text("lifecycle_restart=1"); line();
        }
        if (elapsed - last_print >= 100) {
            if (!disable_led) led_frame(elapsed);
            sample(elapsed, opbtn, beams, coins);
            last_print = elapsed;
        }
        Sleep(5);
    }
    slider_stop();
    count = load(&callbacks);
    Sleep(60);
    if (count != load(&callbacks)) ++code;
    if (!count || load(&bad_callbacks) || (verify_synthetic && !load(&active_callbacks))) ++code;
    text("summary callbacks="); number(count);
    text(" active_callbacks="); number(load(&active_callbacks));
    text(" zero_callbacks="); number(load(&zero_callbacks));
    text(" bad_callbacks="); number(load(&bad_callbacks));
    text(" led_frames="); number(load(&led_frames));
    text(" lifecycle_restarts="); number(restarted);
    text(" exit_code="); number(code ? 3 : 0); line();
    FreeLibrary(dll);
    LocalFree(argv);
    ExitProcess(code ? 3 : 0);
}
