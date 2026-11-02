#ifndef CHUNI_BRIDGE_INPUT_STATE_H
#define CHUNI_BRIDGE_INPUT_STATE_H

#include <stdint.h>

#define BRIDGE_STALE_MS 300u
#define BRIDGE_REARM_MS 100u

typedef struct {
    uint32_t session, heartbeat, online, source, aux, pressure[8];
} BridgeInput;

typedef struct {
    uint32_t observed, confirmed, session, heartbeat;
    uint64_t last_change, last_sample;
} BridgeLiveness;

/* Each sampling thread owns its state. No locks or OS calls. A long sampling
 * pause must re-arm rather than mistake a heartbeat that advanced before a
 * dead producer stopped for fresh input. All time arguments are reader-local. */
static inline int bridge_input_live(BridgeLiveness *state, const BridgeInput *input,
                                    uint64_t now) {
    if (!state->observed || input->session != state->session
            || now - state->last_sample >= BRIDGE_REARM_MS) {
        state->observed = 1;
        state->confirmed = 0;
        state->last_change = now;
    } else if (input->heartbeat != state->heartbeat) {
        state->confirmed = 1;
        state->last_change = now;
    }
    state->session = input->session;
    state->heartbeat = input->heartbeat;
    state->last_sample = now;
    return state->confirmed && input->online == 1
        && (input->source == 1 || input->source == 2)
        && now - state->last_change < BRIDGE_STALE_MS;
}

#endif
