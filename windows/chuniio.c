/* Wine chuniio: bounded direct slider snapshots; cached JVS/keyboard controls.
 * Only the background worker opens files. No HID or USB access from this DLL;
 * LED data is published to the shared-memory LED region for the Linux bridge.
 */
#include "chuniio.h"
#include "input_state.h"
#include "led_state.h"
#include <winuser.h>

#define REGION_SIZE 4096u
#define MAGIC 0x43504231u
#define VERSION 2u
#define STALE_MS BRIDGE_STALE_MS
#define CACHE_STALE_MS BRIDGE_REARM_MS
#define KEYBOARD_POLL_MS 8u

_Static_assert(sizeof(DWORD) == 4, "32-bit shared words required");
_Static_assert(__atomic_always_lock_free(4, 0), "lock-free word loads required");

_Static_assert(__atomic_always_lock_free(sizeof(void *), 0), "lock-free mapping pointer required");

typedef BridgeInput Input;

typedef struct {
    DWORD seq, published_low, published_high, aux, pressure[8];
} Cache;

static INIT_ONCE initialized = INIT_ONCE_STATIC_INIT;
static SRWLOCK slider_lock = SRWLOCK_INIT;
/* Serializes LED publications between game threads calling slider_set_leds
 * and led_set_colors. The seqlock in the LED region does not tolerate two
 * simultaneous writers; a game has one process, so this lock suffices. */
static SRWLOCK led_lock = SRWLOCK_INIT;
static HANDLE slider_thread;
static DWORD slider_tid, slider_stop;
/* Published once after validation; never unmapped after publication. The worker
 * and DLL are process-lifetime pinned, so slider readers need no lifetime lock.
 * Mapped read-write: the DLL reads input and writes the LED region. */
static DWORD *bridge_mapping;
static BridgeLedCache led_cache;
static DWORD coin_total;
static Cache cache;
static WCHAR bridge_path[1024], ini_path[1024], environment_path[1024];

static DWORD load(const DWORD *word) {
    return __atomic_load_n(word, __ATOMIC_SEQ_CST);
}

static void store(DWORD *word, DWORD value) {
    __atomic_store_n(word, value, __ATOMIC_SEQ_CST);
}

static int snapshot(const DWORD *base, Input *out) {
    DWORD attempt, i;
    for (attempt = 0; attempt < 3; ++attempt) {
        DWORD seq = load(base + 16);
        if (seq & 1) continue;
        out->session = load(base + 17);
        out->heartbeat = load(base + 18);
        out->online = load(base + 19);
        out->source = load(base + 20);
        out->aux = load(base + 21);
        for (i = 0; i < 8; ++i) out->pressure[i] = load(base + 24 + i);
        if (seq == load(base + 16)) return 1;
    }
    return 0;
}

static DWORD raw_pressure(const Input *input, DWORD i) {
    return (input->pressure[i / 4] >> (8 * (i % 4))) & 255;
}

static void publish_cache(const BYTE *pressure, DWORD aux, ULONGLONG now) {
    DWORD i, seq = load(&cache.seq);
    store(&cache.seq, seq + 1);
    store(&cache.published_low, (DWORD)now);
    store(&cache.published_high, (DWORD)(now >> 32));
    store(&cache.aux, aux);
    for (i = 0; i < 8; ++i) {
        const BYTE *p = pressure + 4 * i;
        store(cache.pressure + i, (DWORD)p[0] | ((DWORD)p[1] << 8)
              | ((DWORD)p[2] << 16) | ((DWORD)p[3] << 24));
    }
    store(&cache.seq, seq + 2);
}

/* Only atomic loads and a clock read: no mapping, locks, or file operations. */
static DWORD read_cache(BYTE *pressure) {
    DWORD attempt, i, aux = 0;
    for (attempt = 0; attempt < 3; ++attempt) {
        DWORD before = load(&cache.seq);
        ULONGLONG timestamp;
        if (before & 1) continue;
        timestamp = ((ULONGLONG)load(&cache.published_high) << 32) | load(&cache.published_low);
        aux = load(&cache.aux);
        for (i = 0; i < 8; ++i) {
            DWORD word = load(cache.pressure + i), j;
            for (j = 0; j < 4; ++j) pressure[4 * i + j] = (BYTE)(word >> (8 * j));
        }
        if (before == load(&cache.seq) && GetTickCount64() - timestamp < CACHE_STALE_MS)
            return aux;
    }
    for (i = 0; i < 32; ++i) pressure[i] = 0;
    return 0;
}

static int pressed(UINT key) {
    return key && key <= 255 && (GetAsyncKeyState((int)key) & 0x8000);
}

static DWORD *open_mapping(void) {
    LARGE_INTEGER size;
    HANDLE file, mapping;
    DWORD *base = NULL;
    /* Read-write: the DLL publishes LED data into the same region. If this
     * fails (for example a read-only file), input stays unavailable too; the
     * worker retries as usual. */
    file = CreateFileW(bridge_path, GENERIC_READ | GENERIC_WRITE,
                      FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                      NULL, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, NULL);
    if (file == INVALID_HANDLE_VALUE) return NULL;
    if (GetFileSizeEx(file, &size) && size.QuadPart == REGION_SIZE) {
        mapping = CreateFileMappingW(file, NULL, PAGE_READWRITE, 0, REGION_SIZE, NULL);
        if (mapping) {
            base = (DWORD *)MapViewOfFile(mapping, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, REGION_SIZE);
            CloseHandle(mapping);
            if (base && (load(base) != MAGIC || load(base + 1) != VERSION
                         || load(base + 2) != REGION_SIZE)) {
                UnmapViewOfFile(base);
                base = NULL;
            }
        }
    }
    CloseHandle(file);
    return base;
}

static DWORD WINAPI input_worker(void *unused) {
    DWORD *base = NULL;
    BridgeLiveness liveness = {0};
    ULONGLONG next_open, next_keyboard = 0;
    DWORD keyboard_buttons = 0;
    int hardware_coin = 0, keyboard_coin = 0, coin_latch = 0;
    UINT test_key, service_key, coin_key;
    DWORD length;
    (void)unused;

    length = GetEnvironmentVariableW(L"CHUNI_BRIDGE_INI", ini_path, 1024);
    if (!length || length >= 1024) lstrcpyW(ini_path, L".\\segatools.ini");
    GetPrivateProfileStringW(L"chuniio", L"shmPath", L"Z:\\dev\\shm\\chuni_bridge_probe",
                             bridge_path, 1024, ini_path);
    length = GetEnvironmentVariableW(L"CHUNI_BRIDGE_PATH", environment_path, 1024);
    if (length >= 1024) return 0;
    if (length) lstrcpyW(bridge_path, environment_path);
    test_key = GetPrivateProfileIntW(L"io3", L"test", 0x31, ini_path);
    service_key = GetPrivateProfileIntW(L"io3", L"service", 0x32, ini_path);
    coin_key = GetPrivateProfileIntW(L"io3", L"coin", 0x33, ini_path);
    next_open = GetTickCount64();

    for (;;) {
        ULONGLONG now = GetTickCount64();
        DWORD i, buttons = 0, beams = 0;
        BYTE pressure[32] = {0};
        Input input;
        int coin;
        if (!base && now >= next_open) {
            base = open_mapping();
            if (base) __atomic_store_n(&bridge_mapping, base, __ATOMIC_RELEASE);
            next_open = now + 200;
        }
        if (base && snapshot(base, &input)) {
            if (bridge_input_live(&liveness, &input, now)) {
                DWORD air = input.aux & 0x3f;
                buttons = (input.aux >> 8) & 3;
                beams = ((air & 0x15) << 1) | ((air & 0x2a) >> 1);
                for (i = 0; i < 32; ++i)
                    pressure[(i & 1) ? 32 - i : 30 - i] = (BYTE)raw_pressure(&input, i);
                hardware_coin = !!(input.aux & 0x400);
            } else if (!input.online || now - liveness.last_change >= STALE_MS) {
                hardware_coin = 0;
            }
        } else if (!base || now - liveness.last_change >= STALE_MS) {
            hardware_coin = 0;
        }
        buttons |= keyboard_buttons;
        coin = hardware_coin || keyboard_coin;
        if (coin && !coin_latch) store(&coin_total, load(&coin_total) + 1);
        coin_latch = coin;
        publish_cache(pressure, buttons | (beams << 8), now);
        /* Operator keys do not need slider-rate scanning. Publish hardware
         * controls first, and keep Wine keyboard/server queries off the slider
         * path entirely. Keyboard changes reach the next control publication. */
        if (now >= next_keyboard) {
            keyboard_buttons = (pressed(test_key) ? 1u : 0u)
                             | (pressed(service_key) ? 2u : 0u);
            keyboard_coin = pressed(coin_key);
            next_keyboard = GetTickCount64() + KEYBOARD_POLL_MS;
        }
        Sleep(1);
    }
}

static BOOL CALLBACK initialize(PINIT_ONCE once, void *parameter, void **context) {
    HMODULE module;
    HANDLE thread;
    (void)once; (void)parameter; (void)context;
    /* The mapping worker intentionally survives slider stop/restart. Pin before
     * launching it so FreeLibrary can never unmap code under a running thread. */
    if (!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
                           (const WCHAR *)(ULONG_PTR)&initialize, &module)) return FALSE;
    thread = CreateThread(NULL, 0, input_worker, NULL, 0, NULL);
    if (!thread) return FALSE;
    CloseHandle(thread);
    return TRUE;
}

static void ensure_worker(void) {
    InitOnceExecuteOnce(&initialized, initialize, NULL, NULL);
}

static DWORD WINAPI slider_worker(void *parameter) {
    chuni_io_slider_callback_t callback = (chuni_io_slider_callback_t)parameter;
    BridgeLiveness liveness = {0};
    store(&slider_tid, GetCurrentThreadId());
    while (!load(&slider_stop)) {
        const DWORD *base = __atomic_load_n(&bridge_mapping, __ATOMIC_ACQUIRE);
        BYTE pressure[32] = {0};
        Input input;
        DWORD i;
        /* No file/mapping operations or keyboard queries on this path. Read the
         * latest publication immediately before invoking the game callback,
         * instead of waiting for a second polling thread's intermediate cache. */
        if (base && snapshot(base, &input)
                && bridge_input_live(&liveness, &input, GetTickCount64())) {
            for (i = 0; i < 32; ++i)
                pressure[(i & 1) ? 32 - i : 30 - i] = (BYTE)raw_pressure(&input, i);
        }
        callback(pressure);
        /* A bounded atomic stop check avoids per-tick Wine event/server waits.
         * External stop still joins before returning; callback stop never joins
         * itself. This is a sleeping loop, not a spin or realtime-priority loop. */
        if (!load(&slider_stop)) Sleep(1);
    }
    store(&slider_tid, 0);
    return 0;
}

WORD __cdecl chuni_io_get_api_version(void) { return 0x0102; }
HRESULT __cdecl chuni_io_jvs_init(void) { ensure_worker(); return 0; }
HRESULT __cdecl chuni_io_slider_init(void) { ensure_worker(); return 0; }

void __cdecl chuni_io_jvs_poll(BYTE *opbtn, BYTE *beams) {
    BYTE ignored[32];
    DWORD aux = read_cache(ignored);
    if (opbtn) *opbtn = (BYTE)aux;
    if (beams) *beams = (BYTE)(aux >> 8);
}

void __cdecl chuni_io_jvs_read_coin_counter(WORD *total) {
    if (total) *total = (WORD)load(&coin_total);
}

void __cdecl chuni_io_slider_start(chuni_io_slider_callback_t callback) {
    HANDLE thread;
    if (!callback || load(&slider_tid) == GetCurrentThreadId()) return;
    ensure_worker();
    AcquireSRWLockExclusive(&slider_lock);
    if (slider_thread && WaitForSingleObject(slider_thread, 0) == WAIT_OBJECT_0) {
        CloseHandle(slider_thread);
        slider_thread = NULL;
        store(&slider_tid, 0);
    }
    if (!slider_thread) {
        store(&slider_stop, 0);
        thread = CreateThread(NULL, 0, slider_worker, (void *)callback, 0, NULL);
        if (thread) slider_thread = thread;
    }
    ReleaseSRWLockExclusive(&slider_lock);
}

void __cdecl chuni_io_slider_stop(void) {
    if (load(&slider_tid) == GetCurrentThreadId()) {
        store(&slider_stop, 1);
        return;
    }
    AcquireSRWLockExclusive(&slider_lock);
    if (slider_thread) {
        store(&slider_stop, 1);
        WaitForSingleObject(slider_thread, INFINITE);
        CloseHandle(slider_thread);
        slider_thread = NULL;
        store(&slider_tid, 0);
    }
    ReleaseSRWLockExclusive(&slider_lock);
}

/* Publish LED data into the shared-memory LED region for the Linux bridge.
 * Never blocks, never touches HID: a missing mapping just drops the frame.
 * DWORD and uint32_t are both exactly 4 bytes here (static assert above). */
static void publish_led_locked(void) {
    DWORD *base = __atomic_load_n(&bridge_mapping, __ATOMIC_ACQUIRE);
    if (base)
        bridge_led_publish(&led_cache,
                           (uint32_t *)(void *)(base + BRIDGE_LED_WORD_OFFSET));
}

void __cdecl chuni_io_slider_set_leds(const BYTE *gbr) {
    if (!gbr) return;
    AcquireSRWLockExclusive(&led_lock);
    bridge_led_slider(&led_cache, gbr);
    publish_led_locked();
    ReleaseSRWLockExclusive(&led_lock);
}

HRESULT __cdecl chuni_io_led_init(void) { return 0; }

void __cdecl chuni_io_led_set_colors(BYTE board, const BYTE *rgb) {
    if (!rgb) return;
    AcquireSRWLockExclusive(&led_lock);
    if (bridge_led_billboard(&led_cache, board, rgb)) publish_led_locked();
    ReleaseSRWLockExclusive(&led_lock);
}
