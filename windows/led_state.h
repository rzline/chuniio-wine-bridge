#ifndef CHUNI_BRIDGE_LED_STATE_H
#define CHUNI_BRIDGE_LED_STATE_H

#include <stdint.h>

/* Shared-memory LED region (protocol v2): 32-bit words starting at byte 128,
 * i.e. word offset 32 of the region. Mirrors src/lib.rs constants
 * LED_SEQ/LED_GEN/LED_GROUND/LED_AIR_LEFT/LED_AIR_RIGHT. The single writer is
 * the game process through the chuniio exports; the Linux bridge only reads.
 *
 * Current game trial: incoming [G, B, R] becomes device [B, R, G].
 * Reorder each triple once here as source indexes [1, 2, 0], then publish
 * device-ready bytes. Linux copies them without another color rotation.
 * Zone order and billboard compatibility colors are unchanged.
 * chuni_io_led_set_colors passes 61 billboard LEDs per board (183 bytes); the
 * upstream-tested RPunithm fork maps the air towers to billboard indexes 150
 * (left) and 180 (right). */
#define BRIDGE_LED_SEQ 0u
#define BRIDGE_LED_GEN 1u
#define BRIDGE_LED_GROUND 2u  /* 24 words = 96 bytes */
#define BRIDGE_LED_GROUND_WORDS 24u
#define BRIDGE_LED_AIR_LEFT 26u
#define BRIDGE_LED_AIR_RIGHT 27u
#define BRIDGE_LED_WORD_OFFSET 32u
#define BRIDGE_LED_GROUND_BYTES 96u
#define BRIDGE_BILLBOARD_BYTES 183u
#define BRIDGE_AIR_LEFT_INDEX 150u
#define BRIDGE_AIR_RIGHT_INDEX 180u

typedef struct {
    uint32_t gen; /* Last published generation counter. */
    uint8_t ground[BRIDGE_LED_GROUND_BYTES];
    uint8_t air_left[3], air_right[3];
} BridgeLedCache;

static inline uint32_t bridge_led_load(const uint32_t *word) {
    return __atomic_load_n(word, __ATOMIC_SEQ_CST);
}

static inline void bridge_led_store(uint32_t *word, uint32_t value) {
    __atomic_store_n(word, value, __ATOMIC_SEQ_CST);
}

/* Game GBR -> device BRG trial. Do not repeat this rotation in Linux. */
static inline void bridge_led_slider(BridgeLedCache *led, const uint8_t *gbr) {
    uint32_t i;
    for (i = 0; i < BRIDGE_LED_GROUND_BYTES; i += 3) {
        led->ground[i] = gbr[i + 1];
        led->ground[i + 1] = gbr[i + 2];
        led->ground[i + 2] = gbr[i];
    }
}

/* Extract one board's air tower color from the 183-byte billboard payload.
 * Returns 0 for unknown boards without touching the cache. */
static inline int bridge_led_billboard(BridgeLedCache *led, uint8_t board, const uint8_t *rgb) {
    uint32_t index, i;
    uint8_t *air;
    if (board == 0) {
        index = BRIDGE_AIR_LEFT_INDEX;
        air = led->air_left;
    } else if (board == 1) {
        index = BRIDGE_AIR_RIGHT_INDEX;
        air = led->air_right;
    } else {
        return 0;
    }
    for (i = 0; i < 3; ++i) air[i] = rgb[index + i];
    return 1;
}

/* Publish the cached LED state with the same seqlock framing as the input
 * region: odd seq, payload, even seq. Single writer only; callers must
 * serialize concurrent game threads (the DLL holds one SRWLOCK). `words` is
 * the absolute LED word base (&region[32]), not the region start. */
static inline void bridge_led_publish(BridgeLedCache *led, uint32_t *words) {
    uint32_t old = bridge_led_load(words + BRIDGE_LED_SEQ);
    uint32_t odd = (old & 1) ? old + 2 : old + 1; /* skip a stale odd */
    uint32_t i;
    bridge_led_store(words + BRIDGE_LED_SEQ, odd);
    led->gen += 1;
    bridge_led_store(words + BRIDGE_LED_GEN, led->gen);
    for (i = 0; i < BRIDGE_LED_GROUND_WORDS; ++i) {
        const uint8_t *p = led->ground + 4 * i;
        bridge_led_store(words + BRIDGE_LED_GROUND + i,
                         (uint32_t)p[0] | ((uint32_t)p[1] << 8)
                         | ((uint32_t)p[2] << 16) | ((uint32_t)p[3] << 24));
    }
    bridge_led_store(words + BRIDGE_LED_AIR_LEFT,
                     (uint32_t)led->air_left[0] | ((uint32_t)led->air_left[1] << 8)
                     | ((uint32_t)led->air_left[2] << 16));
    bridge_led_store(words + BRIDGE_LED_AIR_RIGHT,
                     (uint32_t)led->air_right[0] | ((uint32_t)led->air_right[1] << 8)
                     | ((uint32_t)led->air_right[2] << 16));
    bridge_led_store(words + BRIDGE_LED_SEQ, odd + 1);
}

#endif
