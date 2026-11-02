#ifndef CHUNI_BRIDGE_CHUNIIO_H
#define CHUNI_BRIDGE_CHUNIIO_H

#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <stdarg.h>
#include <windef.h>
#include <winbase.h>

typedef void (__cdecl *chuni_io_slider_callback_t)(const BYTE *pressure);

WORD __cdecl chuni_io_get_api_version(void);
HRESULT __cdecl chuni_io_jvs_init(void);
void __cdecl chuni_io_jvs_poll(BYTE *opbtn, BYTE *beams);
void __cdecl chuni_io_jvs_read_coin_counter(WORD *total);
HRESULT __cdecl chuni_io_slider_init(void);
void __cdecl chuni_io_slider_start(chuni_io_slider_callback_t callback);
void __cdecl chuni_io_slider_stop(void);
/* Trial: 32 game GBR triples -> device BRG triples; zone order unchanged. */
void __cdecl chuni_io_slider_set_leds(const BYTE *gbr);
HRESULT __cdecl chuni_io_led_init(void);
void __cdecl chuni_io_led_set_colors(BYTE board, const BYTE *rgb);

#endif
